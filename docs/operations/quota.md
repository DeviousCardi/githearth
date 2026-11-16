# Storage quota

`repository_quota_bytes` (env: `GITHEARTH_REPOSITORY_QUOTA_BYTES`) sets a
per-repository on-disk size limit and `organization_quota_bytes` (env:
`GITHEARTH_ORGANIZATION_QUOTA_BYTES`) sets the total for every active
repository in one organization. Defaults: 5 GiB and 50 GiB. `0` disables a
quota. Valid range: `0` or `1 MiB`–`1 TiB`, and a non-zero repository quota may
not exceed the organization quota.

**What is counted:** every regular file under `objects/` in each bare
repository, plus any objects staged in the quarantine directory during a push.
Symlinks are never followed. The organization figure is the sum of that tree
over every non-deleted repository in the organization, plus the pushing
repository's incoming quarantine.

**What is not counted:** `refs/`, `packed-refs` and journal files; the object
store's large objects (that is a separate per-organization ledger, ADR 0014
§1); and repositories already marked `deleted`.

**When a server-initiated write is charged:** a merge charges its own objects
(the merge commit, a squash's tree, a fetched fork head) before the ref moves.
Two other server-initiated writes are not checked at the moment they happen: a
full repository fork's initial copy and a suggestion commit. Their bytes are
counted by the organization sum, so the next push or merge in that
organization is refused until it is back under the limit; only the merge path
refuses before writing. This is deliberate for M4.2 — a fork that fails after
its copy would need a rollback path of its own — and is a known gap, not an
unlimited allowance.

**When it is checked:** a push is measured in `pre-receive`, before any ref
moves or journal record is written. The server measures the organization's
other repositories before spawning `receive-pack` and hands the figure to the
hook, which only adds what it can see on disk. A server-side merge (including a
squash, a rebase and the cross-repository fetch that brings a fork's head in)
is measured after the candidate is written and before the ref moves, so
merge-created objects are charged. A refused merge leaves its candidate
objects unreachable until housekeeping reclaims them after the grace period.

**Cost:** both checks walk the `objects/` trees they measure — one walk of the
organization's whole set per push and per merge — on the blocking pool while
the repository lock is held. A tenant with many repositories or many loose
objects sees a delay proportional to its object count before `receive-pack`
starts, and the delay is bounded by nothing but the tree size. The alternative
(a cached byte count) was rejected: it drifts after a crash, a restore or a
hand-run `gc`, and this quota is a refusal, not an estimate.

Only `objects/` is counted, so repacking can change the measured size; that
matters only close to the limit.

**How to raise it:** increase `repository_quota_bytes` or
`organization_quota_bytes` in configuration, or set either
`GITHEARTH_REPOSITORY_QUOTA_BYTES=0` / `GITHEARTH_ORGANIZATION_QUOTA_BYTES=0`
to disable it. Restart the server. Pushes that bring no new objects (deleting a
branch, pointing a ref at an existing commit) are always allowed.

**How space is freed:** deleting refs does not free space by itself, because
`gc.auto` is 0. Scheduled [housekeeping](housekeeping.md) repacks the repository
and removes unreachable objects once they are older than
`housekeeping_grace_seconds` (default eight days). After it runs, the next push
sees the smaller `objects/` size, so a push refused with "repository storage
quota exceeded" can succeed once housekeeping has reclaimed the deleted history.
Check when it last ran:

```sql
SELECT bytes_before, bytes_after, last_run_at, last_outcome
FROM repository_housekeeping WHERE repository_id = '<uuid>';
```

To reclaim space immediately rather than waiting for the schedule, stop the
server and run `githearth-server housekeeping --repository <uuid>`: the lock is
in-process only, so the command must not run beside a live server (see
[housekeeping](housekeeping.md#running-it-by-hand)). It uses the same grace
period, so objects younger than the grace period stay. Lowering
`housekeeping_grace_seconds` shortens the wait but also shortens the grace that
protects an unreconciled push or a just-created object before it is reachable
(see [housekeeping](housekeeping.md#configuration)); a recorded merge-candidate
keep point and a pull-request head pin are not governed by the grace period,
because they are explicit and are held for as long as their row says.

A pull request's head and base, and their reachable history, are kept by pins
for `housekeeping_pin_retention_days` (90 by default) after the request closes
(open requests are re-pinned on every run; closing or merging starts the
window). Those objects remain inside `objects/`, so they count toward the
quota exactly like reachable objects; a pin protects them from pruning, it does
not exempt them from the limit.

A fork pull request's head is also copied into the **target** repository (the
target's `compare`, code-owner checks and mergeability resolve it there) and
pinned there, so those bytes are charged to the **target organization**:
they are part of the target's measured `objects/`, and
`repository_fork_fetches` records how many bytes and fetches each fork has
cost it. One copy may add at most `housekeeping_fork_fetch_limit_bytes`
(64 MiB by default) to the target; a larger head is refused before it is
copied, and the pull request renders from the fork's own pinned copy instead.

The push check happens in `pre-receive`, so a refused push writes no journal
record and moves no ref; the merge check happens before the ref is
compare-and-swapped, so a refused merge moves nothing either. If the size
cannot be measured the push or merge is refused rather than treated as free.
