# Off-host backup verification

Date: 2026-11-09. State: **the off-host copy procedure in [backup-restore.md](../operations/backup-restore.md#off-host-copy) restores a lost primary.** Verified with [`scripts/verify-offhost-backup.py`](../../scripts/verify-offhost-backup.py) against the native release binary.

The harness subclasses the M2 harness in [`scripts/verify-m2.py`](../../scripts/verify-m2.py). Its **backup host** is a disposable container: `debian:bookworm-slim` with `openssh-server` and `rsync`, built once as the local image `githearth-backup-host:drill` and cached.
- `sshd` listens on a free loopback port and allows only a generated Ed25519 key for an unprivileged `ghbackup` user.
- The client uses a throwaway `known_hosts` with `StrictHostKeyChecking=yes`.
- Every container, temporary directory and server process is removed on exit. The cached image stays for the next run; remove it with `docker rmi githearth-backup-host:drill`.

## Environment

| Item | Value |
| --- | --- |
| Source revision | `586a65e` (main) plus this branch |
| Server | `githearth-server` and `githearth` CLI release builds |
| PostgreSQL | `postgres:17-bookworm` disposable container |
| Backup host | `githearth-backup-host:drill` (from `debian:bookworm-slim`), host network, loopback only |
| Git | 2.43.0 |
| OS / kernel | Linux 7.0.0-42-generic x86_64, glibc 2.39 |

## Command

| Command | Result |
| --- | --- |
| `python3 scripts/verify-offhost-backup.py --report <path>` | **Passed, 5/5 scenarios.** Afterwards no container, volume or server process from the run remained. |

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Fresh install | Migrates and bootstraps on an empty database and storage root, starts the server, and creates an organization and a private repository through the API. | PASS | 0.3 s |
| 2 | Push and issue | Pushes a commit of about 2 MiB over HTTP, waits for the refs, and creates an issue. | PASS | 0.2 s |
| 3 | Off-host copy | Follows the runbook: `pg_dump -Fc`, a storage tarball excluding `./run`, and a `SHA256SUMS` manifest. `rsync` copies all three to the backup host, and `sha256sum -c` passes there over SSH. | PASS | 0.7 s |
| 4 | Lose the primary, restore from off-host | Stops the server, **drops the database and deletes the storage root**, then rsyncs the three files back into a new directory and verifies the manifest locally.<ul><li>Restores into a **new** database and a **new** storage root.</li><li>`githearth check` exits 0, and the server starts on the restored data.</li><li>An HTTP clone's HEAD equals the commit pushed before the backup, and the issue is readable.</li></ul> | PASS | 1.3 s |
| 5 | Tamper detection | Overwrites 8 bytes of the tarball on the backup host; `sha256sum -c` then fails. | PASS | 0.4 s |

## Not covered

- A real second machine and network: the backup host is a container on the same host.
- Retention pruning; the runbook gives a recommendation only.
- Encrypting backups at rest, and off-site key management for the configuration file and SSH host key.
- An online (non-maintenance-window) backup.
