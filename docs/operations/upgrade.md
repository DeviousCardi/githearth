# Upgrade

Before upgrading: read the release notes for the supported upgrade window.  
Upgrades are forward-only; an older binary refuses a newer schema.

## Steps

1. **Back up**
   Follow [backup-restore.md](backup-restore.md) and keep the old binary
   alongside the backup.

2. **Stop the server**
   ```
   systemctl stop githearth-server   # or your process manager
   ```
   No pushes may be in flight. A `warn lifecycle` line from `doctor` later is fine:
   the reconciler retries those repositories when the server starts again.

3. **Install the new binary** — replace the old binary file.

4. **Preflight check**
   ```
   githearth-server doctor --config /path/to/config.toml
   ```
   Expect `fail schema: N pending migrations` when the release adds migrations
   (`ok schema` when it adds none). Every other check must be `ok` or `warn`.
   `serve` refuses to start until the schema matches the binary.

5. **Run the migrations**
   ```
   githearth-server migrate --config /path/to/config.toml
   ```
   Rerun `doctor`; all checks must read `ok` or `warn` (0 failed).

6. **Start and verify**
   ```
   systemctl start githearth-server
   ```
   Verify: log in via the web UI, clone a repository, push a change.  
   Optionally run `githearth check <storage-root>` on the storage (read-only).

## Rollback

Migrations cannot be reversed, and the old binary refuses to start against the
migrated database (`database schema does not match this binary: ... newer than
this binary`). Roll back by restoring the pre-upgrade backup
([backup-restore.md](backup-restore.md)) and running the old binary on it.

## Not covered

- No expand/contract online upgrade (a maintenance window is required).
- No journal or storage format versioning beyond what already exists.
