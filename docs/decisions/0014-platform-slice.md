# ADR 0014: Platform slice — object storage, tokens, secrets, backups and fairness

Date: 2026-11-15.

Status: **decision accepted for M4.2; the object-store half is implemented in
increment 7A and its detailed design is recorded below.** The token, secret,
fairness and export halves remain open decisions.

## Context

Four later capabilities need foundations M4 did not build:

- The identity slice stores authenticator seeds, OIDC client secrets and
  recovery material.
- M6 stores CI logs, artifacts, caches and secrets, and issues credentials to
  jobs.
- Any installation that hosts more than one organization needs fairness
  between them, and a public host needs limits that anonymous traffic cannot
  escape.
- M4 ended with a backup that cannot run while the server accepts writes, which
  the M4 sign-off records as a known limitation.

Building these once, before the features that consume them, is cheaper than
retrofitting four separate secret stores, token models and limit mechanisms.
The alternatives were to build each inside its consuming milestone (duplicated
secrets and tokens, three incompatible limit schemes), or to defer them to M11
(M6 cannot start without them).

## Decision

### 1. One object-store interface, two backends

- One trait with S3-shaped operations: put, ranged get, delete, checksum,
  prefix listing, multipart or resumable upload, and append for a stream that
  is still being written (CI logs). Reads are byte ranges, not whole objects.
- Keys are prefixed by tenant (`org/<organization_id>/...`) and by purpose, so
  a quota, a delete and a backup can act on a prefix without scanning.
- The filesystem backend is the single-node implementation and is built in
  M4.2. Each object is a file written to a temporary name, fsynced, then
  renamed; a checksum is stored beside it and verified on read and on backup
  verification.
- The S3-compatible distributed backend is chosen in M5, not here, because a
  cluster installation needs it for CI data and M5 owns the distributed
  storage decision. M7 builds LFS, packages and releases on the same interface
  and adds no backend.
- Abandoned multipart uploads expire after a configured deadline and are
  removed by the reconciler's durable pass. Quota accounting is per
  organization (see "Quota: the organization owns the bytes"): it counts
  committed object bytes plus open-upload part bytes and releases them when an
  object is deleted or an upload is completed, aborted or swept. Repository
  size stays on `repository_quota_bytes`, which counts a repository's own
  `objects/` tree; the two ledgers do not overlap.
- `backup` includes the object store: a manifest of every object's key,
  checksum and size, and the bytes themselves, either in the archive or by a
  snapshot the restore can verify.

### 2. Token subjects and principals

- Three subjects: a **user token** (a person), a **service account** (an
  organization-owned non-human identity), and a **run token** (issued to one
  CI run, expiring with it).
- A service account and a run token are principal types in the authorization
  model, not rows on an account. `access_tokens.account_id` is `NOT NULL` and
  `audit_events.actor_id` references `accounts`, so both the token table and
  the audit table gain a principal kind: `subject_kind` in
  `('account','service_account','run')` and an `actor_kind` on audit rows with
  a nullable account. A run token's actor resolves to the run and its
  triggering account.
- A token carries a **resource set** (a list of repositories, packages or
  other resources), scopes over that set, an audience (API, Git HTTP, registry)
  and a short expiry. Resource sets exist because a normal job clones more than
  one repository and a registry credential is a different resource type.
- Issuance is an authenticated API call that returns the token once; the
  server stores only a hash. Delivery to a job is a file whose path is in an
  environment variable, plus a masked variable for tools that need it inline.
- Revocation is a row state checked on every admission. A run token is
  invalid the moment the run ends, including on cancellation. Runs arrive in
  M6, so M4.2 tests this subject against a stub run record.
- The existing two-scope `repo:read`/`repo:write` tokens keep working under a
  deprecation window; the end date is a configuration value and the `doctor`
  command reports how many remain.
- "A token cannot act outside its scope" is tested on the API and Git HTTP for
  every subject. SSH is not a token transport; a service account authenticates
  over SSH with its own key, tested the same way.

### 3. Secrets at rest

- Envelope encryption: each stored secret has a data key; the data key is
  wrapped by an installation key encryption key (KEK). The ciphertext and the
  wrapped data key are stored together.
- The KEK is operator-supplied and never stored in the database or in a
  backup: a file with mode `0600`, a KMS reference, or a passphrase-derived
  key. The backup manifest records only a key id, the algorithm and the wrap
  method.
- The wrapped set includes every secret column: SMTP credentials, TOTP seeds,
  OIDC client secrets, CI secrets, deployment credentials, recovery material,
  per-account and per-organization data keys, and `audit_ip_keys`. The audit IP
  key is a plaintext 32-byte secret in PostgreSQL today and every stored IP
  digest is an HMAC under it, so a dump is brute-forceable for IPv4 while it
  exists.
- Secrets are decrypted at the point of use and never returned by an API.
  Environment variables remain an operator option for bootstrap values.
- Rotation introduces a new KEK, re-wraps every data key in a bounded batch
  job, records progress, and keeps the old KEK for decrypt-only until the
  re-wrap completes. A failed batch leaves each row readable by one of the two
  keys.
- Deleting a per-account or per-organization data key destroys every secret and
  encrypted column in older backups (crypto-shredding); it does not make a
  plaintext profile column unreadable, and a backup predating the keys cannot
  be shredded at all. The identity slice's disposal promise is scoped to
  secrets and encrypted columns, with retention the control for the rest.

### 4. Backups: encrypted, fenced, and online on one node

- Every archive is encrypted with an operator key or passphrase; the manifest
  carries a key id, never the key. While encrypted, the archive contains no
  plaintext secret. It includes the configuration and the SSH host key because
  restore needs both, so after decryption the only private material present is
  the database and SMTP values in that file and the SSH host key, all
  operator-owned; every stored secret column is encrypted under the installation
  key, which is not in the archive. The decryption claim is therefore about
  stored secrets, not about the operator's own material.
- The maintenance-window path has a mechanism, not a sentence. `backup`
  proceeds only when it can prove no writer can acknowledge a commit:
  - the server is not running (its bind address refuses connections), or
  - the server is in maintenance mode: a control command sets a durable flag,
    the server rejects new writes with `503`, waits for in-flight requests to
    finish and reports drained.
  It records which check it used, and refuses otherwise.
- **Single-node online backup** uses a filesystem snapshot or copy-on-write
  clone of the storage root. It takes a metadata transaction at a recorded
  PostgreSQL position, records one journal position per repository, starts the
  snapshot, and verifies that every repository's client-visible refs at the
  recorded position are present in the snapshot. A write that starts before
  the snapshot and commits during it is either in the restore or was refused
  before acknowledgement. Filesystems without snapshots keep the
  maintenance-window path; the command reports which mode it used.
- Restore verifies the manifest, the key id, the database-to-storage
  consistency, and every repository's journal position. A wrong key or a
  missing key fails before the server is started, with a message naming the
  key id.
- The cluster backup is M5's; this ADR fixes only the single-node form it must
  extend.

### 5. Fairness and limits

Limits are configuration with published safe defaults. The shape:

- **Authenticated request rate:** per organization and per account, separate
  budgets for the API and for Git operations.
- **Anonymous traffic:** per client address and globally, because public clone,
  login and search are unauthenticated. `trusted_proxies` must be configured
  for the address to be the client's; the documentation says so and `doctor`
  warns when limits are on and no proxy is trusted.
- **Git concurrency:** a share of the global slots per organization, with a
  floor of one slot and no organization able to take every slot. A request over
  its share waits or is refused with `Retry-After`; it is never silently
  starved.
- **Storage:** per-organization quota on top of the per-repository one,
  counting objects, merge-created objects, fork copies and stored large
  objects.
- **Organization creation:** a cap per account and per installation, because a
  new organization otherwise earns a fresh share.
- **Background work:** per-organization shares of the job worker pool and of
  the email send rate, so one tenant's backlog cannot occupy every worker.
- **The bound:** with the configured shares, an organization at its share sees
  a p95 within twice its measured single-organization p95 at the same total
  load. The numbers are defaults to be tuned by measurement; the test asserts
  behaviour against the configured values, not an absolute latency.

Default values, recorded so the exit can be tested and later revised:

| Limit | Default |
| --- | --- |
| Global Git concurrency | 8 slots |
| Per-organization Git share | floor 1, ceiling 6, fair share of the rest |
| API requests | 300/min per organization, 60/min per account |
| Anonymous clone/fetch | 120/min per address, 2,000/min installation |
| Anonymous login/search | 20/min per address |
| Organization creation | 3 per account per 24 h, 50 per installation per day |
| Repository size | 5 GiB per repository, 50 GiB per organization |
| Job worker share | 2 of 4 workers per organization |
| Email | 200/h per organization, 5/h per address for security mail |

### 6. Export contract

M4.2 defines the export format the later milestones must round-trip: versioned
JSON Lines for metadata, native Git for history, and an object manifest with
checksums for large objects. It ships a harness that exports, imports into a
fresh installation and compares counts, checksums and sampled content. M12
builds the full product export, importers and the CLI; M7, M8 and M10 add a
round-trip test for the data they introduce. The contract exists here because
M4.2 already owns the objects and the metadata that an export must carry.

### 7. Outbound request client

Every request the server itself makes goes through one client: an egress
allowlist, no redirect to a different host, no link-local, loopback,
private-range or cloud-metadata address unless the operator allows it, the
resolved address pinned for the connection, bounded size and time, and a
recorded reason. It is built here because the abuse baseline needs it and its
first users follow immediately — OIDC discovery (M11a), external action
sources (M6), webhooks, mirrors and importers (M12). No later feature opens
its own outbound connection.

## Alternatives considered

- **Separate secret stores and token models per milestone.** Rejected: three
  incompatible envelopes, three audit stories, and M6 blocked on M11.
- **A vendor object store as the only backend.** Rejected: self-hosting must
  not require a vendor service; the interface allows an operator's S3 store in
  a cluster without making it mandatory.
- **Tokens as account-only rows with a synthetic service account per
  organization.** Rejected: it makes every run look like a person in the audit
  trail and gives a run the service account's whole grant set.
- **Backups unencrypted, with access control on the backup host.** Rejected:
  the archive contains the database and SMTP credentials, and off-host copies
  leave the installation's control.
- **A single global rate limit.** Rejected: one organization can then deny
  service to the rest, which defeats the hosting goal.
- **Fairness measured as configured rather than as observed.** Rejected: a
  configuration that is never exercised is not evidence.

## Consequences

- Four subsystems move earlier than their consumers, so M4.2 is a foundation
  milestone with no user-visible feature. It is still the cheapest order.
- The token and audit schemas change in a way that touches authorization and
  every audit writer; this is a larger change than a token table.
- Backup gains two modes (maintenance window and snapshot) and an encryption
  key the operator must keep; losing that key loses the secrets, by design.
- Fairness adds a scheduler in front of Git and jobs, with configuration that
  must be tuned. A wrong default is visible as latency, not corruption.

## Open items

1. **Resolved in M4.2 increment 7A** — object backend layout, the multipart
   format, the append format, quota accounting, crash consistency, torn tails
   and the size bounds, below.
2. Token resource-set grammar, the issuance API shape and the exact legacy
   deprecation date.
3. KMS adapter interface and the passphrase-derived key parameters.
4. Re-wrap batch size and the operator signal that rotation is complete.
5. **Resolved in M4.2 increment 7A** — object-verification depth for backup
   and restore, below. The filesystem snapshot mechanism itself remains the
   operator's choice; the single-node path uses the proved write fence and a
   whole-root tar, so no filesystem-specific snapshot API is required here.
6. The measured fairness bound on the reference machine, and the defaults
   above revised from it.
7. **Partly resolved in M4.2 increment 7A** — export format versioning and
   comparison tolerances, and the object manifest, below. The metadata JSON
   Lines schema and the export/import harness remain open: they are the rest
   of increment 7A, not done by the object half.

## Resolutions recorded by M4.2 increment 7A

### Open item 1: key-addressed filesystem backend, multipart and append

**Crate.** The interface and its filesystem backend live in `githearth-git`,
module `object_store`. That crate already owns durable filesystem storage
(`layout`, the per-repository lock, the push journal), has no database
dependency, and is the type the server's `AppState` holds. M5 can add an
S3-compatible implementation behind the same `ObjectStore` trait without
moving a type name or making the server depend on a vendor SDK.

**Key-addressed, not content-addressed.** The caller chooses the key; a put
overwrites, a delete removes, prefix listing walks a key namespace, and a
tenant prefix is a path prefix. Content addressing was rejected because it
cannot express overwrite, delete-by-name, append to a live log, or the
caller-named artifacts and LFS keys M6 and M7 need, and because chunk
deduplication is not an M4.2 goal. Integrity comes from a checksum recorded
beside the bytes, not from the key.

**Keys.** A key is relative to its tenant: ASCII letters, digits, `-`, `_`,
`.`, `+`, `=`, in non-empty `/`-separated segments, no `.` or `..`, at most
1024 bytes. Every operation takes a `TenantId`; the backend writes under
`data/<organization uuid>/` and never accepts an absolute key, so one tenant
cannot address another's prefix.

**Layout** under the storage root (`<storage_root>/objects`):

```text
data/<tenant>/<key...>                  payload bytes, fsynced then renamed
meta/<tenant>/<key...>.json             checksum, length, chunk table, appendable
uploads/<tenant>/<key...>/<upload id>/  upload.json + part-NNNNN + part-NNNNN.json
usage/<tenant>.json                     durable byte counter (derived cache)
tmp/                                    atomic-write scratch
locks/<sha256(tenant/key)>              per-key flock
locks/gate-<tenant>                     shared by writers, exclusive for reconciliation
locks/quota-<tenant>                    serialises the counter read-modify-write
```

Each write goes to a private file under `tmp/` (or beside its target), is
fsynced, renamed into place, and its parent directory is fsynced. The metadata
sidecar records a whole-object SHA-256 and a SHA-256 per 1 MiB
chunk. Metadata is written by the same atomic procedure. A read whose length
disagrees with the sidecar is refused as `Corrupt`; a read whose bytes do not
match the checksum is refused as `ChecksumMismatch`; corrupt bytes are never
returned. A ranged read verifies only the chunks it touches, which is why a
range can cross a chunk boundary without reading the rest of the object.

`usage/` is a derived cache over `data/` and `uploads/`. It is excluded from
the backup tar, rebuilt lazily when absent, and never treated as an object.
`tmp/`, `locks/` and `uploads/` are also excluded: the first two are scratch,
and an open upload is not part of a published object set.

**Multipart format.** `create_multipart` writes an `upload.json` manifest
(upload id, tenant, key, created and last-updated second). A part is
`part-NNNNN` plus `part-NNNNN.json` (number, length, SHA-256); re-uploading a
part replaces it atomically, so a resumed upload lists what is on disk and
never duplicates a part. `complete_multipart` accepts an explicit strictly
increasing part set or every part present, verifies each part's checksum,
concatenates once, writes and fsyncs the object, records the whole-object and
chunk checksums, and removes the upload directory. Completion copies each part
through a 1 MiB buffer while hashing it, so no part is ever allocated whole.
Completing an upload twice is refused because the upload no longer exists.

**Append format.** An appendable object is a raw byte stream with a committed
length in its sidecar. `append` takes the per-key lock, reads the sidecar, and
first re-hashes the committed prefix and compares it with the recorded
SHA-256. If the committed bytes are corrupt the append is refused
(`ChecksumMismatch`) and neither the payload nor the sidecar changes: an append
can only extend a verified base, never launder corruption into a new checksum.
Only then does it charge the organization quota for the incoming bytes,
truncate any bytes past the committed length (an unacknowledged torn tail),
append, fsync, recompute the whole and chunk checksums, and rewrite the sidecar
atomically. A crash before the sidecar rename therefore loses an unacknowledged
tail and a read never returns a partial line set. `put` creates an immutable
object; `create_appendable` creates the log form; turning an immutable object
into a log and replacing a live log with an immutable `put` are both refused.
The cost is honest: every append reads the committed prefix to verify it and
the whole file again to record the new chunk table, so appending `n` times in
one-byte steps is quadratic in bytes. M6 must batch log lines into bounded
appends; a later increment may fuse the verify and digest passes.

**Abandoned uploads.** The deadline is
`object_upload_deadline_seconds`, default **86,400 s (24 h)**, bounded
60 s–30 days. A part upload refreshes the upload's `updated_unix_seconds`, so
an active resume is not swept. The reconciler's durable pass calls
`cleanup_abandoned` on every run (and at start), so a restart finishes the
cleanup; an unreadable manifest falls back to the directory mtime.

### Quota: the organization owns the bytes

An object is charged to the **organization** that owns its tenant prefix, not
to a repository. Tenant scoping already makes the organization the owner; M6
logs and artifacts belong to a run and its organization rather than to one
repository; charging both would double-count and require an object-to-
repository mapping no consumer needs. Repository bytes continue to be counted
by the existing `repository_quota_bytes` path over a repository's `objects/`,
which the object store is not inside, so the two ledgers do not overlap.

`organization_quota_bytes` follows the same convention as
`repository_quota_bytes`: `0` disables it. Usage is committed object bytes
plus the bytes of open upload parts (an abandoned upload consumes quota until
it is completed or swept); completing an upload moves its part bytes into the
object's charge rather than adding them twice, and deleting the object,
aborting an upload or sweeping it releases the bytes.

The check is a **durable per-tenant byte counter**, not a walk of the tenant
on every write. `usage/<tenant>.json` holds the total (committed objects plus
open-upload parts). A write takes the tenant's shared gate, the key lock, and
then the tenant's quota lock; under the quota lock it reads the counter,
subtracts the existing bytes it will replace, adds the incoming bytes, refuses
with `QuotaExceeded` if the result exceeds the limit, and writes the new
counter before any payload byte is written. `put`, `append`, `upload_part` and
`complete_multipart` all reserve this way; a failed write releases the
reservation. The per-tenant quota lock is what makes the read-decide-write
atomic between writers, so four concurrent writes cannot each see the same
free space.

The counter is a cache with an exact source of truth. The reconciler calls
`reconcile_usage` once per pass, taking each tenant's gate exclusively so no
write is in flight, scanning `data/` and `uploads/`, and rewriting the
counter; `open_with` does the same at startup before any writer exists. A
crash between the payload and the counter therefore cannot leave an
under-count across a restart, and a stale counter can only over-charge in the
window before the next pass. The cost is one scan per tenant per reconcile
pass (default interval is the reconciler's) instead of one scan per write:
writes are O(1) in the number of objects, while the sweep remains O(all
objects). `usage()` itself still scans, because it reports exact current
figures; no write path calls it. A corrupt or missing counter is discarded and
rebuilt rather than trusted. `append` is charged like a put with the existing
bytes credited as zero, so a log cannot outgrow the organization quota one
line at a time. The cost per charged write is one small extra atomic write and
fsync for the counter, in addition to the payload and sidecar; the quota lock
is held only for the counter read-modify-write, not for the payload write.

The gate lock is held shared for the duration of a write and exclusively by
reconciliation. That is what stops a scan from overwriting a reservation made
by a write that has not finished. It does not serialise writes against each
other; only the brief counter update does.

### Open item 5: verification depth on backup and restore

The backup manifest now records every object's tenant, key, SHA-256 and size.
`backup` samples that inventory before and after the storage tar and refuses
to publish if it changed. The archive this command wrote is read back and its
object store verifies **every object and every chunk checksum**, and its
inventory must equal the manifest exactly; `restore` runs the same check
against the staged tree before publishing it. The depth is complete rather
than sampled because the checksums are already recorded at write and cost one
sequential read. Repositories keep the existing depth (journal position, then
`git fsck` and per-repository journal verification); a large repository is
bounded by the same command that already verifies it today.

Verification distinguishes **corruption** from **crash residue**. A payload
that disagrees with its sidecar is `Corrupt` and fails backup and restore. A
payload with no usable sidecar, or a sidecar with no payload, is an *orphan*:
it is reported by `verify_all` for a future repair command, but backup
tolerates it, because the tar carries the bytes and sidecars for repair and the
manifest lists only objects that can be verified. Listing and inventory skip
orphans instead of returning a fatal `NotFound` for the whole tenant. `usage`
counts only a readable sidecar whose payload exists, so a phantom is not
charged and a corrupt sidecar cannot refuse an entire tenant's writes. A torn
tail on a live log is not a problem at all (see "Torn tails"): reads and
verification stop at the committed length, and the next append truncates the
tail.

### Crash consistency between the payload and the sidecar

An object is two files, and a crash can land between them. The windows are
made survivable rather than eliminated:

- **Put** writes the payload, then the sidecar. A crash in between leaves a
  payload with no sidecar (an orphan payload). `list` and `inventory` skip it,
  `verify_all` reports it as an orphan, backup carries the bytes and does not
  fail, and a later `put` for the same key adopts the orphan by overwriting
  both files.
- **Delete** removes the sidecar first, then the payload. A crash in between
  leaves an orphan payload, not a phantom object that `stat` reports and `get`
  refuses. `usage` counts only a sidecar whose payload exists, so a phantom is
  not charged.
- A sidecar that exists with no payload is reported as an orphan too. It is
  left for the repair path rather than silently deleted, but it does not fail
  a backup.
- **Append** writes the payload, fsyncs, then rewrites the sidecar. A crash
  in between leaves more bytes than the sidecar commits; that is a torn tail,
  below.

The rule is: a payload with no sidecar was never acknowledged, so it is not
data; a sidecar with no payload is bookkeeping residue; neither is allowed to
take down a tenant's listing or its backup. Corruption of a *committed* object
is different and is always refused.

### Torn tails on a live log

A torn tail is **readable up to the last committed record**, not refused. The
sidecar's committed length is the log's contract: bytes past it were never
acknowledged to the writer, so a read returns and verifies exactly
`min(file length, committed length)`, ranged reads are bounded by the
committed length, and `verify`/`verify_all` treat extra bytes as uncommitted,
not as damage. Refusing instead would make one crash between the payload
fsync and the sidecar rename take a live CI log offline until someone
truncated it, and would make the backup refuse a log whose committed prefix is
perfectly good. The alternative — treating the tail as part of the log — would
return a partial line set, which the format exists to prevent. The next
`append` takes the key lock and truncates the tail before writing, so the
state heals on the next write; a backup carries the tail bytes in the tar but
manifests and verifies only the committed prefix. The tail is therefore never
data, never a verification failure, and never a reason to block a backup.

### Streaming and size bounds

`put` and `get` take and return a whole `Vec<u8>` for the filesystem backend;
the interface is not streaming in 7A. The bounds are explicit rather than
implicit:

- `max_object_bytes` and `max_part_bytes` are options, both defaulting to
  **5 GiB** (S3's part ceiling; it also matches the 5 GiB per-repository size
  default). `put`, `append` and `complete_multipart` refuse an object over the
  former with `TooLarge`; `upload_part` refuses a part over the latter.
- **Completion never allocates a part.** It copies each part through a 1 MiB
  buffer, hashing as it copies, so the peak allocation is one chunk regardless
  of part size. This was the unbounded allocation a hostile tenant could use
  to exhaust the server.
- A whole-object `get` still allocates the object, bounded by
  `max_object_bytes` (5 GiB). That is a documented bound, not an accident:
  M6 must serve CI logs with ranged reads and must not `get` a multi-gigabyte
  artifact into memory; a streaming read is owed before any consumer needs
  one. A CI log grows by bounded append slices supplied by M6 (line batches,
  not one syscall per byte); a large artifact is uploaded as parts and
  completed by the streaming path.
- `list` still walks the tenant and filters by prefix; prefix-native listing
  and a paginated `inventory` are owed by M5 alongside its S3 backend.

### Open item 7: export contract versioning and tolerances



The export contract is versioned JSON Lines for metadata, native Git for
history, and an object manifest for large objects. The object manifest is the
backup inventory above, versioned by the archive's `format_version` (**2**;
version 1 archives are still read, but only 2 is written): one record per
object carrying tenant, key, SHA-256 and size. Version 2 exists because a
reader of version 1 would accept a manifest carrying objects and silently skip
object verification; the bump makes that impossible. Comparison tolerances are
exact, not approximate: object counts must be equal, every object checksum
must be equal, and sampled content is compared byte for byte.
A missing object, an extra object, or a checksum that differs is a failed
round trip, never a warning. Importing into a fresh installation uses the
same `ObjectStore` operations, so a backend change in M5 cannot change the
contract. The metadata JSON Lines schema and the export/import harness are not
defined here yet; they are the remaining half of increment 7A, so open item 7
is only partly closed.
