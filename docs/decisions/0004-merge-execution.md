# ADR 0004: Merge execution contract

Date: 2026-10-29.

Status: **proposed for M3**, pending Person A review. Defines the boundary between Person A (pull requests, reviews, checks, protection, persisted merge intents/scheduling, UI) and Person B (comparison, merge methods, expected-ref updates, journaled execution, duplicate-worker handling). A reviews this before either side builds on it.

## Constraints

From the [metadata contract](../plan/01-metadata-contract.md), [forge roadmap](../plan/04-forge-roadmap.md) M3 and [ADR 0003](0003-git-push-and-repository-lifecycle.md):

- There is no atomic commit across PostgreSQL and Git. A merge intent row, an operation ID, a Git result and a reconciler stand in for one; the journal is the durable merge record.
- Merges are ref changes made outside `receive-pack`, so ADR 0003's Consequences require them to go through the same `reference-transaction` path with an operation ID.
- Metadata keyed by `(repository_id, journal_sequence)` is applied at most once; replaying the same journal record changes nothing.
- Authoritative refs, objects and the journal live in Git storage; `gc.auto` is `0`, so maintenance is scheduled and never inline.

## Ownership split

| Person A — application | Person B — Git and infrastructure |
| --- | --- |
| Pull requests, reviews, approvals, checks | Comparison (merge-base, diff), merge methods |
| Branch protection config and policy generation | Candidate construction without a work tree |
| Merge intent table, state machine, lease, queue/auto-merge | Expected-old ref update and journaled execution |
| UI and notifications | Merge-record replay and duplicate-worker handling |

The narrow interface is three Rust function boundaries:

1. `githearth_git::merge::build_candidate(repo_path, method, source_oid, target_oid, base_oid, author, committer) -> Result<Candidate, MergeError>` — B. Produces the merge result object and commit metadata without a work tree, or the conflicting paths.
2. `githearth_git::merge::update_ref_expected(repo_path, reference, new_oid, expected_old_oid, operation, actor) -> Result<(), MergeError>` — B. Runs `git update-ref --stdin` with `update <ref> <new> <expected-old>` under `core.hooksPath`, so the `reference-transaction` hook journals the transaction with the intent's operation ID and actor.
3. `githearth_server::merge::execute_intent(state, intent_id) -> Result<MergeOutcome, MergeError>` — B, and the function A's worker calls after claiming an intent. It re-reads the intent under the per-repository lock, revalidates, builds the candidate, executes the ref update, and returns `Merged { oid } | AlreadyMerged { oid } | Conflicted { paths } | Stale { reason } | Failed { reason }`.

In the other direction B calls one A function, `record_merge_result(conn, repository_id, journal_sequence, operation_id, actor, old_oid, new_oid, ref)`: in one transaction it sets the intent `merged` with the result OID and inserts the ref-update, audit and outbox rows, keyed by `(repository_id, journal_sequence)` so a second call changes nothing. `execute_intent` calls it right after the ref commits (as push stage 7 does), and the reconciler calls it on replay. A failure there never turns `Merged` into a failure. B writes the intent row only through this function.

## Merge intent (A's table)

Columns only; A designs the migration and lease columns.

| Column | Meaning |
| --- | --- |
| `operation_id` | Unique operation UUID; also the journal operation ID |
| `repository_id` | Target repository |
| `pull_request_id` | The pull request being merged |
| `source_ref`, `source_oid` | PR head ref and the OID it is bound to |
| `target_ref`, `expected_target_oid` | Destination ref and the OID it is expected to be at |
| `method` | `merge` \| `squash` \| `rebase` \| `ff_only` |
| `candidate_oid` | Result commit returned by `build_candidate`; A's worker stores it |
| `policy_generation` | Branch-protection generation the intent was validated against |
| `checks_approvals_digest` | Hash of `source_oid` plus the IDs of each reviewer's latest non-comment review at `source_oid` (approvals and change requests, so a new change request also makes the intent stale) and the check results |
| `actor_id` | The merging account |
| `state` | See state machine below |
| lease columns | A's claim fence (holder, attempt, expires) for the queue |

State machine: `queued → validating → executing → merged | conflicted | stale | failed | cancelled`.

- A performs `queued → validating`, `validating → executing` (its worker), and `* → cancelled` (queue cancellation before commit).
- `pull_requests.state` gains `merged` (with `merged_at`) in A's intent migration; `record_merge_result` sets it in the same transaction as the intent.
- `→ merged` happens only through `record_merge_result` (from `execute_intent` or the reconciler). A's worker applies `executing → conflicted | stale | failed` from the returned outcome; `AlreadyMerged` needs no transition.
- `cancelled` is refused once the intent is `executing`; cancellation after that point loses to a ref commit.

## Supported methods (M3)

The server checks Git ≥ 2.38 at startup for these (2.43 is in use). `git replay` is not used (too new).

| Method | Candidate construction |
| --- | --- |
| Merge commit | `git merge-tree --write-tree <target> <source>` for the tree; `git commit-tree <tree> -p <target> -p <source>` |
| Squash | Same tree as merge commit; `git commit-tree <tree> -p <target>` with the PR title/body as the message |
| Rebase (cherry-pick sequence) | For each commit `C` in `git rev-list --reverse target..source`, `git merge-tree --write-tree --merge-base=C^ <previous> C` (with `<previous>` starting at the target tip), then `git commit-tree <tree> -p <previous>`. A range containing a merge commit is refused (`Failed`, "cannot rebase merge commits"); a commit whose result tree equals its parent's is dropped, which also drops intentionally empty commits |
| Fast-forward only | No new object; `source_oid` is the new target value, accepted only when `target_oid` is an ancestor of `source_oid` |

Committer identity is the configured server identity. The merging actor authors merge commits; the squash author is set by A (see open items); each rebased commit keeps its original author, author date and message. `build_candidate` receives every identity and timestamp from the caller, so it never reads configuration or the clock. A conflict makes `build_candidate` return the conflicting paths; the intent goes `conflicted`, no ref changes, and the paths are reported. Candidate objects are unreferenced until the ref moves and are left to scheduled maintenance: the scheduled GC never prunes objects unreferenced for less than 48 hours (configurable, and longer than any intent's queue lifetime), so a candidate survives the execute window and a crash recovery.

## Execution

Under the per-repository lock (`Storage::repository_lock`, acquired before any pool connection), `execute_intent`:

1. Re-reads the intent and revalidates every fact it recorded:
   - the pull request still open, and its source and target branches still equal the intent's `source_ref` and `target_ref` (A cancels `queued` intents when a PR closes, but that can race a worker already `executing`, so this check sits under the lock);
   - target still at `expected_target_oid`, source still at `source_oid`;
   - `policy_generation` unchanged;
   - actor still authorized (`pull_request.merge` through the Cedar engine with merge facts for the exact head/target OIDs);
   - approvals/checks still bound to the same source OID (digest recomputed equals the stored digest).
2. Reuses the stored `candidate_oid` when its first parent is the current target tip, otherwise builds a new one. Candidate OIDs include commit timestamps, so a rebuilt candidate is a different OID; nothing in M3 binds to candidate OIDs (see below).
3. Runs `git update-ref --stdin` with `update <target_ref> <candidate_oid> <expected_target_oid>` (`<source_oid>` for ff-only), launched with `core.hooksPath` and the environment carrying `GITHEARTH_OPERATION`, `GITHEARTH_ACTOR` and `GITHEARTH_MERGE=1`. The `reference-transaction` hook's environment guard changes to require only operation and actor; the socket and token stay required for `pre-receive`, which `update-ref` never runs. The hook journals the transaction exactly as for a push, so a merge ref change is durable before `update-ref` returns success.

No `pre-receive` runs for merges: policy was decided at revalidation. This is equivalent because the server computed the exact ref update itself and holds the per-repository lock that serializes every ref writer, including `receive-pack` pushes (`git_http` takes the same lock before `receive-pack` touches any ref); no other writer can move the target between the decision and the ref commit, so a second policy decision at commit time would evaluate the same facts and cannot change the outcome. The `reference-transaction` hook that does run is journaling, not policy.

`Storage::repository_lock` is an in-process `tokio::sync::Mutex`, so this argument holds for one server process. M5 (several gateways) must replace it with a fenced, cross-process lock: expected-old still prevents a double merge across processes, but policy-at-revalidation would no longer be equivalent to a commit-time decision.

## Stale authorization and checks

In M3, checks and approvals bind to code by the source head OID only. Until a check system exists (M6), "no checks recorded" means `checks_passed = false`, so a rule with `require_checks` refuses every merge rather than passing silently. Binding checks to a tested candidate (merge queue) needs deterministic or persisted candidates and is decided with the queue increment.

- Target moved (`expected_target_oid` mismatch), source unchanged: approvals/checks still bind to the same source code, so no new approval is required; only a recomputed candidate against the new target tip is needed. The intent goes `stale` and the queue re-drives it with the new `expected_target_oid`, unless branch protection requires the source to be up to date with the target (a future `require_up_to_date` rule flag, which A adds with merge intents; #30 has no such flag yet), in which case the author must update the branch.
- Source moved (`source_oid` mismatch): existing approvals/checks do not bind to the new head; new approvals/checks are required, and the intent goes `stale`.
- Revocation race (admission rule): authorization is evaluated at revalidation under the lock. A grant, approval or check revocation that commits after that point does not stop that one ref transaction; it applies to every later admission, exactly as in ADR 0003's push acknowledgement boundary.

## Duplicate delivery and concurrent workers

A's lease means only one worker claims an intent at a time. The rest is B's idempotence, argued for "cannot merge twice":

1. Under the per-repository lock and before executing, B looks up the intent's operation ID in the journal. A committed record means the merge already happened; B returns its outcome instead of re-running Git.
2. No record yet: any number of workers serialize on the per-repository lock. The first builds, revalidates and runs `update-ref` with `expected-old = expected_target_oid`; the ref moves.
3. The second worker, on taking the lock, either finds the committed journal record (step 1) or revalidates and sees the target no longer at `expected_target_oid`, and goes `stale`.
4. Even a worker that skipped the journal lookup would fail at the ref: `git update-ref` refuses a `update` whose expected old value does not match the current ref value.
5. Two different intents on one target serialize on the lock; the first wins and the loser's expected-old mismatches, so it goes `stale`.

Every merge is therefore a compare-and-swap on the target ref's current value, and the target moves at most once.

## Lost metadata after the Git commit

The merge's prepared record gains an optional `origin` field, `"merge"`, copied by the `reference-transaction` hook from `GITHEARTH_MERGE`; records without it are pushes, so existing journals stay valid. The operation ID is the intent's. Push replay (`pushes::replay`) skips merge records, and the reconciler dispatches them to `record_merge_result` instead, so a merge is never also recorded as a push. Applying the same record twice changes nothing, so metadata that lags the Git commit converges after a crash or restart.

## Recovery

| Crash point | Journal | Refs | Recovery |
| --- | --- | --- | --- |
| Before candidate | none | unchanged | A re-drives from `queued`/`executing` after lease expiry; intent eventually `merged`, `stale` or `failed` |
| After candidate, before lock | none | unchanged | Candidate unreferenced (GC grace); re-drive rebuilds it |
| Prepared only, ref at expected old | prepared, no marker | target at `expected_target_oid` | aborted (marker lost): reconciler appends the aborted marker; no metadata; worker may re-drive |
| Committed, marker lost | prepared, no marker | target at candidate | committed-marker-lost: reconciler appends the committed marker and replays the merge record |
| Committed before metadata | prepared + committed | target at candidate | reconciler replays the merge record into the intent + ref-update/outbox rows |
| Metadata done | prepared + committed | target at candidate | reconciler no-op (past the high-water mark) |

## Consequences

- A merge is a compare-and-swap ref update journaled through the same `reference-transaction` path as pushes, satisfying ADR 0003's requirement that every non-receive-pack ref change be journaled and authorized identically.
- The intent row is metadata; the journal record is the durable merge result. One idempotent function turns a merge record into a merged intent, on the normal path and on replay, so lost metadata self-heals.
- Candidate objects are unreferenced until the ref moves; scheduled maintenance must honour the 48-hour grace period above.

## Open items

- Signed merge commits and a signed-commit policy (phase 4/5).
- Fork contributions: cross-repository source refs; M3 merges same-repository refs only (phase 4/5).
- Merge queue batching: rebasing/retesting each queued PR against the previous one (phase 4/5).
- Code owners as an approval source (phase 4/5).
- Squash commit author: A's decision is the pull request author as author, with the merging actor recorded in a trailer; settled when A builds the intent.
- Binding checks to tested candidates for the merge queue (see Stale authorization and checks).
