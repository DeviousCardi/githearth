# PostgreSQL disk-full verification

Date: 2026-11-09. Verified with [`scripts/verify-db-disk-full.py`](../../scripts/verify-db-disk-full.py) against the native release binary. The drill fills **PostgreSQL's** disk while GitHearth runs; repository storage running out of space is drilled separately (s8 in `scripts/verify-m4.py`).

The harness subclasses the M2 harness in [`scripts/verify-m2.py`](../../scripts/verify-m2.py) and overrides only the database container:
- PGDATA lives on a **96 MiB tmpfs**, and the harness refuses to fill any filesystem larger than 256 MiB, so the host disk is never touched.
- The filler is written from inside the PostgreSQL container until `df` shows 0 bytes available.
- Every container, temporary directory and server process is removed on exit.

## Environment

| Item | Value |
| --- | --- |
| Source revision | `9c4971d` (main) plus this branch |
| Server | `githearth-server` release build, default configuration (5 s statement timeout, 3 s lock timeout) |
| PostgreSQL | `postgres:17-bookworm`, PGDATA on `--tmpfs ...:size=96m` |
| OS / kernel | Linux 7.0.0-42-generic x86_64, glibc 2.39 |

## Command

| Command | Result |
| --- | --- |
| `python3 scripts/verify-db-disk-full.py --report <path>` | **Passed, 4/4 scenarios.** Afterwards no container or server process from the run remained. |

## Observed behaviour

| Step | Observed |
| --- | --- |
| Baseline | Install, organization, private repository, push and issue all succeed. |
| Fill | The tmpfs is 100% used, and PostgreSQL reports `could not extend file ...: No space left on device`. |
| Create a comment while full | **503**, after about 1 s, with no hang. |
| Create an issue while full | **201**. The small insert still fit in an existing page, so a full disk doesn't make every write fail. |
| Push while full | **Accepted** in 0.19 s, and the ref moved on disk. Repository storage is a separate filesystem; the push's metadata write happens later, in replay. |
| Reads while full | Listing issues returns 200, and clone and `ls-remote` succeed. |
| `/health/live`, `/health/ready` | **200 and 200.** Readiness doesn't detect that PostgreSQL can't write. |
| `githearth-server doctor` | **Exit 0, all checks `ok`.** Its `disk` check measures the storage root's filesystem, not PostgreSQL's. |
| Recover (delete the filler, no GitHearth restart) | Writes work again at once (a new issue in 5 ms, and a new push). The **push accepted while the disk was full appears in `ref_updates`**, so the journal and database agree; the drill waits up to 60 s. `githearth check` exits 0, and `doctor` exits 0. |

## Findings

- GitHearth stays up and recovers by itself: no crash, no hang, no restart needed, and nothing is lost.
- **Neither readiness nor `doctor` signals a full database disk.** GitHearth can't see a remote PostgreSQL server's filesystem, so **monitor PostgreSQL's disk separately**, for example with the database host's own disk alerting. Readiness stays 200 while some writes fail with 503.
- The failure is partial: writes that need new pages fail, and small ones may still succeed.

## Not covered

- PostgreSQL's WAL filling up separately from the data directory, and a PostgreSQL crash or restart caused by a full disk.
- A long-running full disk under load.
- A remote PostgreSQL on a different host.
