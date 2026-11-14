# ADR 0014: Platform slice — object storage, tokens, secrets, backups and fairness

Date: 2026-11-15.

Status: **decision accepted for M4.2; detailed design open.** This records the
shape of M4.2. The open items at the end are implementation choices, not
missing decisions. Nothing here is implemented.

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
  removed by a durable job. Quota accounting is per organization and per
  repository, counts stored bytes plus committed part bytes, and is released
  when an object is deleted.
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
  invalid the moment the run ends, including on cancellation.
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

1. Object backend layout, the multipart format and the append format for a
   log that is still being written.
2. Token resource-set grammar, the issuance API shape and the exact legacy
   deprecation date.
3. KMS adapter interface and the passphrase-derived key parameters.
4. Re-wrap batch size and the operator signal that rotation is complete.
5. Snapshot mechanism per filesystem, and the verification depth when a
   repository is large.
6. The measured fairness bound on the reference machine, and the defaults
   above revised from it.
7. Export format versioning and the comparison tolerances.
