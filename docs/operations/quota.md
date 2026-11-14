# Storage quota

`repository_quota_bytes` (env: `GITHEARTH_REPOSITORY_QUOTA_BYTES`) sets a global
per-repository on-disk size limit. Default `0` = unlimited. Valid range:
`0` or `1 MiB`–`1 TiB`.

**What is counted:** every regular file under `objects/` in the bare repository,
plus any objects staged in the quarantine directory during a push. Symlinks are
never followed.

**What is not counted:** merge-created objects (not checked on push), objects
arriving via any path that does not go through a push (e.g. server-initiated
operations), and `refs/` / `packed-refs` / journal files.

Only `objects/` is counted, so repacking can change the measured size; that
matters only close to the limit.

**How to raise it:** increase `repository_quota_bytes` in configuration or set
`GITHEARTH_REPOSITORY_QUOTA_BYTES=0` to disable. Restart the server.
Pushes that bring no new objects (deleting a branch, pointing a ref at an
existing commit) are always allowed.

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

A pull request's head, and everything it reaches, is kept by a pin for
`housekeeping_pin_retention_days` (30 by default) after the request closes
(open requests are re-pinned on every run; closing or merging starts the
window). Those objects remain inside `objects/`, so they count toward the
quota exactly like reachable objects; a pin protects them from pruning, it does
not exempt them from the limit.

The check happens in `pre-receive`, so a refused push writes no journal record
and moves no ref. If the size cannot be measured the push is refused.
