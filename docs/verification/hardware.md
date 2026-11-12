# Hardware minimums

Date: 2026-11-12. State: **measured on one machine; minimum and recommended
hardware published in [hardware.md](../operations/hardware.md).** This closes the
"Hardware minimums beyond one host" item from
[m4-qualification.md](m4-qualification.md#remaining-scope) for this host. It is
one machine and one workload, not a capacity claim.

The drill is [`scripts/verify-hardware.py`](../../scripts/verify-hardware.py),
which subclasses the M4 harness in [`scripts/verify-m4.py`](../../scripts/verify-m4.py)
and reuses its server, API, Git and process-sampling plumbing. It measures
server-process and server-plus-git-children RSS (via `/proc`), CPU time (via
`/proc/<pid>/stat` jiffies) and client latency for three scenarios:

- **large repository** — a synthetic 50 000-commit repository with 9 × 100 MiB of
  incompressible data (≈ 1.44 GiB of objects), then clone, push and browse;
- **concurrency sweep** — 1, 8, 32 and 64 simultaneous clones of an 8 MiB
  repository;
- **soak** — 20 minutes of repeated push + clone + browse, watching for RSS
  growth, leaked `git` children and errors.

The harness builds the release binary, runs a disposable `postgres:17-bookworm`
container and the server on ephemeral loopback ports, never prints generated
secrets, and removes every container, process and temporary directory on exit.
All synthetic data lives under `./.scratch/` (see the `--workdir` flag).

## Environment

| Item | Value |
| --- | --- |
| Source revision | `2f3bb08` |
| Host toolchain | rustc 1.99.1; server built in release mode by the harness |
| Git client and server-side Git | 2.43.0 |
| PostgreSQL fixture | `postgres:17-bookworm`, disposable container |
| Docker Engine | 29.1.3 |
| OS / kernel | Linux 7.0.0-42-generic x86_64, glibc 2.39 |
| CPU | 16 cores |
| RAM | 15.0 GiB |

## Command

| Command | Result |
| --- | --- |
| `python3 scripts/verify-hardware.py --report .scratch/hw-report.json` | **Passed, 3/3 scenarios.** Afterwards no `githearth-server` process and no harness container remained. |

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Large repository | Generates a 50 000-commit / ≈1.44 GiB repository, then measures clone, push and browse latency and the server's RSS/CPU throughout. | PASS | 1300 s |
| 2 | Concurrency sweep | 1, 8, 32 and 64 simultaneous clones; asserts concurrent `upload-pack`/`receive-pack` children never exceed `git_concurrency` (8) and records p50/p95/p99 latency and peak RSS. | PASS | 10.7 s |
| 3 | Soak | 20 minutes of push + clone + browse (959 iterations); asserts zero errors, zero leaked `git` children, and RSS growth under a 32 MiB ceiling. | PASS | 1204 s |

## Measured resource use

### Large repository (50 000 commits, ≈1.44 GiB of objects)

| Measurement | Value |
| --- | --- |
| Synthetic generation (fast-import + 9 × 100 MiB `dd`) | 795 s |
| Push wall time | 373 s |
| Push, server RSS peak | 56 MiB |
| Push, server + git children RSS peak | 767 MiB |
| Clone wall time | 125 s |
| Clone, server RSS peak | 75 MiB |
| Clone, server + git children RSS peak | **1709 MiB** |
| Browse, `/refs` p50 | 21 ms |
| Browse, `/tree/main` p50 | 21 ms |
| Browse, `/commits/main?limit=50` p50 | 21 ms |
| Browse, `/commit/<oid>` p50 | 269 ms |

The clone peak is dominated by `git pack-objects` on the server side: for a
repository of mostly incompressible data, packing the ≈1.44 GiB object store
needs roughly 1.7 GiB of process memory. The server process itself stays below
100 MiB throughout.

### Concurrency sweep (8 MiB repository, default `git_concurrency` 8)

| Clients | Succeeded | Busy | p50 | p95 | Max concurrent git children | Server + git peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 1 | 0 | 559 ms | 559 ms | 1 | 83 MiB |
| 8 | 8 | 0 | 1073 ms | 1084 ms | 8 | 229 MiB |
| 32 | 16 | 16 | 2691 ms | 2700 ms | 8 | 233 MiB |
| 64 | 26 | 38 | 4948 ms | 4965 ms | 8 | 247 MiB |

Concurrent git children never exceeded the configured 8. Excess clones get an
immediate busy response rather than queueing, so the "busy" column is the
admission throttle working. Latency climbs with concurrency because clone bursts
are CPU-bound in `pack-objects`; each concurrent `upload-pack`/`pack-objects`
costs about 25 MiB.

### Soak (20 minutes, 959 push + clone + browse iterations)

| Measurement | Value |
| --- | --- |
| Iterations | 959 |
| Server RSS, start (median of first 3 samples) | 39 MiB |
| Server RSS, end (median of last 3 samples) | 60 MiB |
| Server RSS, peak | 60 MiB |
| RSS growth | 22 MiB |
| Errors | 0 |
| Leaked git children after settle | 0 |

The 22 MiB growth over 959 iterations is within the 32 MiB ceiling and is
consistent with allocator arena growth in a multi-threaded process, not a
confirmed leak. A longer soak and allocator profiling are listed as not measured
below.

## Derived hardware

The numbers above, together with the earlier single-host measurement in
[m4-qualification.md](m4-qualification.md#measured-resource-use), are translated
into minimum and recommended hardware in
[hardware.md](../operations/hardware.md). In short: the server process is small
(under 100 MiB), PostgreSQL uses ≈ 73 MiB, and memory is dominated by `git`
children, so the decisive variable is the largest repository you expect to
clone.
