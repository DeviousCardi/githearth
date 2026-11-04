# Backup and restore

A backup is only proven by restoring it and verifying object integrity
(ADR 0003 "Backup and restore", deployment contract "Recovery and upgrade requirements").

## Procedure

1. **Dump the database** snapshot first, so the storage snapshot that follows
   includes any journal records that reference rows in the dump.

   ```sh
   pg_dump -Fc -f githearth-$(date +%Y%m%d-%H%M).dump
   ```

2. **Take a storage snapshot** — a tar of the storage root excluding `run/`
   (the server's PID and socket directory).  Run this after the dump completes.

   ```sh
   tar -czf storage-$(date +%Y%m%d-%H%M).tgz \
     -C <storage-root> \
     --exclude run \
     .
   ```

   Atomic filesystem snapshots (btrfs, ZFS, LVM) are preferred when available.
   In a single-host deployment with no concurrent writes a tar is acceptable;
   the journal reconciler will still replay any discrepancy on restore.

## Restore

1. Restore the PostgreSQL backup into a fresh cluster:

   ```sh
   createdb githearth
   pg_restore -d githearth githearth-YYYYMMDD-HHMM.dump
   ```

2. Extract the storage snapshot into the new storage root:

   ```sh
   tar -xzf storage-YYYYMMDD-HHMM.tgz -C <storage-root>
   ```

3. Start the server.  At startup the reconciler replays any journal records
   newer than the metadata high-water mark (see ADR 0003 "Recovery").
   Repositories created after the backup will appear under `orphans/`.

4. **Verify** the restored storage with the read-only check command:

   ```sh
   githearth check <storage-root>
   ```

   A zero exit status and no problem lines means every repository's objects
   and journal are intact.

5. Do a representative clone to confirm the service answers:

   ```sh
   git clone https://<host>/<org>/<repo>.git
   ```

## What `check` does NOT verify

- Database relationships, permissions, or metadata consistency (that is a
  separate database-level restore test).
- Whether the storage snapshot is consistent with the database dump — the
  journal reconciler handles that.
- Search indexes (rebuilt after restore). Back up configuration and keys separately.
