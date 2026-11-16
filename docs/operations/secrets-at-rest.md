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

## Per-account and per-organization data keys (crypto-shredding)

M11a adds one data key per account and one per organization. A secret that
belongs to an account or organization — an authenticator seed, a reset secret,
an OIDC client secret, any encrypted column the identity slice stores — is
encrypted under a fresh data key, and that data key is wrapped by the owner's
key. The owner key is itself wrapped by the installation key.

**The reference.** The owner row carries the key id:
`accounts.data_key_id` and `organizations.data_key_id` (the identity-schema
lane owns those columns). The id is derived deterministically from the row:
SHA-256 over `domain || kind || 0x00 || 16 raw UUID bytes` (`domain` is
`githearth-owner-data-key-v1\0`), with the UUID version and variant bits set —
`UUIDv5`-shaped, but SHA-256 rather than SHA-1, and over binary bytes rather
than a `"kind:uuid"` string. A restore therefore needs no lookup table and two
installations cannot mint different ids for the same row. The exact function is
`githearth_server::data_keys::id_for` (or the owner-kind wrappers
`account_data_key_id` / `organization_data_key_id`); call it rather than
reimplementing it. `ensure_account_data_key` / `ensure_organization_data_key`
create the key and return that id; `verify_reference` checks a stored value.
`secret_data_keys.owner_key_id` names the same id for every secret the key
protects.
The column is informational and is never the shred input: shredding is driven
by the owner, so a wrong or stale reference cannot delete another owner's key.

**Where the key lives.** Owner keys are **not** in PostgreSQL and **not** in an
archive. They live in `data_key_directory` (environment
`GITHEARTH_DATA_KEY_DIRECTORY`), which defaults to a `data-keys` directory
beside `installation_key_file`. Configuration validation refuses a directory
inside `storage_root`, for the same reason it refuses the installation key
there. Back up this directory with the installation key and keep both off-host:
losing it loses every secret it protects, by design. Shred tombstones live in
the same directory and contain no key material, so they can be kept with the
key custody.

**A shred is only as strong as the custody copies you discard.** The tombstone
and the key-file removal are forward-only: a copy of `data_key_directory` taken
before a deletion has neither the tombstone nor the removal, and restoring that
older copy makes the older archive readable again. Therefore:

- keep only the current custody directory, or a copy that has had every
  shredded key removed;
- never restore a custody copy that predates a deletion;
- when a shred happens, remove the key file from **every** retained copy (an
  old copy has no tombstone to do it for you), and verify the restored archive
  against the current directory, not an older one.

**What a shred does.** Deleting an account or organization destroys its owner
key file and writes a tombstone. Every secret and encrypted column under that
key becomes unreadable:

- in the live database, immediately;
- in **every older backup** whose custody directory is the current one (or was
  copied only after the key was removed): the archive carries the ciphertext
  and the per-secret wrapped data key but never the owner key. An older
  custody copy that still holds the key un-shreds that archive; see the custody
  rule above.

What a shred does **not** do: the pseudonymous account or organization row and
its plaintext profile columns (username, email, display name before deletion)
remain in a backup taken before the deletion until that backup's retention
expires. Retention is the control for those columns; the key is not.

The tombstone is written before the key file is removed, so an interrupted
shred is still a shred. A later attempt to create a key for a shredded owner is
refused: deletion is permanent.

**The honest limits.**

- A **pre-key backup** — one taken before the account and organization keys
  existed, where every secret was wrapped directly by the installation key —
  cannot be shredded at all. Deleting a key has nothing to destroy in it, and
  retention is the only control for that archive.
- A secret written for an account that never had an owner key (a pre-M11a
  secret) is also outside the shred until the first owner-key write. Deleting
  such an account writes a tombstone that records `had_key = false` rather than
  pretending the old secret was destroyed.
- Plaintext profile columns are not encrypted, so they are not shredded either;
  see the paragraph above.
- Key custody is the operational risk: destroying a key is unrecoverable data
  loss by design, exactly like losing the installation key.
- A retained custody copy that predates a deletion un-shreds the archives it
  can still open, because that copy has neither the tombstone nor the removal.
  Retention of `data_key_directory` copies, not the tombstone, bounds a shred
  against an older archive; follow the custody rule above.

**Backup and restore.** The manifest lists each owner key as `{key_id,
owner_kind, owner_id, kek_id, algorithm, wrap_method, shredded, had_key}`; it
never contains the key or its wrapped form. A pre-key archive has an empty
`data_keys` list. `restore` requires the custody directory to be at the path in
the archived configuration or at `GITHEARTH_DATA_KEY_DIRECTORY`, and:

- a key **present** opens normally;
- a key **missing with a tombstone** is a completed shred: restore refuses by
  default and names the key ids, because publishing a database with unreadable
  secrets must be intended; `--accept-shredded` restores, reports the count,
  and the affected secrets stay unreadable;
- a key **missing without a tombstone** is a lost key: restore fails before it
  publishes anything, because silently starting with unreadable secrets is
  what the envelope forbids.

**Doctor and status.** `githearth-server secrets status` and `doctor` list
shredded owner keys separately from damaged ones and fail on a missing key. A
shredded key does not make rotation incomplete.

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
   because each row names the key that wrapped it. It also re-wraps each
   per-account and per-organization key in `data_key_directory` under the new
   installation key; the owner key itself and the secret ciphertext do not
   change.
4. `githearth-server secrets status` prints `rotation complete` and exits zero
   only when no data key is left under the old key or an older wrap method (a
   renamed key descriptor can leave rows whose key id is current but whose
   method is not; `rotate` re-wraps those too). Until then, keep both keys,
   and `backup` refuses rather than write an archive that restores only with a
   key the operator was not told to keep.
5. Remove `installation_key_previous_file`, restart, and destroy the old key.

If the old key is destroyed before step 4, the rows still under it are
unrecoverable.

## Restore

`restore` verifies the archive's installation key id against the destination
key and, after restoring the database, opens every wrapped data key — every row,
not one sample per key id — before it publishes the storage tree or
configuration. Per-account and per-organization keys are opened from
`data_key_directory` too; a missing key without a tombstone fails the restore,
while a tombstoned key makes the restore refuse by default (naming the key ids)
and proceed only with `--accept-shredded`, after which its secrets remain
unreadable. A missing key file, a key file
whose mode lets group or others read it, a wrong key and a key id that does not
match the archive each fail with a distinct message and drop the partially
restored database. Put the key where the archived configuration names it, or
set `GITHEARTH_INSTALLATION_KEY_FILE` (the restore rewrites
`installation_key_file` to that path). Set `GITHEARTH_DATA_KEY_DIRECTORY` for
the owner-key custody directory on a fresh host.

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
- Plaintext profile columns (username, email, display name) are not encrypted
  and are not shredded by deleting a data key; a backup taken before a deletion
  carries them until its retention expires. A pre-key backup cannot be shredded
  at all, and retention is its only control.
