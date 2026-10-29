# ADR 0003: Single-node push transaction and repository lifecycle

Date: 2026-10-27.

Status: **accepted for M2**. Defines the acknowledgement boundary for `git push` on one node, the repository journal, and how repository metadata in PostgreSQL and repository storage on disk converge after a crash. Replicated pushes (M5) extend the journal; they do not replace it.

## Constraints

From the [metadata contract](../plan/01-metadata-contract.md) and [deployment contract](../plan/03-deployment-contract.md):

- Git objects, authoritative refs and the repository journal live in Git storage, outside the metadata transaction. There is no atomic commit across PostgreSQL and Git; an intent, an operation ID, the Git result and a reconciler stand in for one.
- A push is acknowledged only after its objects and ref updates are durable on the host. Objects are validated and placed before any ref that names them is published.
- Metadata may lag the committed Git result. The API and UI distinguish *in progress / unknown* from *failed*.

## Storage layout

```
<storage_root>/
  repositories/<id[0..2]>/<id>.git    one bare repository per immutable repository UUID
  tmp/<operation-id>/                 repositories under construction
  trash/<id>.<unix-seconds>.git       deleted repositories inside the retention window
  orphans/<id>.<unix-seconds>.git     directories found with no metadata row; never deleted automatically
```

Paths derive only from the repository UUID, never from names, so rename and transfer are metadata changes. The storage layer's containment checks (no symlinks, canonical path inside the root) apply to every path above. The storage root is owned by the service user, mode `0700`.

Every repository is created with:

| Setting | Value | Reason |
| --- | --- | --- |
| `core.fsync` | `objects,derived-metadata,reference` | Objects, packs, indexes and refs reach disk before Git reports success |
| `core.fsyncMethod` | `fsync` | No batch mode until measured |
| `core.hooksPath` | GitHearth's hook directory | Only GitHearth hooks run; repository-local hooks are ignored |
| `receive.fsckObjects`, `transfer.fsckObjects` | `true` | Malformed objects are refused in quarantine |
| `receive.denyNonFastForwards`, `receive.denyDeletes` | `false` | Policy decides per ref, not Git config |
| `receive.maxInputSize` | configured pack limit | Bounded uploads |
| `gc.auto` | `0` | Maintenance is scheduled, never inline in a push |

Git runs with the sanitized environment (`env_clear`, no system or global config, fixed `PATH`, `LC_ALL=C`). Object format is detected from the repository (`extensions.objectFormat`); new repositories default to SHA-1 and may be created as SHA-256. The supported Git range is 2.36 or newer, checked at startup.

## Push transaction

Smart HTTP `git-receive-pack` runs native `git receive-pack --stateless-rpc` with these stages. Each stage either completes or leaves a state the reconciler can classify.

1. **Admission.** Authenticate (password or token per request; anonymous push is never admitted). Authorize `repository.read` on the repository so its existence is not revealed to others. Take a slot from the Git process budget, which is separate from the JSON request budget. Allocate an operation ID and pass it to the hooks with a one-use hook token over a local Unix socket.
2. **Quarantine.** Git writes the received pack to its quarantine directory and runs `fsck` on it. Nothing is visible in the repository yet.
3. **Policy (`pre-receive`).** The hook sends every `<old> <new> <ref>` line to the server, which decides each one through the [ADR 0002](0002-authorization.md) engine: `ref.push`, `ref.force_push` when `old` is not an ancestor of `new` (checked against the quarantined objects), `ref.delete` when `new` is zero, plus branch protection. One denied ref rejects the whole push. Only `refs/heads/*` and `refs/tags/*` are writable; any other namespace is refused. On acceptance Git migrates the quarantined objects into the repository and fsyncs them.
4. **Journal (`reference-transaction`, state `prepared`).** Before Git commits any ref, the hook appends a *prepared* record to `<repo>/githearth/journal`: operation ID, actor, and every `<old> <new> <ref>` in the transaction. The append is fsynced. If the append fails, the hook exits non-zero and Git aborts the transaction.
5. **Ref commit.** Git writes the refs (fsynced by `core.fsync=reference`) and runs the hook in state `committed` or `aborted`, which appends the matching marker. Marker appends are best-effort: the reconciler can recover a missing marker from the refs themselves.
6. **Acknowledgement.** `receive-pack` reports `ok`/`ng` per ref to the client. An `ok` therefore means objects, journal record and ref are all on disk.
7. **Metadata (`post-receive` and after).** The server records the push in PostgreSQL in one transaction: the push operation row, ref update rows, and the outbox event, keyed by `(repository_id, journal_sequence)` so applying the same journal record twice changes nothing. A failure here never turns an acknowledged push into a failed one; the reconciler applies it later.

Git applies the ref updates of a non-atomic push in one transaction per ref. Each transaction gets its own journal record, so a partially applied push is visible exactly as Git left it. Clients that send `--atomic` get one transaction for all refs.

### Journal format

Append-only file of records: `u32 length | u32 CRC-32C | payload`, payload JSON. Each record has a per-repository `sequence` that strictly increases. A torn final record (short or bad checksum) is truncated on open; any other bad record stops the repository from accepting writes and raises an alert. Rotation keeps the file below a configured size by moving applied records into `journal.<first-sequence>`; metadata application uses the sequence, never file offsets.

### Recovery

At startup, and periodically, the reconciler visits each repository with records newer than the metadata high-water mark:

| Journal state | Refs on disk | Classification |
| --- | --- | --- |
| prepared + committed | — | committed: apply to metadata |
| prepared + aborted | — | aborted: record and skip |
| prepared only | all at `new` | committed (marker lost): append marker, apply |
| prepared only | all at `old` | aborted (crash before commit): append marker |
| prepared only | mixed | diverged: refuse writes to the repository and alert; an operator repairs it |

A ref changed on disk with no journal record (manual edit, restored older snapshot) is also *diverged*. GitHearth never invents history to cover it.

## Repository lifecycle

Repository rows in PostgreSQL hold identity, owner, name, visibility, archive state and a `status`: `creating`, `active`, `deleting`, `deleted`, `failed`. Names are unique per organization among rows that are not `deleted` (partial unique index), so a deleted name is reusable.

- **Create.** (1) Insert the row as `creating` with its operation ID and commit. (2) `git init --bare` into `tmp/<operation>`, write the config above, fsync the tree, `rename` into `repositories/`, fsync the parent directory. (3) Mark the row `active`. The API answers `201` only after step 3; a lost response is answered by replaying the operation ID.
- **Rename / transfer.** Metadata only. The previous `owner/name` is stored as a redirect for Git and web URLs until the name is reused.
- **Archive / unarchive.** Metadata only; ADR 0002's archive forbids stop writes, including pushes, at the next decision.
- **Delete.** Mark `deleting`, then rename the directory into `trash/`, then mark `deleted`. Trash is purged after the retention window (default 7 days). There is no automatic restore: a `deleted` row never transitions, so within the window an operator moves the directory back by hand and creates a new repository row.

The lifecycle reconciler handles each intermediate state:

| Row status | On disk | Action |
| --- | --- | --- |
| `creating` | final path valid | mark `active` |
| `creating` | missing or only in `tmp/` | redo the create from step 2; after repeated failures mark `failed` (a `failed` row can be retried to `creating` or deleted, so its name is never stuck) |
| `deleting` | still in `repositories/` | move to trash, mark `deleted` |
| `deleting` | already in trash | mark `deleted` |
| no row | directory in `repositories/` | move to `orphans/` and alert |

`tmp/` entries with no live `creating` row are removed.

## Idempotent mutations

JSON mutations accept an `Idempotency-Key` header (1–128 visible ASCII characters). The `operations` table stores `(actor_id, key)` unique, a SHA-256 of the method, path and body, the status and the response. A retry with the same key and request returns the stored response; the same key with a different request is `422`; a retry while the first attempt is still running is `409`. Keys expire after 24 hours. Creation endpoints also record the operation ID on the created object, which is how create reconciliation finds the right row.

## Backup and restore

A consistent backup is a PostgreSQL backup plus a storage snapshot taken *after* it. On restore the journal and refs are authoritative for Git state and PostgreSQL for which repositories exist. The reconcilers above bring the two into agreement. Pushes newer than the database backup are reapplied from the journal; repositories created after the database backup become orphans and are kept for the operator.

## Consequences

- Push durability relies on Git's own fsync and quarantine behavior plus one fsynced journal append per ref transaction. The journal is the unit that M5 replicates: a replicated group commits the prepared record before the hook allows the ref commit.
- Every ref change made outside `receive-pack` (merges in M3, web edits, API ref updates) must go through the same `reference-transaction` path with an operation ID, so it is journaled and authorized identically.

## Open items

- Personal access tokens and their scopes, and whether a token counts as fresh authentication for the `sensitive` actions `ref.force_push` and `ref.delete`. Until decided, only password-authenticated requests count as fresh.
- SSH transport (before M4) reuses stages 2–7 with a different admission step.
- Measure the per-push cost of the journal fsync at the enterprise profile before M4.
- M2 decision: a `write` grant allows `ref.force_push` and `ref.delete` over Basic auth (fresh per request); branch protection (M3) is where this is tightened.
