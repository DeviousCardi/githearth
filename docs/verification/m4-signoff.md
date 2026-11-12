# M4 qualification sign-off

Date: 2026-11-12. State: **evidence gathered; awaiting the independent-installation drill and two reviewer sign-offs.** This document maps every M4 exit item in [04-forge-roadmap.md](../plan/04-forge-roadmap.md#m4--recoverable-single-node-alpha) to its evidence, records the step-by-step independent installation checklist, and collects the two reviewer sign-offs. The executable drills are summarised in [m4-qualification.md](m4-qualification.md) and the specialised verification notes in [tls.md](tls.md), [offhost-backup.md](offhost-backup.md) and [db-disk-full.md](db-disk-full.md).

## Exit items → evidence

Status is one of **verified** (evidence recorded), **pending person** (needs a human on a clean host), or **pending PR** (in review, not merged). Dates are the run dates recorded in each evidence document.

| # | M4 exit item | Evidence (doc) | Script / method | Date | Status |
| --- | --- | --- | --- | --- | --- |
| 1 | Independent user installs from the runbook on clean Linux | [m4-signoff.md](m4-signoff.md) independent-installation checklist below | A person who has not seen the code, on a clean Linux host, using only `docs/operations/` | — | pending person |
| 2 | Restart | [m4-qualification.md](m4-qualification.md) s6 (restart and jobs), s7 (container restart, data persists) | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 3 | Disk-full behavior | [m4-qualification.md](m4-qualification.md) s8 (repository storage) and [db-disk-full.md](db-disk-full.md) (PostgreSQL disk full) | `scripts/verify-m4.py`, `scripts/verify-db-disk-full.py` | 2026-11-08, 2026-11-09 | verified |
| 4 | Bounded resources | [m4-qualification.md](m4-qualification.md) s10 (concurrency, memory, request limits) and "Measured resource use" | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 5 | Off-host backup | [offhost-backup.md](offhost-backup.md) (copy to a separate host and restore a lost primary) | `scripts/verify-offhost-backup.py` | 2026-11-09 | verified |
| 6 | Fresh restore | [m4-qualification.md](m4-qualification.md) s4 and [offhost-backup.md](offhost-backup.md) scenario 4 | `scripts/verify-m4.py`, `scripts/verify-offhost-backup.py` | 2026-11-08, 2026-11-09 | verified |
| 7 | Upgrade | [m4-qualification.md](m4-qualification.md) s5 (forward-only upgrade drill) | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 8 | Controlled uninstall | [m4-qualification.md](m4-qualification.md) s9 (keep-data and full removal) | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 9 | HTTP/SSH authorization agrees | [m4-qualification.md](m4-qualification.md) s2 and the authorization matrix | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 10 | Publish minimum hardware only after measurement | [m4-qualification.md](m4-qualification.md) "Proposed minimum hardware" (RAM 1 GiB, 1 core) — measurement gate met; actual publication still pending | measurement from s10 on one host | 2026-11-08 | verified |
| 11 | Resolve license files/notices before external distribution | `docs/operations/third-party-notices.md`, `THIRD_PARTY_NOTICES`, `LICENSE` (PR #109) | `scripts/generate-third-party-notices.py` + stale check in `scripts/ci.sh` | — | pending PR #109 |
| 12 | Review TLS/session defaults | [tls.md](tls.md) and [tls.md](../operations/tls.md#session-and-tls-defaults) | `scripts/verify-tls.py` | 2026-11-08 | verified |
| 13 | Accessibility | `docs/verification/accessibility.md` (PR #109) | WCAG 2.2 AA basics audit and fixes | — | pending PR #109 |

Items 1, 11 and 13 are the only open exits. Everything else has recorded evidence. Online (non-maintenance-window) backup is explicitly out of scope for M4: backup/restore is maintenance-window-only, as documented in [m4-qualification.md](m4-qualification.md#online-backup-maintenance-window-only).

## Independent installation checklist

A person who has never seen the code follows this on a clean Linux host using only the runbooks in `docs/operations/` ([install.md](../operations/install.md), [backup-restore.md](../operations/backup-restore.md), [upgrade.md](../operations/upgrade.md), [uninstall.md](../operations/uninstall.md), [tls.md](../operations/tls.md), [quota.md](../operations/quota.md), [audit-retention.md](../operations/audit-retention.md)). Record `Result` (PASS/FAIL), `Time` (wall clock) and any `Problems` per step. Config keys and their environment overrides are in `githearth.example.toml` (and summarised in [foundation.md](../foundation.md)).

| Step | Action (commands from the runbooks) | Result | Time | Problems |
| --- | --- | --- | --- | --- |
| 1 | Confirm prerequisites: Linux host, Git ≥ 2.38, `curl`, `openssl`, Python 3, and either Docker Engine with Compose or a local PostgreSQL 17. | | | |
| 2 | Provision PostgreSQL and a dedicated database/user; note the connection URL. | | | |
| 3 | Write `/etc/githearth/config.toml` from `githearth.example.toml`: set `database_url`, `storage_root`, `ssh_enabled = true` and `ssh_host_key_path`. Keep it mode `0600`. | | | |
| 4 | Preflight: `githearth-server doctor --config /etc/githearth/config.toml` — expect `fail schema` (or `database not initialized; run migrate`) before the first migrate, and every other check `ok`/`warn`. | | | |
| 5 | `githearth-server migrate --config /etc/githearth/config.toml`, then re-run `doctor` — expect exit 0. | | | |
| 6 | Bootstrap the admin: `printf '%s' "$PASSWORD" | githearth-server bootstrap-admin --username admin` (password on stdin only). | | | |
| 7 | `githearth-server ssh-keygen --config /etc/githearth/config.toml` — creates the SSH host key; re-running must not overwrite it. | | | |
| 8 | `githearth-server serve --config /etc/githearth/config.toml` — both the HTTP and SSH listeners come up; `curl --fail http://127.0.0.1:8080/health/ready` returns 200. | | | |
| 9 | Create an organization and a private repository through the API, then push and clone over **HTTP** and over **SSH**; confirm the refs agree and a non-member is denied. | | | |
| 10 | Restart the server (`systemctl stop`/`start`); confirm clones still work and `doctor` reports `jobs ok`. | | | |
| 11 | Backup and fresh restore per [backup-restore.md](../operations/backup-restore.md): `pg_dump -Fc` + storage tarball, restore into a new database and storage root, `githearth check <storage-root>` exits 0, and a clone matches. | | | |
| 12 | Off-host copy per [backup-restore.md](../operations/backup-restore.md#off-host-copy): copy the dump, tarball and `SHA256SUMS` to a second host, verify with `sha256sum -c`, then restore from that copy. | | | |
| 13 | Upgrade per [upgrade.md](../operations/upgrade.md): install a newer binary, `doctor` shows pending migrations, `migrate`, `doctor` exits 0, old data is intact and new pushes work. | | | |
| 14 | Uninstall per [uninstall.md](../operations/uninstall.md): try the keep-data variant (data survives) and the full removal (storage root empty, database dropped, ports refuse). | | | |
| 15 | TLS per [tls.md](../operations/tls.md): place the nginx config, confirm `/health/ready` over HTTPS, a push/clone over HTTPS, `Strict-Transport-Security`, `Secure; HttpOnly; SameSite=Lax` cookies, and that plain HTTP to the TLS port is refused. | | | |
| 16 | Record whole-install timings and resource observations (server RSS, DB size, storage size) for the minimum-hardware table. | | | |

After the checklist is complete, record the host OS/kernel, toolchain, Git and Docker/PostgreSQL versions here, and note any problem that blocked a step or any runbook instruction that did not work as written.

## Sign-off

Two reviewers each confirm the exit-item table above and the completed independent-installation checklist. Leave fields blank until the review is performed.

### Reviewer 1

- Name:
- Date:
- Verdict (approve / request changes):
- Notes and problems found:

### Reviewer 2

- Name:
- Date:
- Verdict (approve / request changes):
- Notes and problems found:
