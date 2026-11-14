# Repository housekeeping

Scheduled maintenance repacks each repository and removes unreachable objects
once they are old enough. It is the only process that frees space: `gc.auto` is
`0`, so a push never runs Git's own maintenance, and deleting a branch does not
free space until housekeeping runs. Design and safety rules are in
[ADR 0013](../decisions/0013-repository-housekeeping.md).

## What a run does

For one repository, under its per-repository lock:

1. Replays the push journal into the database (a journal that cannot be
   reconciled defers the run instead of deleting anything).
2. Keeps every merge candidate whose intent has not reached its ref update,
   refreshes the head pin of every open pull request whose branch lives here
   (and, for a fork request, the target-side copy it already holds), and keeps
   the base pin of every open request whose target lives here.
3. Repacks reachable history with `git repack -A -d`. `-A` loosens an
   unreachable object that lives only in a pack into a fresh loose file, so its
   age can be measured per object. (`-a -d` would delete those objects
   immediately, so it must never be used.)
4. Deletes loose objects that no ref, pinned candidate or pull-request pin
   reaches and that are at least the grace period old. The scan and the
   deletion share one timeout, created after the snapshot; if it expires the
   run stops cleanly, reports `truncated` and leaves the work for the next run.
   A failed `unlink` fails the run instead of being counted as a prune.
5. Verifies the result with `git fsck --full` and a walk of every ref.

The live set is computed from `git rev-list --objects --all` plus the keep
points. If that walk produces more than `housekeeping_max_live_set_bytes`
(64 MiB by default), the run fails before the scan or any deletion: a partial
live set is never used to decide what to delete, so a merge candidate or a
pinned pull-request head cannot be removed by a truncated answer. Nothing is
deleted in that run, and the job retries (which repeats the failure until the
cap is raised). The failure is closed, never a partial prune: the run records
`failed` with the cap in the message and the next run tries again.

**Sizing the cap.** The walk prints one line per live object. Measured on this
project's own repository, that is about **68.6 bytes of walk output per
object**, so the 64 MiB default covers roughly **1 million** live objects and
the 1 GiB maximum roughly **15 million**. A kernel-sized history (about 10
million objects) needs roughly 650 MiB. If `doctor` or the log reports the
live-set cap failure, raise `housekeeping_max_live_set_bytes` above
`live_objects × 70` (and no higher than 1 GiB, the maximum), or stop and read
[the ADR](../decisions/0013-repository-housekeeping.md): above about 15 million
objects this version cannot prune at all, and streaming the walk is the fix
that is not yet implemented. Until then such an installation keeps its objects
and its space, which is the safe direction.

A push or merge that arrives during a run waits for the lock and then either
completes or is refused by Git exactly as it would be without housekeeping.
Neither protocol changes. The six maintenance phases plus the open-pin refresh
are bounded by seven step timeouts plus the job budget's 60 seconds of slack.
`housekeeping_timeout_seconds` bounds each of those seven phases separately:
the open-pin refresh, the live-set walk before the snapshot, the size walk
before the snapshot, the snapshot, the live-set walk after it, the scan and
deletion pass (its deadline starts *after* the snapshot, so a slow repack
cannot starve it), and the verification (`fsck` and the final size walk share
one). The push-journal replay that precedes them is bounded per Git step by
`git_timeout_seconds`, not by the housekeeping budget; a journal too large to
replay within that is deferred rather than pruned. Lower the timeout if pushes
to very large repositories must not wait longer than a few minutes.

## Configuration

| Setting | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `housekeeping_enabled` | `GITHEARTH_HOUSEKEEPING_ENABLED` | `true` | Run scheduled housekeeping |
| `housekeeping_interval_seconds` | `GITHEARTH_HOUSEKEEPING_INTERVAL_SECONDS` | `86400` (1 day) | How often each repository is due; 60–2592000 |
| `housekeeping_grace_seconds` | `GITHEARTH_HOUSEKEEPING_GRACE_SECONDS` | `691200` (8 days) | Minimum age of an unreachable object before it may be deleted; 0–31536000 |
| `housekeeping_concurrency` | `GITHEARTH_HOUSEKEEPING_CONCURRENCY` | `2` | Runs in flight at once; 1–16 |
| `housekeeping_timeout_seconds` | `GITHEARTH_HOUSEKEEPING_TIMEOUT_SECONDS` | `600` | Bound on each of the run's seven housekeeping phases separately. The job's own budget is seven of these plus 60 s; the journal replay before them is not part of that sum |
| `housekeeping_max_live_set_bytes` | `GITHEARTH_HOUSEKEEPING_MAX_LIVE_SET_BYTES` | `67108864` (64 MiB) | Largest `rev-list --objects --all` output a run accepts; a larger walk fails before any deletion; 1048576–1073741824 |
| `housekeeping_pin_retention_days` | `GITHEARTH_HOUSEKEEPING_PIN_RETENTION_DAYS` | `90` | How long a closed or merged pull request's head and base pins, and a superseded status's pin, stay live, counted from the close, merge or supersession; 1–3650 |
| `housekeeping_fork_fetch_limit_bytes` | `GITHEARTH_HOUSEKEEPING_FORK_FETCH_LIMIT_BYTES` | `67108864` (64 MiB) | Most bytes one fork pull request's head may add to its target repository; a larger head is refused before it is copied; 1048576–1073741824 |

The grace period protects work that is real but not yet visible through a ref:
objects a push or server operation has just written before a ref reaches them,
and a branch that was deleted and pushed again a moment later. A merge candidate
is not protected by this clock: `execute_intent` records its `candidate_oid` as
an explicit keep point, and a pull request's head is an explicit pin. Keep the
grace period long enough for the longest in-flight write, and longer than the
journal replay window. The price is that a deleted branch keeps its objects for
that long.

A repository is due when it is `active`, not archived, and either has never been
maintained or its last run is older than the interval. Archived repositories are
skipped because nothing can push to them; unarchiving enqueues a run
immediately. Repositories that are `creating`, `deleting`, `deleted` or `failed`
are skipped. At most `housekeeping_concurrency` runs are queued; a repository
never has two runs at once.

`doctor` reports `housekeeping` as a warning when active repositories are more
than two intervals overdue or a run failed or was deferred. It never writes.

## Running it by hand

```sh
githearth-server --config /etc/githearth/githearth.toml housekeeping --repository <uuid>
```

**Stop the server**, or disable housekeeping (`housekeeping_enabled = false`)
**and** ensure no housekeeping job is pending or running. The per-repository
lock is an in-process mutex, not a file or database lock, so a command in a
separate process does not share it with a running server: the two could repack
and delete the same repository concurrently. A "quiet" window is not enough,
because the scheduler lives in the server and can claim a second run for the
same repository while the command runs. With the server stopped, no push or
merge can be writing while the command runs.

The command prints the sizes before and after, how many unreachable objects
were found, and whether the snapshot and the deletion pass completed. It uses
the same grace period as the schedule, so younger objects stay. Exit status is
non-zero only when the run itself failed. It also runs when
`housekeeping_enabled = false`: disabling the schedule stops automatic work,
not an explicit command.

The repository UUID is the durable identifier from `GET
/api/v1/organizations/{org}/repositories/{name}` (the `id` field) or the
`repositories` table; it never changes on rename or transfer.

## Results

Every run writes one row in `repository_housekeeping` and one audit event of
kind `repository.housekeeping` in the same transaction:

```sql
SELECT repository_id, bytes_before, bytes_after, bytes_after - bytes_before AS delta,
       expired_objects, last_run_at, last_outcome, last_error
FROM repository_housekeeping
ORDER BY last_run_at DESC;
```

`last_outcome` is `completed`, `deferred`, `failed` or `never`. `deferred` means
a live push quarantine existed, the journal could not be replayed, or the
repository names an alternate object store (`objects/info/alternates`); the
next scheduled run retries, and `last_error` says which. A quarantine older
than twice the push timeout is treated as a killed process's debris and
removed, so a stale directory cannot defer every later run. `failed` keeps the
error message on the row and the job is retried with the runner's backoff.

`bytes_before`/`bytes_after`/`expired_objects` are the last **completed**
measurement: a failed or deferred run updates `last_outcome` without zeroing
them. Object identifiers are not added to the row or the audit payload. A Git
error string kept in `last_error` is bounded to 500 characters but can contain
a path or an object id from Git's own message, so treat it as diagnostic rather
than sanitized text.

The first run on a repository that was never packed adds the pack index, the
reverse index and the multi-pack index to `objects/`, so `bytes_after` can be
larger than `bytes_before` the first time. Later runs are the ones that shrink a
repository. Compare `bytes_before` and `bytes_after` per run rather than
expecting the first run to save space.

## Interruptions

Every step is safe to interrupt:

- `repack` writes each pack under a temporary name and renames it; a crash
  leaves the old packs, the new pack or both.
- The deletion pass removes one loose object at a time; a crash leaves the rest
  for the next run.
- Verification runs on every completed run, so a run that stops earlier is
  simply repeated.

A restarted run (the job retry, or the operator command above) finishes the
work. No reachable object is ever a candidate: the deletion set is computed
from `git rev-list --objects --all` plus the pinned merge candidates and every
kind of pin (heads, bases, review comments, statuses).

The job has no lease heartbeat. Its claim lease is twice the run budget, so a
run inside its own budget cannot lose the lease; a run that did outlive it
(only possible if the process stalls past 2 × (7 × `housekeeping_timeout_seconds`
+ 60 s)) can be reclaimed by another runner, and the fenced result update then
discards its outcome. The repeat run is safe — every step is idempotent — but
the operator sees the first attempt reported as a retry. A graceful shutdown
also waits up to the longest run budget plus one second before returning an
in-flight job to `pending`.

## Pinned commits: what a prune may not delete

The keep-set is every ref plus every object ID PostgreSQL references. A run
starts every walk from those pins as well, of every kind:

- **Pull-request heads.** Opening a pull request records its head commit in
  `pinned_commits`, in the repository that owns the head, in the same
  transaction as the pull request. Every run refreshes the pin of an open
  request's current branch, so a force push is followed.
- **Pull-request bases.** The base commit of the request (the target branch's
  tip when it opened) is pinned in the **target** repository in the same
  transaction, so a closed request whose target branch is later deleted still
  shows `compare/base...head`.
- **Review comments.** An anchored comment pins the commit it names while the
  comment exists; deleting the comment releases it.
- **Commit statuses.** Posting a status pins its commit while that status is
  the latest result for its context. A newer result for the same context
  supersedes it, and its pin then expires after
  `housekeeping_pin_retention_days` instead of living with the row. That is what
  stops a status on a since-unreachable commit from pinning its whole ancestry
  forever.
- **Merge candidates.** An intent in `validating` or `executing` pins its
  candidate (and result), and the pin ends when the intent moves its ref.

A closed or merged request keeps its head and base pins for
`housekeeping_pin_retention_days` (90 by default), counted from the close or
merge, so its diff stays readable even after the source branch is deleted and a
prune has run; the read path resolves a missing branch to the head pin and a
missing target branch to the base pin. A pin is a database row, not a Git ref:
it is never advertised, and clients cannot fetch it. Pinned objects live in
`objects/` and therefore count toward the quota.

A pin is only a keep-set entry: it does not fetch anything. A commit whose
objects are not in the repository a pin names keeps nothing (`cat-file` reports
it missing and the walk skips it).

### Fork pull requests

A fork pull request's head objects are pinned in the **fork**, which holds the
authoritative copy. The **target** also keeps a copy of the head, because its
`compare`, code-owner checks and mergeability resolve the head there; that copy
is pinned in the target too. The copied bytes live in the target repository and
are charged to the target organization (they count toward its quota, and
`repository_fork_fetches` records how much each fork has cost it). The copy is
bounded by `housekeeping_fork_fetch_limit_bytes` (64 MiB by default): a head
whose objects would add more than that to the target is refused **before**
anything is copied, logged, and the pull request still renders from the fork's
own pin.

If a target prune releases the copy (the pin expired, or the request was
deleted), the next read copies the objects from the fork again — under the same
cap — instead of answering `revision not found`. The fork's pin is what makes
that possible; deleting the fork ends it, and the request then reports no head,
exactly as before pins existed.

## Disabling it

Set `housekeeping_enabled = false` or `GITHEARTH_HOUSEKEEPING_ENABLED=false` and
restart. The scheduler stops and the job handler becomes a no-op, so any job
still queued finishes without touching storage. Space is then never reclaimed
automatically; the explicit operator command above still runs when you ask for
it. Re-enable it by removing the setting or setting it to `true`.

## Related

- [Storage quota](quota.md): how `objects/` size is measured and enforced.
- [Backup and restore](backup-restore.md): housekeeping does not change what a
  backup must contain, but run a backup before the first maintenance of a large
  installation if you want the pruned objects preserved.
