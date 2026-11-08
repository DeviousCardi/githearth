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
| Source revision | `aea70e3` plus this branch (main, the SSH fix in #74, and #75) |
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
| `python3 scripts/verify-m4.py --report <path>` | **Passed, 9/9 scenarios**. Afterwards no `githearth-server` process and no harness container remained. |
| `scripts/ci.sh` | Passed |

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Install from the runbook | Follows [install.md](../operations/install.md) on a fresh database and an empty storage root.<ul><li>`doctor` with an invalid config exits non-zero.</li><li>Before `migrate`, `doctor` reports `fail schema` and `serve` refuses to start.</li><li>Then `migrate`, `bootstrap-admin` (password on stdin) and `ssh-keygen`, which needs no database and does not overwrite on rerun.</li><li>`doctor` exits 0, and `serve` brings up both the HTTP and SSH listeners.</li></ul> | PASS | 0.4 s |
| 2 | HTTP/SSH authorization agreement | Covers four actors: the owner, a read-only member, a non-member and a suspended account. SSH keys are registered through the API.<ul><li>Builds a matrix of actor × repository state (active, archived) × operation (clone, push).</li><li>Asserts that HTTP (Basic auth) and SSH (key) reach the same allow/deny decision in every cell, and that the decision is the expected one (the table below). The non-member must get not-found on both transports.</li><li>A revoked SSH key and a revoked access token are both denied.</li></ul> | PASS | 33.0 s |
| 3 | Quota | Restarts the server with `GITHEARTH_REPOSITORY_QUOTA_BYTES=1048576`.<ul><li>Ref-only pushes succeed over HTTP and SSH.</li><li>A push of 1.14 MiB of random data is refused over **both** HTTP and SSH, within the timeout, and no ref moves. The SSH refusal must come from the server, and the "no ref moved" check requires a working `ls-remote` that lists `main`.</li></ul> | PASS | 3.7 s |
| 4 | Backup and fresh restore | Follows [backup-restore.md](../operations/backup-restore.md).<ul><li>Takes a `pg_dump -Fc` and a storage tarball.</li><li>Restores them into a new database and a new storage root; `githearth check` exits 0.</li><li>Clones over HTTP and SSH match the refs from before the backup.</li><li>Spot checks still hold: the owner is allowed, and the non-member is denied on both transports.</li></ul> | PASS | 3.7 s |
| 5 | Upgrade drill | Follows [upgrade.md](../operations/upgrade.md).<ul><li>Builds `9b4a372` in a temporary worktree with its own target directory.</li><li>Installs, migrates and serves that old binary, then creates a repository, a push and an issue.</li><li>New binary: `doctor` reports pending migrations and `serve` refuses to start; after `migrate`, `doctor` exits 0.</li><li>The old data is intact, and new pushes work over HTTP and SSH.</li><li>Afterwards the old binary refuses the newer schema, so upgrades are forward-only.</li></ul> | PASS | 167.8 s (mostly the old release build) |
| 6 | Restart and jobs | Restarts the upgraded server. The `doctor` jobs check is `ok`, and no running job has an expired lease. | PASS | 0.3 s |
| 7 | Container install | Builds the `Dockerfile` image, which now includes `git`, a storage volume at `/var/lib/githearth`, and SSH on 2222.<ul><li>On a private Docker network with a disposable PostgreSQL and a fresh named volume, it runs `migrate`, `bootstrap-admin` (password on stdin) and `ssh-keygen`, then `serve`, all from the image.</li><li>From the host: login and repository creation through the API; push and clone over HTTP and SSH, with matching refs; `doctor` inside the container exits 0.</li><li>After a container restart, the data in the volume persists and clones still work.</li></ul> | PASS | 7.7 s (image build cached) |
| 8 | Disk full | Runs the image with its storage root on a 32 MiB **tmpfs**. The harness refuses to fill any filesystem larger than 64 MiB, so the host disk is never filled.<ul><li>After a seed push, it fills the tmpfs to about 100 KiB free.</li><li>A 5 MiB push fails cleanly over HTTP and over SSH. The SSH refusal must come from the server, not from a failed connection. No ref moves.</li><li>The server stays ready, `doctor` reports `disk` as not `ok`, and `githearth check` on a copy of the storage exits 0.</li><li>Once the filler is deleted, a new push succeeds and appears in a fresh clone.</li></ul> | PASS | 6.8 s |
| 9 | Uninstall drill | Follows [uninstall.md](../operations/uninstall.md) on the native binary.<ul><li>**Keep-data**: stops the server, replaces the binary and configuration, runs `migrate` then `doctor` (exits 0), and starts the server — all clone refs match the pre-reinstall mirrors and an issue is readable.</li><li>**Full removal**: exports every repository as `git clone --mirror` via the API, verifies mirror refs; stops the server, removes the storage root and drops the database. Asserts storage root is empty, database is gone, and HTTP/SSH ports refuse connections. Mirror exports still clone fine.</li></ul> | PASS | 1.6 s |

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
- **Disk-full on the host filesystem.** s8 fills a tmpfs inside the container; a full host disk under PostgreSQL is not exercised.
- **The `compose.yaml` file itself.** s7 runs the same image and steps with `docker run` (Compose's fixed host ports would clash with other services). The compose file is checked only with `docker compose config`.
- **Off-host backup.** The backup and restore run on the same host.
- **Bounded resources and published hardware minimums.** The M2 harness measured memory; this run did not.
- **License files and notices, and accessibility review.** Not part of this harness.
- **SHA-256 repositories, public/internal visibility, and online (non-maintenance-window) backup.** Not exercised.
