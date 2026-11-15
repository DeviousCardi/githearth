# Backup and restore

A backup is only proven by restoring it and verifying object integrity
(ADR 0003 "Backup and restore", deployment contract "Recovery and upgrade requirements").

## Procedure

**No write may be acknowledged at any point between the start of the database
dump and the end of the storage snapshot.** Stop the server before step 1 and
start it again only after the storage snapshot is complete. A push that lands
between the dump and the snapshot, or while the snapshot is being taken, can be
acknowledged to the client and missing after a restore. The M4 drill ran these
steps on an **idle** installation, not a stopped one, so its evidence covers
only a quiescent installation; see the correction in
[m4-qualification.md](../verification/m4-qualification.md).

M4.1 replaces this procedure with `githearth-server backup`, which refuses to
run unless it has proved that no writer can acknowledge a commit — either the
server is not running (both its HTTP and SSH bind addresses refuse
connections), or it is in maintenance mode and has drained. M4.2 adds an
encrypted online mode for single-node deployments with a filesystem snapshot.

## M4.1 commands

```sh
# Stop accepting writes and wait until in-flight pushes, API writes and
# background work (merge, reconcile and housekeeping) finish.
githearth-server maintenance start --config /etc/githearth/config.toml
githearth-server maintenance status --config /etc/githearth/config.toml

# Encrypted backup. The key is read from a file or stdin, never an argument.
githearth-server backup --config /etc/githearth/config.toml \
  --output /srv/backups/githearth-20261115.ghbackup --key-file /etc/githearth/backup.key

# On a fresh host, with GITHEARTH_DATABASE_URL naming a database that does not exist.
githearth-server --config /etc/githearth/config.toml restore \
  --archive /srv/backups/githearth-20261115.ghbackup --key-file /etc/githearth/backup.key \
  --storage-root /var/lib/githearth/repositories \
  --ssh-host-key-path /etc/githearth/ssh_host_key

githearth-server maintenance stop --config /etc/githearth/config.toml
```

`backup` prints and records in its manifest which fence it used
(`not-running` or `maintenance`). The archive is encrypted with AES-256-GCM
under an Argon2id-derived key; the manifest records the key id, algorithm and
wrap method, never the key. Both commands refuse a wrong key, a missing key, a
corrupted archive and an incomplete archive with distinct messages. `backup`
re-reads the archive it just wrote, verifies every part, extracts the storage
snapshot and runs the same storage check `restore` runs before it publishes the
output path. `restore` verifies every manifest checksum, the journal position of
every repository and database-to-storage consistency before it publishes
storage, configuration or the host key; a failed restore removes what it
created and leaves the destination untouched.

The manifest records `postgres.dump_lsn`, a single `pg_current_wal_lsn()` value
read beside the dump. It is **advisory only**: it is not the dump transaction's
snapshot position and `restore` does not compare it. Consistency between the
database and the storage snapshot is proved by the per-repository journal
positions, not by the LSN.

The maintenance control endpoint is cryptographically authenticated and is
reachable only from a loopback client, so it is not exposed off-host even when
`bind` is not loopback. A loopback bind (or `0.0.0.0`, which the
client probes on loopback) works for `maintenance start|status|stop` and
`backup`; with an explicit non-loopback `bind`, stop the server and use the
`not-running` fence instead.

Configuration keys: `backup_directory` (default `backups`, used when
`--output` is omitted), `backup_key_file` (default empty; `--key-file`
overrides it) and `maintenance_drain_timeout_seconds` (default 300, the bound
`maintenance start` waits for). The environment names are
`GITHEARTH_BACKUP_DIRECTORY`, `GITHEARTH_BACKUP_KEY_FILE` and
`GITHEARTH_MAINTENANCE_DRAIN_TIMEOUT_SECONDS`.

The manual procedure below remains the reference for what the archive holds
and for older installations; the commands are the supported path from M4.1.

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
   A snapshot taken while the server is stopped is equivalent to a stopped tar.
   A tar taken while the server is running is not a backup: the reconciler
   replays journal records after the metadata high-water mark, which covers a
   push committed *after* the dump, not a push that commits *during* the tar
   after its journal record was already copied.

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

- **Encryption key material**: an installation key or backup passphrase does
  **not** belong with the backup. Keep it on a separate medium and a separate
  access path, so that one stolen archive does not also yield its key. The
  manifest records only a key id, algorithm and wrap method. Losing the key
  means the archive cannot be decrypted; there is no recovery path by design.
  From M4.1 the archive itself is encrypted with this key or passphrase,
  because the configuration file it contains holds the database URL with its
  password and the SSH host key is a part of the same archive.

The M4.1 `backup` command includes the configuration file and the host key in
its checksummed manifest; until then this manual procedure leaves both outside
`SHA256SUMS`, so their integrity is not checked.

### Retention

Keep N most-recent generations (e.g. `N=7` for a daily backup — one
week).  Prune: keep the backup directory sorted by date, delete anything
past the N-th most recent generation.

Backup retention bounds IP retention: raw IPs pruned from the live
database and old keys deleted still exist in backups — see
[audit-retention.md](audit-retention.md).

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
- Whether the storage snapshot is consistent with the database dump. The
  journal reconciler replays records committed after the dump, but it cannot
  recover a write that committed during the tar; that is why the server must
  be stopped (or, from M4.1, why `backup` requires the fence).
- Encryption and key custody. The manual procedure produces plaintext
  artefacts; from M4.1 the archive is encrypted and the manifest names a key
  id.
- Rotated/archived journals (`journal.<first>`); `verify` reads only the live
  `journal` file.
- Search indexes (rebuilt after restore).  Back up configuration and keys separately.
