# Secrets at rest

GitHearth stores very little secret material, but the audit IP key is a
32-byte HMAC key in PostgreSQL, and every stored IP digest is brute-forceable
for IPv4 while that key is plaintext. ADR 0014 §3 defines the envelope that
fixes this; this runbook is the operator's part.

## The key

`installation_key_file` names a file with mode `0600`, owned by the server
account, not a symlink, and **outside** `storage_root` and the backup
directory. Configuration validation refuses to start `serve`, `migrate` or
`backup` when an installation key, its passphrase, the SMTP credential or the
archive key is inside the storage root, because the storage tar would carry it.

```sh
sudo install -d -m 0700 /etc/githearth
sudo -u githearth githearth-server secrets keygen --output /etc/githearth/installation.key
```

Keep a copy off-host. Losing the key makes every stored secret unreadable;
that is the design, not a recovery bug.

The file is either a versioned descriptor (what `secrets keygen` writes) or a
bare 32-byte, 64-hex or base64 key. For a passphrase-derived key:

```sh
printf '%s' "$PASSPHRASE" | sudo tee /etc/githearth/installation.passphrase >/dev/null
sudo chmod 0600 /etc/githearth/installation.passphrase
sudo -u githearth githearth-server secrets keygen --passphrase \
  --output /etc/githearth/installation.key
# configuration:
# installation_key_file = "/etc/githearth/installation.key"
# installation_key_passphrase_file = "/etc/githearth/installation.passphrase"
```

Argon2id v19, 64 MiB, 3 iterations, one lane, 32-byte output, 16-byte salt in
the descriptor. The manifest records only the key id, algorithm and wrap
method.

## Upgrading an existing installation

1. Stop the server.
2. Generate the key outside `storage_root` (above).
3. Set `installation_key_file` in `config.toml` (or export
   `GITHEARTH_INSTALLATION_KEY_FILE`).
4. Run `githearth-server migrate`. It applies 0056 and encrypts every plaintext
   audit IP key in 100-row transactions. With no key it stops before touching
   a row; supply the key and re-run.
5. Start `serve`. A wrong or missing key names the key id and refuses to listen.
6. Take a backup. The manifest is format 3 and records the installation key id,
   never the key.

An interrupted migration leaves every row readable by either the old plaintext
column or the new envelope; re-run it. Once a row is encrypted, losing the key
makes that row unrecoverable.

`migrate` and `serve` re-count the plaintext rows after the backfill and refuse
if any remain (for example because another `serve`, `migrate` or `backup` held a
row lock). Stop every other writer and re-run; do not start serving or take a
dump on the strength of "schema migrated". If a plaintext row appears at use
after start-up, the request fails with `installation key unavailable` rather
than returning the key; stop every writer and re-run `migrate`.

After a successful migration, run:

```sql
VACUUM (FULL, ANALYZE) audit_ip_keys;
```

The `SET key = NULL` updates leave dead tuples and WAL that still contain the
plaintext key until vacuum. A logical `pg_dump` excludes them, but a physical
base backup or raw data-directory copy taken before vacuum can still contain it.
U28 covers `pg_dump` and this binary's archives, not physical base backups.

## Rotation

1. Generate a new key: `secrets keygen --output /etc/githearth/installation.new.key`.
2. In configuration set `installation_key_file` to the new key and
   `installation_key_previous_file` to the old one. Restart `serve`.
3. Run `githearth-server secrets rotate`. It re-wraps 256 data keys per
   transaction without decrypting any secret; in-flight requests keep working
   because each row names the key that wrapped it.
4. `githearth-server secrets status` prints `rotation complete` and exits zero
   only when no data key is left under the old key. Until then, keep both keys,
   and `backup` refuses rather than write an archive that restores only with a
   key the operator was not told to keep.
5. Remove `installation_key_previous_file`, restart, and destroy the old key.

If the old key is destroyed before step 4, the rows still under it are
unrecoverable.

## Restore

`restore` verifies the archive's installation key id against the destination
key and, after restoring the database, opens every wrapped data key — every row,
not one sample per key id — before it publishes the storage tree or
configuration. A missing key file, a key file
whose mode lets group or others read it, a wrong key and a key id that does not
match the archive each fail with a distinct message and drop the partially
restored database. Put the key where the archived configuration names it, or
set `GITHEARTH_INSTALLATION_KEY_FILE` (the restore rewrites
`installation_key_file` to that path).

## What is not encrypted

- SMTP credentials are an operator file (`smtp_password_file`), never a
  database value or a TOML value.
- The SSH host key is an operator file and is intentionally carried in the
  encrypted archive because restore needs it.
- `database_url` in `config.toml` is operator-owned and is carried in the
  encrypted archive.
- Password and token columns are one-way verifiers or SHA-256 digests of
  high-entropy tokens; the plaintext never reaches the database.
- Raw client IPs are personal data, not credentials, and are pruned by audit
  retention.
