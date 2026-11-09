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
   The `--exclude` pattern is anchored with `./` so it matches only the
   top-level `run/` directory and does not drop paths like `refs/heads/run`.

   ```sh
   tar -czf storage-$(date +%Y%m%d-%H%M).tgz \
     -C <storage-root> \
     --exclude ./run \
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

4. **Verify** the restored storage with the read-only check command.
   Run this with no pushes in flight (ideally with the server stopped) so an
   in-flight journal append is not reported as a torn tail.  If a torn tail is
   reported after restore, it is harmless: every reconciler pass (including the one at
   startup) opens each active repository's journal, and `Journal::open`
   truncates a torn final frame.  Start the server once, then rerun `check`.

   ```sh
   githearth check <storage-root>
   ```

   A zero exit status and no problem lines means every repository's objects
   and journal are intact.

5. Do a representative clone to confirm the service answers:

   ```sh
   git clone https://<host>/<org>/<repo>.git
   ```

## Off-host copy

A backup stored on the same host does not help when the host fails.
Copy the artifacts to a separate host, verify them there, and restore
from that copy at least once to prove the round-trip works.

### How it works

Three files must leave the primary host:

1. the database dump (see *Procedure* above),
2. the storage tarball, and
3. a `SHA256SUMS` manifest of both so integrity can be verified at the
   far end without trusting the network.

### Copy to the backup host

On the backup host, create a dedicated unprivileged user (for example
`githearth-backup`, not Debian's built-in `backup` system account) that owns
only the backup directory, such as `/srv/backups/githearth/`. On the primary
host, generate a key for it:

```sh
ssh-keygen -t ed25519 -N '' -f /etc/githearth/backup-key   # private key, mode 0600
```

Add `/etc/githearth/backup-key.pub` to that user's `authorized_keys`. Prefix
the line with `restrict` to disable forwarding and PTYs:

```
restrict ssh-ed25519 AAAA... githearth-backup
```

The key must still be able to run commands, because the upload uses `rsync`
and the check runs `sha256sum -c` on the backup host. Don't use a
`command="rsync --server --sender"` restriction: that only permits downloads.

Generate the checksum manifest alongside the dump and the tarball:

```sh
sha256sum githearth-YYYYMMDD-HHMM.dump storage-YYYYMMDD-HHMM.tgz > SHA256SUMS
```

Push all three with `rsync` (or `scp`):

```sh
rsync -e "ssh -i /etc/githearth/backup-key -o StrictHostKeyChecking=yes" \
  githearth-YYYYMMDD-HHMM.dump \
  storage-YYYYMMDD-HHMM.tgz \
  SHA256SUMS \
  githearth-backup@backuphost.example.com:/srv/backups/githearth/
```

Verify the manifest on the remote side:

```sh
ssh -i /etc/githearth/backup-key \
  githearth-backup@backuphost.example.com \
  "cd /srv/backups/githearth && sha256sum -c SHA256SUMS"
```

An `OK` line is printed for every file.  If any file says `FAILED`,
delete the remote copy, recheck the dump/tarball on the source, and
repeat the upload.

### What else must be kept off-host

Secrets that are not inside the dump or storage tarball:

- **Configuration file** — the TOML file at the path passed to `--config`
  (or the environment variables).  It often contains database credentials
  and host-key paths.  Treat it as a secret and copy it to a safe off-host
  location.
- **SSH host key**: the file at `GITHEARTH_SSH_HOST_KEY_PATH` (there is no
  default). If it lies inside the storage root, as in the container image
  (`/var/lib/githearth/ssh_host_key`), the storage tarball already contains
  it. Otherwise copy it separately. Without it, every client sees a new host
  key after a restore, and `StrictHostKeyChecking` blocks their connections.
  It is a private key: store it encrypted.

### Retention

Keep N most-recent generations (e.g. `N=7` for a daily backup — one
week).  Prune: keep the backup directory sorted by date, delete anything
past the N-th most recent generation.

### Restoring from the off-host copy

1. Fetch the three files back to the (new) primary host:

   ```sh
   rsync -e "ssh -i /etc/githearth/backup-key" \
     githearth-backup@backuphost.example.com:/srv/backups/githearth/SHA256SUMS \
     githearth-backup@backuphost.example.com:/srv/backups/githearth/githearth-YYYYMMDD-HHMM.dump \
     githearth-backup@backuphost.example.com:/srv/backups/githearth/storage-YYYYMMDD-HHMM.tgz \
     /tmp/githearth-restore/
   ```

2. Verify the manifest locally:

   ```sh
   cd /tmp/githearth-restore && sha256sum -c SHA256SUMS
   ```

3. Follow the *Restore* section above, starting from step 1
   (`createdb` / `pg_restore`).

## What `check` does NOT verify

- Database relationships, permissions, or metadata consistency (that is a
  separate database-level restore test).
- Whether the storage snapshot is consistent with the database dump — the
  journal reconciler handles that.
- Rotated/archived journals (`journal.<first>`); `verify` reads only the live
  `journal` file.
- Search indexes (rebuilt after restore).  Back up configuration and keys separately.
