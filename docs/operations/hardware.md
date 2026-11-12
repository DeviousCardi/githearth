# Hardware requirements

Minimum and recommended hardware, derived from measurement, not from a vendor
table. The recorded run is in
[verification/hardware.md](../verification/hardware.md); it supersedes the
single-point estimate in
[verification/m4-qualification.md](../verification/m4-qualification.md#proposed-minimum-hardware).

These are for a single-node GitHearth server with PostgreSQL on the same host.
The Git objects live in local storage; PostgreSQL is a separate service.

## What drives resources

- The server process is small: idle ≈ 40 MiB, peak ≈ 84 MiB even under a
  64-client burst.
- PostgreSQL uses ≈ 73 MiB for the workload measured.
- Memory is dominated by `git` children. Each concurrent
  `upload-pack`/`pack-objects` costs ≈ 25 MiB, bounded by `git_concurrency`
  (default 8). The largest single cost is cloning a big, mostly-incompressible
  repository: packing a ≈1.44 GiB object store peaked at ≈1.7 GiB of server +
  git memory.
- Clone bursts are CPU-bound in `pack-objects`. One core works; more cores lower
  latency when many clones run at once.

## Minimum

For a small installation (a few repositories, each a few tens of MiB, default
`git_concurrency` 8):

- **CPU: 1 core.** Clone bursts are CPU-bound, not memory-bound.
- **RAM: 1 GiB.** Server + git peaked at ≈ 270 MiB and PostgreSQL at ≈ 73 MiB in
  [m4-qualification.md](../verification/m4-qualification.md#measured-resource-use),
  leaving room for the OS and page cache. Lower `git_concurrency` on smaller hosts.
- **Disk:** the repositories' size plus their journals, the database (≈ 10 MiB
  for the measured workload), and room for backups and for `doctor`'s free-space
  check (warns below 10% free or below twice `git_push_limit_bytes`). PostgreSQL's
  disk is monitored separately; see
  [db-disk-full](../verification/db-disk-full.md).

## Recommended

For comfortable operation and headroom, and required as soon as you host a
repository larger than a few hundred MiB:

- **CPU: 2 cores or more.** A single clone of a small repository took ≈ 0.55 s;
  8 concurrent clones raised p50 to ≈ 1.0 s and 64 clients to ≈ 4.5 s on one
  machine. Extra cores shorten those tail latencies.
- **RAM: 2 GiB, scaled to the largest repository you expect to clone.** Plan for
  roughly `postgres (≈ 100 MiB) + server (≈ 100 MiB) + git_concurrency × 25 MiB
  + the peak pack of your largest repository`. The measured clone of a ≈1.44 GiB
  incompressible repository peaked at ≈ 1.7 GiB, so allow ≈ 1.5× your largest
  repository's size for `pack-objects`. This is the single number that scales:
  a 1 GiB repository needs ≈ 2 GiB total; a 5 GiB repository needs several GiB.
- **Disk:** the repositories plus journals, the database, and enough free space
  that `doctor` stays above its warning threshold during the largest push and
  backup.

`git_concurrency` (env `GITHEARTH_GIT_CONCURRENCY`) trades memory against
throughput: each concurrent git child costs ≈ 25 MiB, and excess requests are
refused as busy rather than queued.

## Limits not measured

These bounds were not measured and should not be read as supported capacity:

- **Larger repositories.** The "large" drill was 50 000 commits and ≈ 1.44 GiB;
  bigger repositories need their own measurement, especially for clone/push
  memory and wall time.
- **More than 64 concurrent clients.**
- **A soak longer than 20 minutes.** RSS grew ≈ 22 MiB over 20 minutes and stayed
  under the 32 MiB ceiling; a longer soak and allocator profiling were not run,
  so a slow leak is not ruled out.
- **SSH resource use under load.** The drill measures HTTP; SSH for small pushes
  is bounded in [m4-qualification.md](../verification/m4-qualification.md#scenario-results)
  (s3, s10) but not under the large-repository or concurrency workloads.
- **A full host disk under PostgreSQL.** Covered separately by
  [db-disk-full](../verification/db-disk-full.md); the disk-full drill there
  fills a container tmpfs, not the host filesystem.
- **CPU-accounting precision.** CPU is read from `/proc` jiffies and is
  approximate; wall time is the reliable number.

Publish updated numbers whenever the largest repository, concurrency settings or
user count changes; one machine is one measurement, not a guarantee.
