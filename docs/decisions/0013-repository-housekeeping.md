# ADR 0013: Repository housekeeping (repack, prune and grace)

Date: 2026-11-15.

Status: **proposed for M4.1**. Defines which objects scheduled maintenance may
delete, how it is excluded from every concurrent writer, and how a run that is
interrupted at any step finishes its work without losing a reachable object.
It corrects the grace period ADR 0004 assumed and settles ADR 0008's open
question about a fork's object store.

## Context

Every repository is created with `gc.auto = 0` (ADR 0003), so nothing repacks
loose objects or removes unreachable ones. Deleting a branch therefore frees no
space, and `docs/operations/quota.md` refers to "scheduled repository
maintenance" that does not exist. Roadmap M4.1 lists repository housekeeping:
"scheduled repack and pruning of unreachable objects as a durable job, safe
against concurrent pushes, merges, quarantine and forks; quota accounting
reflects the result."

A wrong decision here is destructive: objects an accepted push or a merge still
needs are unreachable for a while, and once pruned they cannot be recovered
except from a backup (ADR 0003 "Backup and restore").

The writers that create objects or move refs on one node:

| Writer | Ref changes | Objects | Lock |
| --- | --- | --- | --- |
| Smart HTTP push (`git_http::receive_pack`) | `reference-transaction` hook | `receive-pack` writes them to a quarantine directory, then migrates them into `objects/` | in-process per-repository lock, held from before `receive-pack` spawns until the post-push journal replay finishes |
| Merge worker (`merge::execute_intent`) | `update-ref --stdin` through the same hook | `merge-tree` writes loose objects; a fork pull also `fetch_ref`s the fork's head into the target | the target's per-repository lock (plus the source fork's) |
| Suggestion apply, fork creation | journal hook, or `core.hooksPath=/dev/null` | candidate objects, a full fetch from the parent | the target's lock |
| SSH push (`ssh`) | `reference-transaction` hook | quarantine, as smart HTTP | the per-repository lock |
| Cross-repo head fetch (`pulls::fetch_cross_repo_head`, from create and every read) | none | `git fetch` writes the fork's head objects into the target, unreferenced | **no lock**; only the grace period protects them, and it protects nothing when set to zero |
| Mergeability (`browse::git_compare` → `build_candidate`) | none | candidate commit/tree/blob objects written into the target | **no lock**; only the grace period protects them |

The last two rows are the unlocked writers this ADR cannot exclude. The
reference server never reads a fetched or candidate object until a ref or keep
point names it, so a zero grace period can collect a target-side fetch between
the fetch and the diff; that is a pre-existing read-path property, not a
correctness rule the lock enforces.

`Storage::repository_lock` is an in-process `tokio::sync::Mutex` keyed by
repository id. M5 replaces it with a fenced cross-process lock (ADR 0004
"Execution"); this ADR is written against the same interface so that
replacement carries over.

## Decision

### What is live

A run computes exactly one live set for the repository it maintains, in Rust,
and deletes only loose objects outside it:

1. **Everything reachable from a ref.** `git rev-list --objects --all` over the
   repository's refs (branches, tags, and `HEAD`'s branch when it resolves).
   Refs are read under the repository lock, after the push journal has been
   replayed, so the set is the refs Git will commit next; a ref the hook has
   journaled but not committed cannot exist yet, and its transaction holds the
   lock.
2. **Objects in an in-progress push quarantine.** `receive-pack` holds the lock
   for the whole request, so no GitHearth push can be quarantined while a run
   holds it. Belt and braces: the run refuses the repository while a real
   quarantine directory (`objects/tmp_objdir-incoming-*`, the name Git's
   `tmp-objdir.c` creates) exists, because a manually started
   `git receive-pack` is invisible to the in-process lock. There is no lock
   file inside a quarantine. A quarantine older than twice the push timeout
   (`git_timeout_seconds`) cannot belong to a live push; it is the debris of a
   killed process, so the run removes it instead of deferring forever.
3. **Merge candidates.** A candidate object is unreferenced until
   `update-ref` moves the target. `merge::execute_intent` persists
   `candidate_oid` on the intent under the repository lock, before it moves
   the ref, so a crash in that window leaves a durable keep point. The caller
   resolves every unfinished merge intent of the repository (`validating` or
   `executing`) to its `candidate_oid` and `result_oid` and passes them as
   **keep points**. A keep point's commit, its tree and its parents pin the
   represented source and target history, so the candidate and the commits it
   merges cannot be collected. A keep point that no longer exists keeps
   nothing.
4. **Journal entries not yet reconciled.** The journal's commit markers carry
   no object ids, so "every object they could still need" would be all of
   history. Instead maintenance is never the first to resolve a journal: under
   the lock it replays the repository's journal into PostgreSQL
   (`pushes::replay`, idempotent), and refuses the repository when the replay
   reports divergence, a corrupt journal or a database error. The reconciler
   repeats the replay anyway.
5. **Alternates.** A repository whose `objects/info/alternates` names another
   object store borrows objects this run cannot see, so the run defers; GitHearth
   never creates one. This is the guard ADR 0008 relies on: if an operator adds
   alternates by hand, maintenance refuses to prune rather than deleting
   objects the other store still needs.
6. **Forks and parents.** ADR 0008 copied forks fully, and a repository's live
   set never depends on another repository's object store. A fork and its
   parent are therefore both safe to run on concurrently, and recursively: a
   pull request's head survives in a fork that has no writers because the
   fork's own refs keep it, and the parent's copy keeps the objects the merge
   fetched. GitHearth creates no alternates between them, so no run can remove
   an object another repository still reads. This settles ADR 0008's open question: **object
   stores stay independent and maintenance preserves that**.
7. **Pulled-in object ids (pins).** Opening a pull request writes a row in
   `pinned_commits` in the same transaction, in the repository that owns the
   subject. The pin is a row, not a Git ref, and is never advertised. The
   live-set walk starts from every live pin of every kind with
   `rev-list --objects --all <pins>`, so each pinned commit's tree and ancestry
   are protected too. Pinned objects stay inside `objects/` and therefore count
   toward the repository quota like any other object.

   The kinds (migration `0046`), each written in the transaction that writes
   the row referencing it, each with its own bound:

   - **Head** — the request's head, in the repository that owns it (the source
     repository for a fork pull request). Every run extends the pin of each
     open request's current source branch, so a force push is followed; a
     closed or merged request keeps its last pin for
     `housekeeping_pin_retention_days` (90 by default) from the close or merge,
     and the read path resolves a missing source branch to that pin.
   - **Base** — the request's base commit, in the **target** repository,
     written at create. It lives while the request exists and for the same
     retention after it closes, so a deleted target branch does not cost a
     closed request its `compare/base...head`. The read path resolves a
     missing target branch to it. Requests that predate `0046` cannot have
     their base recovered exactly (the row never stored it and there is no
     branch table), so `serve` backfills the base of every one of them that has
     no base pin, before the scheduler starts, using the target branch's
     current tip. What that does:
     - **Backfilled:** every open request; every closed request whose
       `closed_at` is inside `housekeeping_pin_retention_days`; every merged
       request whose `merged_at` is inside it. A closed or merged request's
       backfilled pin carries its real state, so it expires
       `housekeeping_pin_retention_days` from the backfill (the close or merge
       is at most that window in the past) rather than living while the row
       does. A closed or merged request outside the window is not backfilled:
       its retention has already elapsed.
     - **Approximated:** the pinned commit is the target branch's *current tip*.
       For a branch that has not moved since the request opened it *is* the
       base; for one that has moved it is a later commit that still reaches the
       original base, which is a safe reachable super-set.
     - **Skipped:** a request whose target branch no longer exists has no tip
       to pin, so it is left without a base pin — and that is exactly the
       request nothing else protects. It is recorded in
       `pull_request_base_backfill_skips` with its state and reason, and
       `githearth-server doctor` reports it by repository, number and state so
       an operator can rescue the base before the first prune. A repository
       whose Git storage cannot be read is reported and retried on the next
       start.
   - **Review comment** — the `commit_id` an anchored comment names, while the
     comment exists; it cascades with the comment. A reviewed commit is also
     pinned while any review names it (the pre-`0046` backfill already pinned
     reviews as head pins; this is the kind-owned row).
   - **Status** — the `commit_id` a status names while it is the latest result
     for its `(repository, commit, context)`, and for
     `housekeeping_pin_retention_days` after a newer result supersedes it. This
     is the bound U11 states, and it is what stops a status on a
     since-unreachable commit from pinning its ancestry forever.

   A fork whose repository is deleted loses its pins with it (the foreign key
   cascades), so a deleted fork still reads as a null head. A pin keeps nothing
   if the repository that owns it does not hold the objects: it is a keep-set
   entry, not a fetch.

8. **A fork pull request's target-side copy.** A fork pull request's head
   objects are copied into the target repository whenever the target has to
   resolve the head there (create, mergeability, code-owner checks), because
   `compare` and mergeability run in the target. The copy is pinned in the
   target as well, charged to the target organization — the bytes are in the
   target's `objects/`, which is what the quota measures, and
   `repository_fork_fetches` records the attribution. A copy is bounded by
   `housekeeping_fork_fetch_limit_bytes` (64 MiB by default), measured before
   anything is copied (the missing objects' stored size), so an oversized fork
   head cannot fill the target; the request then renders from the fork's own
   pin. The target-side copy is readable while the target's pin is live: the
   request's lifetime, then `housekeeping_pin_retention_days` after it closes
   or merges, the same window as every other pin. Once that pin expires a
   target prune releases the copy, and the target no longer renders the fork
   request's diff (`compare` on the target answers `revision not found`). No
   object is lost: the authoritative objects remain in the fork, under the
   fork's own pin, which is what the pull request and its diff in the fork's
   repository read. Restoring the target copy on the next read is **not**
   implemented: `pulls::resolve_cross_repo_head` runs only on the target's
   mergeability and code-owner paths, and both skip a closed request whose
   head comes from its pin. The authoritative copy is always the fork's.

Objects that are in no quarantine, reachable from no ref and reachable from no
keep point or pin are the only deletion candidates.

### Grace period, pinned to per-object age

A candidate is deleted only when its **file modification time** is at least
`housekeeping_grace_seconds` old (default 691200 = 8 days). The clock is the
filesystem's, per object, not the contents of a commit and not a wall-clock
guess about queue lifetimes.

A pack carries one file time for all its objects, not a per-object age. An
object that was reachable until yesterday can sit in a year-old pack, so
deleting on the pack's age would skip the grace period. **`git repack -A -d`**
is what makes the grace period real:

1. **Snapshot (`-A`, not `-a`).** `git repack -A -d -q --write-midx` repacks
   reachable history into one pack, indexed by a multi-pack index. `-A`
   *loosens* every unreachable packed object: it writes it as a loose file.
   Git copies the source pack's modification time onto that file, so the run
   touches the packs first; otherwise a year-old pack would make a
   just-unreachable object look a year old and delete it immediately. With
   that, the loose file's time starts its grace period now. `-a -d` would
   instead drop unreachable packed objects immediately, bypassing the grace
   period and able to destroy a merge candidate or a push's objects; it must
   not be used. Unreachable **loose** objects are never touched by `repack`;
   their age is their own. There is no separate unpack step: an earlier design
   used `git unpack-objects` on whole packs, but `unpack-objects` does not
   rewrite an object that already exists in the repository, so it wrote
   nothing.
2. A repository that has never been packed first grows by the pack index, the
   reverse index and the multi-pack index; later runs shrink it. The report
   states both sizes rather than promising a saving on the first run.

This replaces ADR 0004's “48 hours, longer than any intent's queue
lifetime”. 48 hours was a wall-clock guess; a candidate is kept by its keep
point for as long as its intent exists, and afterwards by the ordinary grace
period. ADR 0004's paragraph is amended to point here.

### Mutual exclusion with writers

Maintenance is one more holder of `Storage::repository_lock`, nothing else:

- It acquires the lock **before any pool connection** (the module-wide order in
  `reconcile.rs`), moves the guard into the `spawn_blocking` closure that runs
  Git (so a cancelled run cannot release it early), and drops it before writing
  the database result.
- It takes one repository lock at a time and never holds two, so it cannot
  deadlock with a cross-repository merge that holds a fork and its target.
- It uses `lock_owned().await`, never `try_lock`: a push or merge is never
  refused because maintenance holds the lock. It waits for the lock, then
  proceeds exactly as it does without maintenance. A push that arrives during a
  run waits for at most `7 * housekeeping_timeout_seconds` plus the job
  budget's slack, one timeout each for: the open-pin refresh; the live-set walk
  before the snapshot; the size walk before it; the snapshot; the live-set walk
  after it; the scan and deletion pass (whose deadline starts after the
  snapshot); and the verification (`fsck` and the final size walk share one
  phase). The push-journal replay that precedes them is bounded per Git step by
  `git_timeout_seconds`, not by this sum; a journal that cannot be replayed is
  deferred rather than pruned. The scan and the deletion stop cleanly when
  their deadline expires, leaving the rest for the next run. The push
  protocol's durability behaviour is unchanged.
- A push that Git refuses (deadline, quota, policy) fails against Git exactly
  as it does now. Nothing in admission, the hooks or the journal changes.
- The lock is in-process only (`tokio::sync::Mutex`), so the operator command
  in a separate process does **not** share it with a running server. The
  operations note therefore requires the server to be stopped, or housekeeping
  disabled with no job pending or running, before the command. A quiet
  "maintenance window" alone is not enough: the scheduler lives in the server
  and can claim a second run for the same repository. M5's fenced
  cross-process lock is what would let the two run together.

### The durable job

`repository.housekeeping` is a job kind (ADR 0006) with a scheduler task that
enqueues one job per repository every `housekeeping_interval_seconds` (default
86400), at most `housekeeping_concurrency` (default 2) in flight. The job's
`dedupe_key` is the repository id, so two enqueues for one repository cannot
overlap; the runner's lease and fencing add the rest.

The kind has its **own run budget** in `jobs::Registry`: seven step timeouts
plus 60 seconds of slack (`housekeeping_job_timeout_seconds()` in
`config.rs`), and its claim lease is twice that budget. The generic
`job_timeout_seconds` (60 s by default) is shorter than one step, so without
the per-kind budget a real run would be cut off before it could record
anything, its blocking Git work would outlive the job and the next scheduler
tick would enqueue it again. `Config` rejects a budget below the same
seven-phase relation or above the seven-day maximum.

The lease is not heartbeated. A run inside its budget cannot lose it (the lease
is twice the budget), but a process that stalls past the lease can have its job
reclaimed by another runner; the fenced outcome update then discards its
result, and the retried run repeats idempotent work. A graceful shutdown waits
up to the longest budget plus one second before returning a held job to
`pending`.

Eligibility: `status = 'active'` only.

- `archived` repositories are skipped: no writer can change them, so their
  space is not growing. Unarchiving enqueues a run, so a repository unarchived
  after a long archive is compacted promptly.
- `creating`, `deleting`, `deleted` and `failed` rows are skipped. `creating`
  becomes `active` and the scheduler picks it up; `deleting` is trash after
  `trash_retention_seconds`, not worth repacking.

Each run writes `repository_housekeeping` (migration
`0043_repository_housekeeping.sql`): `bytes_before`, `bytes_after`,
`last_run_at`, `last_outcome`, `last_error`. A successful, deferred or failed
run writes its result and one audit event in the same transaction as the row
update. A job has no account, so the audit actor is the repository's creator,
exactly as the reconciler's delete path does; the payload carries ids and sizes
and no content. A `failed` or `deferred` run leaves the last measured sizes
intact rather than zeroing them, so the row's sizes are the last completed
measurement and `last_outcome` says whether the latest attempt succeeded. A
failure keeps the failing step's message (bounded to 500 characters) and the
job retries with the runner's backoff. The message never contains configuration
or credentials.

`housekeeping_enabled = false` (default `true`) disables the scheduler and makes
the handler a no-op that returns `Done`, so a job left over from before the
switch changes nothing.

### Steps, in order, and what an interruption leaves

| Step | Interruption leaves |
| --- | --- |
| Resolve keep points and pins, compute live set | nothing changed |
| Live-set walk larger than `housekeeping_max_live_set_bytes` | nothing changed: the run fails before the scan, and the job retries |
| Snapshot (`repack -A -d --write-midx`) | old packs, the new pack, or both; all valid; the multi-pack index is rebuilt by the next run. `-A` has loosened unreachable packed objects into fresh loose files (all valid) |
| Deletion pass (scan and unlink share one deadline created after the snapshot) | some doomed loose objects removed, the rest for the next run; every removed object was unreachable and past the grace period. A failed unlink fails the run |
| Verification (`fsck --full`, `rev-list --objects --all`) | nothing recorded; the next run repeats the whole sequence |

Each fault point (`before_repack`, `after_repack`, `during_prune`,
`before_verification`) is injectable in the git layer so tests can assert the
repository still verifies and clones, and that a restarted run completes the
work.

### Verification and accounting

A run reports success only after `git fsck --full` and a `git rev-list
--objects --all` walk of every ref succeed. `bytes_before` and `bytes_after`
are `objects/` sizes, the same measurement the quota hook takes, so the quota
reflects freed space from the next push onwards. `docs/operations/quota.md` and
`docs/operations/housekeeping.md` describe the schedule, the grace period, and
how to run a repository by hand or disable the job.

## Alternatives considered

- **`git gc --auto` / `gc.auto` above 0:** rejected by ADR 0003; a push must
  not pay for a repack, and its timing is unobservable to admission.
- **`git gc --prune=now`, or `git prune`'s own `--expire` alone:** a bare
  `git prune` does not preserve unreachable-but-recent objects reliably and
  gives no keep-point mechanism, so a candidate built minutes ago could be
  collected. The live set is therefore computed explicitly.
- **`git repack -a -d`:** rejected: it deletes unreachable packed objects
  immediately, bypassing the grace period. `-A -d` is the mechanism the design
  relies on; it loosens them into fresh loose files instead. Cruft packs add a
  second pack format whose `--cruft-expiration` is a second, differently
  measured grace clock.
- **`git reflog expire --expire=now --all`:** a bare hosted repository has no
  useful reflog; the grace rule already covers what a reflog would keep alive.
- **Pinning every journal object id:** equivalent to disabling prune; replaying
  the journal first is the correct precondition.
- **A cross-repository global lock:** unnecessary for correctness (live sets
  are local, object stores independent) and would let one large run starve
  every push.
- **`alternates` between a fork and its parent:** rejected by ADR 0008, and it
  would make every run's live set depend on a foreign repository.

## Consequences

- Deleting a branch frees space after the grace period, never before; a push
  that re-creates a branch within the grace period keeps its objects.
- An interrupted run is never worse than "work not done yet"; an operator can
  re-run it with `githearth-server housekeeping --repository <uuid>`, with the
  server stopped because the lock is in-process only.
- A push or merge waits for at most one run of that repository: at most
  `housekeeping_timeout_seconds` each for the seven bounded phases (pin
  refresh, both live-set walks, the size walk, the snapshot, the scan plus
  deletion and the verification), plus the job budget's slack; the journal
  replay before them is bounded separately. `tokio::sync::Mutex`
  is first-come first-served, and a repository is enqueued at most once per
  interval, so a waiting push cannot be starved by back-to-back runs; if its own
  deadline expires first it fails cleanly and can be retried.
- A pull request's head survives a prune for `housekeeping_pin_retention_days`
  (90 by default) after the request closes or merges, so its diff stays
  readable; its objects remain in `objects/` and count toward the quota. A
  live-set walk larger than `housekeeping_max_live_set_bytes` fails the run
  before any deletion rather than pruning against a partial set.
- The first run on a repository that was never packed grows it by the pack
  index, reverse index and multi-pack index; the report shows that honestly.
- M5's replicated journal replaces "replay under the lock" with the same order
  against the replicated journal; the keep-point rule stays valid.
