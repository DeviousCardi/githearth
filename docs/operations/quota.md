# Storage quota

`repository_quota_bytes` (env: `GITHEARTH_REPOSITORY_QUOTA_BYTES`) sets a global
per-repository on-disk size limit. Default `0` = unlimited. Valid range:
`0` or `1 MiB`–`1 TiB`.

**What is counted:** every regular file under `objects/` in the bare repository,
plus any objects staged in the quarantine directory during a push. Symlinks are
never followed.

**What is not counted:** merge-created objects (not checked on push), objects
arriving via any path that does not go through a push (e.g. GC or
server-initiated operations), and `refs/` / `packed-refs` / journal files.

Only `objects/` is counted, so scheduled repacking can change the measured
size; that matters only close to the limit.

**How to raise it:** increase `repository_quota_bytes` in configuration or set
`GITHEARTH_REPOSITORY_QUOTA_BYTES=0` to disable. Restart the server.
Pushes that bring no new objects (deleting a branch, pointing a ref at an
existing commit) are always allowed. Deleting refs does not free space by
itself: `gc.auto` is 0, so unreachable objects are removed only by scheduled
repository maintenance. The check happens in `pre-receive`, so a refused push
writes no journal record and moves no ref. If the size cannot be measured the
push is refused.
