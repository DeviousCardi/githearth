# Phase 5 / M4 qualification drills

Date: 2026-11-08. State: **phase 5 drills pass; M4 is not yet accepted.** The phase-5 completion gate in [08-two-person-roadmap.md](../plan/08-two-person-roadmap.md#delivery-phases) asks that "independent installation, HTTP/SSH agreement, restore and upgrade drills pass". An executable harness runs those drills against the native release binary, and they pass. Several M4 exit items in [04-forge-roadmap.md](../plan/04-forge-roadmap.md#m4--recoverable-single-node-alpha) are still unverified; they are listed under [Remaining scope](#remaining-scope).

The harness found one real defect: SSH pushes larger than about 1 MiB stalled. It was fixed in PR #74 (an unread russh `Channel` buffered every data packet), and the quota drill below now checks SSH and HTTP alike.

Implementation: a delegated OpenCode agent running DeepSeek-V4-Pro. Review, the SSH fix and the recorded run below: Person B.

The harness [`scripts/verify-m4.py`](../../scripts/verify-m4.py) subclasses the M2 harness in [`scripts/verify-m2.py`](../../scripts/verify-m2.py), which it loads with `importlib`. It adds SSH, quota, restore and upgrade drills and follows the same conventions:
- generated secrets are never printed or written into the repository;
- Git credentials reach `git` only through a temporary credential helper;
- every container, temporary directory, server process and old-revision worktree is removed on exit, including on failure, SIGINT or SIGTERM.

## Environment

| Item | Value |
| --- | --- |
| Source revision | `66c0684` (main plus the SSH fix in #74) |
| Upgrade from revision | `9b4a372` (the merge of PR #62, the first release with the startup schema check) |
| Host toolchain | rustc 1.99.1; server and CLI built in release mode by the harness |
| Git client and server-side Git | git 2.43.0 |
| PostgreSQL fixture | `postgres:17-bookworm`, disposable container |
| Docker Engine | 29.1.3 |
| OS / kernel | Linux 7.0.0-42-generic x86_64, glibc 2.39 |
| CPU count | 16 (harness run under a 6 GiB memory cap, `CARGO_BUILD_JOBS=4`) |

## Commands

| Command | Result |
| --- | --- |
| `python3 scripts/verify-m4.py --report <path>` | **Passed, 6/6 scenarios**. Afterwards no `githearth-server` process and no harness container remained. |
| `scripts/ci.sh` | Passed |

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Install from the runbook | Follows [install.md](../operations/install.md) on a fresh database and an empty storage root.<ul><li>`doctor` with an invalid config exits non-zero.</li><li>Before `migrate`, `doctor` reports `fail schema` and `serve` refuses to start.</li><li>Then `migrate`, `bootstrap-admin` (password on stdin) and `ssh-keygen`, which needs no database and does not overwrite on rerun.</li><li>`doctor` exits 0, and `serve` brings up both the HTTP and SSH listeners.</li></ul> | PASS | 0.4 s |
| 2 | HTTP/SSH authorization agreement | Covers four actors: the owner, a read-only member, a non-member and a suspended account. SSH keys are registered through the API.<ul><li>Builds a matrix of actor × repository state (active, archived) × operation (clone, push).</li><li>Asserts that HTTP (Basic auth) and SSH (key) reach the same allow/deny decision in every cell.</li><li>A revoked SSH key and a revoked access token are both denied.</li></ul> | PASS | 33.0 s |
| 3 | Quota | Restarts the server with `GITHEARTH_REPOSITORY_QUOTA_BYTES=1048576`.<ul><li>Ref-only pushes succeed over HTTP and SSH.</li><li>A push of 1.14 MiB of random data is refused over **both** HTTP and SSH, within the timeout, and no ref moves.</li></ul> | PASS | 3.7 s |
| 4 | Backup and fresh restore | Follows [backup-restore.md](../operations/backup-restore.md).<ul><li>Takes a `pg_dump -Fc` and a storage tarball.</li><li>Restores them into a new database and a new storage root; `githearth check` exits 0.</li><li>Clones over HTTP and SSH match the refs from before the backup.</li><li>Spot checks still hold: the owner is allowed, and the non-member is denied on both transports.</li></ul> | PASS | 3.7 s |
| 5 | Upgrade drill | Follows [upgrade.md](../operations/upgrade.md).<ul><li>Builds `9b4a372` in a temporary worktree with its own target directory.</li><li>Installs, migrates and serves that old binary, then creates a repository, a push and an issue.</li><li>New binary: `doctor` reports pending migrations and `serve` refuses to start; after `migrate`, `doctor` exits 0.</li><li>The old data is intact, and new pushes work over HTTP and SSH.</li><li>Afterwards the old binary refuses the newer schema, so upgrades are forward-only.</li></ul> | PASS | 167.8 s (mostly the old release build) |
| 6 | Restart and jobs | Restarts the upgraded server. The `doctor` jobs check is `ok`, and no running job has an expired lease. | PASS | 0.3 s |

## Authorization matrix

The table shows the denial message each client printed. For the suspended account, HTTP fails Basic authentication and SSH fails public-key authentication, which the SSH client prints as `Permission denied (publickey)`. Neither transport reveals whether the repository exists.

| Actor | Repo state | Clone (HTTP / SSH) | Push (HTTP / SSH) |
| --- | --- | --- | --- |
| owner | active | allowed / allowed | allowed / allowed |
| owner | archived | allowed / allowed | denied / denied |
| read-only member | active | allowed / allowed | denied (permission) / denied (permission) |
| read-only member | archived | allowed / allowed | denied / denied |
| non-member | active | denied (not found) / denied (not found) | denied (not found) / denied (not found) |
| non-member | archived | denied (not found) / denied (not found) | denied (not found) / denied (not found) |
| suspended | active and archived | denied (authentication) / denied (publickey) | denied (authentication) / denied (publickey) |
| revoked access token | active | denied (authentication) / — | — |
| revoked SSH key | active | — / denied (publickey) | — |

## Remaining scope

The harness does **not** cover the following M4 exit items:

- **Independent installation.** The harness automates the runbook. An install by a person who has not seen the code, on a clean Linux host, is still to be done.
- **TLS ingress and session/TLS defaults.** The harness uses plain HTTP on loopback.
- **Disk-full behaviour.** Not simulated.
- **Off-host backup.** The backup and restore run on the same host.
- **Controlled uninstall.** No runbook or drill yet.
- **Bounded resources and published hardware minimums.** The M2 harness measured memory; this run did not.
- **License files and notices, and accessibility review.** Not part of this harness.
- **SHA-256 repositories, public/internal visibility, and online (non-maintenance-window) backup.** Not exercised.
