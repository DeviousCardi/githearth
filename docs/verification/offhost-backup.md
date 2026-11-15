# Off-host backup verification

Date: 2026-11-16. State: **the off-host copy procedure in [backup-restore.md](../operations/backup-restore.md#off-host-copy) restores a lost primary.** Verified on **macOS with Docker Desktop**, not only on Linux: [`scripts/verify-offhost-backup.py`](../../scripts/verify-offhost-backup.py) ran to completion, **5/5 scenarios**, and every clause below was exercised by an assertion.

The harness subclasses the M2 harness in [`scripts/verify-m2.py`](../../scripts/verify-m2.py). Its **backup host** is a disposable container: `debian:bookworm-slim` with `openssh-server` and `rsync`, built once as the local image `githearth-backup-host:drill` and cached.
- `sshd` listens on a free loopback port and allows only a generated Ed25519 key for an unprivileged `ghbackup` user.
- The client uses a throwaway `known_hosts` with `StrictHostKeyChecking=yes`.
- Every container, temporary directory and server process is removed on exit. The cached image stays for the next run; remove it with `docker rmi githearth-backup-host:drill`.

## Environment

| Item | Value |
| --- | --- |
| Source revision | `f0c9d63` (main) plus this branch |
| Server | `githearth-server` and `githearth` CLI release builds |
| PostgreSQL | `postgres:17-bookworm` disposable container |
| Backup host | `githearth-backup-host:drill` (from `debian:bookworm-slim`), port published on `127.0.0.1`, `sshd` on `0.0.0.0` inside the container |
| Docker | Docker Desktop 4.97.0 (244584); client and Engine **29.9.2**; containerd v2.4.2; runc 1.5.2; `desktop-linux` context |
| OS / kernel | macOS 26.6 arm64 (Darwin 25.6.0) |
| Git / OpenSSH / Python | 2.50.1 (Apple Git-155) / OpenSSH 10.3p1 / 3.9.6 |

## Command

| Command | Result |
| --- | --- |
| `python3 scripts/verify-offhost-backup.py --report <path>` | **Passed, 5/5 scenarios, 7.1 s wall.** Afterwards no container, volume or server process from the run remained. Free space in the Docker VM was 428 GiB before, during and after every scenario; the macOS host had 82–83 GiB free before and after. |

## Running it on macOS

The backup host's container plumbing is now platform-aware
(`B28Harness._backup_host_network()`):

- **Linux** keeps `--network host` with `sshd` on `127.0.0.1:<port>`, so the
  client reaches the container over the host's loopback.
- **macOS (Darwin)** publishes the port
  (`--publish 127.0.0.1:<port>:<port>`) and `sshd` listens on `0.0.0.0:<port>`.
  Docker Desktop's "host" network is the Linux VM's namespace, not the Mac's:
  a container with `--network host` is invisible to `ssh-keyscan 127.0.0.1` on
  the host, which is where the 2026-11-15 attempt stopped. Publishing forwards
  from the Mac's loopback into the VM.

This is harness plumbing only. The backup, the fence proofs, the manifest and
archive assertions, the off-host copy and the four restore refusals are the
same code on both platforms, unchanged.

On Darwin the harness also moves its working directory under `/tmp` before it
starts, so the restored storage root stays short enough for
`<storage_root>/run/hook-XXXXXXXX.sock` to fit the product's 107-byte Unix
socket limit. macOS's default `$TMPDIR` under `/var/folders/...` produced a
110-byte socket path, and `restore` correctly refused the configuration it had
just written. Linux's `/tmp` is short enough already, so nothing changes there.

## Scenario results

| # | Scenario | What it verifies | Result | Wall time |
| --- | --- | --- | --- | --- |
| 1 | Fresh install | Migrates and bootstraps on an empty database and storage root, starts the server with its SSH listener, and creates an organization and a private repository through the API. | PASS | 1.0 s |
| 2 | Push and issue | Pushes about 2 MiB over HTTP, waits for the refs, and creates an issue. | PASS | 0.8 s |
| 3 | Backup fences and off-host copy | `backup` refuses while `serve` accepts writes; succeeds under the `maintenance` fence and under the `not-running` fence, printing and recording each; the archive is copied to the backup host and its SHA-256 matches there. | PASS | 1.3 s |
| 4 | Restore refusals | Wrong key, missing key, corrupted archive and truncated archive are each refused with a distinct message, and no destination database, storage root, config or host key is created. | PASS | 0.4 s |
| 5 | Disaster and restore | Drops the database, deletes the storage root, fetches the archive back from the backup host, restores, starts the restored installation on the restored data and host key, then clones over HTTP and reads the issue. | PASS | 0.9 s |

## Clause verdicts, with captured output

| Clause | Verdict | Evidence |
| --- | --- | --- |
| Metadata is not applied while `serve` accepts writes | **pass** | `backup` exited 1: `the server at 127.0.0.1:64573 is running and accepting writes; stop it or run githearth-server maintenance start and wait for drained`. |
| Fence kind printed and recorded | **pass** | Printed: `fence: maintenance (server at 127.0.0.1:64573 reported drained with 0 writes in flight (HTTP control endpoint))` and `fence used: maintenance (…)`; also `fence: not-running (HTTP 127.0.0.1:64573 and SSH 127.0.0.1:64572 both refused connections)`. In the manifest: `{"kind": "maintenance", "detail": "server at 127.0.0.1:64573 reported drained with 0 writes in flight (HTTP control endpoint)"}`. |
| Archive encrypted; key id, algorithm and KDF parameters recorded; no key | **pass** | Archive begins `GITHEARTH-BACKUP1`; header `{"cipher": "AES-256-GCM", "kdf": "Argon2id-v19", "kdf_memory_kib": 65536, "kdf_iterations": 3, "key_id": "sha256:e1e504af…", salt, nonce}`; `backup` printed `manifest: key id sha256:e1e504af…, AES-256-GCM, Argon2id wrap; the key is not in the archive`; manifest `encryption` has `algorithm` `AES-256-GCM`, `kdf` `Argon2id-v19`, `wrap` `argon2id-symmetric`, 65536 KiB / 3 iterations, and **no `key` field**; the key file's bytes, the admin and database passwords, and `PGDMP`, `manifest.json`, `config.toml`, `database.dump`, `storage.tar`, `ssh_host_key` do not occur in the archive, and the key does not occur in the manifest. Manifest parts cover `database.dump`, `storage.tar`, `config.toml` and `ssh_host_key`, and name the excluded `run/`, `tmp/`, `hooks/`. |
| Four distinct restore refusals | **pass** | (1) `wrong backup key for this archive (archive key id sha256:e1e504af…); the archive was not changed` (2) `no backup key supplied; pass --key-file or --key-stdin (never an argument)` (3) `backup archive is corrupt: authentication failed at chunk 0 (right key, damaged ciphertext)` (4) `backup archive is incomplete: expected 257024 bytes, found 0`. Each exited 1, created no destination database, and left no storage root, config or host key behind. |
| Off-host copy | **pass** | `scp` to `ghbackup@127.0.0.1:/srv/backups/githearth/b28-maintenance.ghbackup`; `sha256sum` there is `623ba162d4771fd5bfed4ea462dbbe3cab898f5c289e4f9f0ccdd956077ea719`, equal to the local hash; the remote directory holds **one** file of 257427 bytes (`b28-maintenance.ghbackup`), not a dump, a tarball and a checksum list. |
| Disaster restore | **pass** | Database `b28` dropped and the storage root deleted; the archive fetched back from the host and re-hashed; `restore` printed `restore complete` and `database-to-storage consistency verified`, published the config, storage root and SSH host key, and created `b28restore`; the restored server accepted a login, an HTTP clone whose `HEAD` equals the commit pushed before the backup, and the issue `Test issue for off-host restore`. |

## Previous attempt

On 2026-11-15 the same command on the same engine passed `fresh_install` and
`push_and_issue` and then stopped at `backup host sshd not reachable within
30 s`, because `start_backup_host()` used `--network host` and probed
`127.0.0.1:<port>` from macOS. No `backup` process was started, so none of the
clauses above had been demonstrated. A Linux runner container with the Docker
socket bind-mounted was attempted as a workaround and filled the host disk; the
engine's metadata database corrupted (`bbolt: Page expected to be: 145, but
self identifies as 0`) and had to be repaired. That approach is not used and is
not needed: the published-port change above runs the drill on the Mac itself.

## Not covered

- A real second machine and network: the backup host is a container on the same host.
- Retention pruning; the runbook gives a recommendation only.
- Encrypting backups at rest, and off-site key management for the configuration file and SSH host key.
- An online (non-maintenance-window) backup.
