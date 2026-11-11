# Audit IP retention

The `githearth audit` subcommands let you manage IP retention in the
tamper-evident audit log.  Events created since migration 0022 carry a per-row
HMAC digest of the client IP instead of hashing the raw IP into the chain.
The raw IP stays in the row (`client_ip` column) so operators can look it up
until the retention window expires.  Once the IP is pruned and its HMAC key is
deleted, the digest alone cannot be reversed to the original IP.

## Important: the digest is reversible by anyone holding the key

IPv4 has only 2^32 addresses.  Anyone who possesses the HMAC key can brute-force
the digest by HMAC-ing every possible IPv4 address and comparing the result.
**Unlinkability starts only when the key is dropped.**  Before that, the IP is
still discoverable by anyone with access to the key rows (`audit_ip_keys`).

## Backups still contain raw IPs and old keys

Backup retention bounds IP retention.  As long as a backup snapshot of the
database still exists:
- raw `client_ip` values that were pruned are still present, and
- key rows that were deleted are still present.

If your backup retention is longer than your IP retention window, prune and
drop on the live database will not remove the IPs or keys from the backups.
Set backup retention to match your IP retention requirements.

## Suggested order

1. **`prune-ips`** — set `client_ip = NULL` on rows older than the retention
   window (rows that carry a digest).
2. **`rotate-ip-key`** — insert a new HMAC key, so new events use it and the
   old key can be dropped once no row still references it with a non-NULL IP.
3. **`drop-ip-keys`** — delete keys older than the retention window that are
   no longer in use (and that are not the newest key).

Running `drop-ip-keys` before `prune-ips` means keys that still have
`client_ip IS NOT NULL` rows will be skipped: pruned IPs must be shredded
first, then the keys can be dropped.

## Commands

### `githearth audit prune-ips --older-than <DAYS> [--dry-run]`

Sets `client_ip = NULL` on rows that have a digest, are older than DAYS, and
still have a raw IP.  Rows without a digest (legacy v1/v2) are never touched
because the trigger refuses to change them.

- Processes rows in batches of 10,000 per transaction.
- Prints how many rows were cleared and how many legacy rows still have a raw
  IP but no digest.
- `--dry-run` prints the same counts without writing.
- DAYS must be >= 1.

### `githearth audit rotate-ip-key [--dry-run]`

Inserts a new key row with 32 random bytes.  New events will use this key
because `current_or_new_ip_key` picks the most recent `audit_ip_keys` row.

- Prints the new key id, never the key bytes.
- `--dry-run` prints the id it would create without writing.

### `githearth audit drop-ip-keys --older-than <DAYS> [--dry-run]`

Deletes key rows older than DAYS, with two safeguards:

- **Refuses to drop the newest key** — the key most recently created is always
  skipped, even if it is old enough.  If deleting it would be the only action,
  the command exits non-zero without modifying anything.
- **Skips keys still referenced** by rows with `client_ip IS NOT NULL` —
  dropping those keys would shred nothing because the raw IP is still
  accessible through them.

Prints which keys were dropped, which were skipped as the newest, and which
were skipped because they are still in use.

## Running from cron or a systemd timer

### cron

```
# Run daily at 03:00 UTC: prune IPs older than 90 days, rotate the key monthly,
# and drop keys older than 90 days that are no longer in use.
0 3 * * * /usr/local/bin/githearth --config /etc/githearth/config.toml audit prune-ips --older-than 90
0 3 1 * * /usr/local/bin/githearth --config /etc/githearth/config.toml audit rotate-ip-key
0 3 * * * /usr/local/bin/githearth --config /etc/githearth/config.toml audit drop-ip-keys --older-than 90
```

### systemd timer

Create three service units (`githearth-prune-ips.service`,
`githearth-rotate-ip-key.service`, `githearth-drop-ip-keys.service`) and
corresponding timer units.  The timer for `rotate-ip-key` should be
`OnCalendar=monthly`; the prune and drop timers should match your retention
window cadence.

Example `githearth-prune-ips.service`:

```ini
[Unit]
Description=GitHearth audit IP pruning
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/githearth --config /etc/githearth/config.toml audit prune-ips --older-than 90
User=githearth
```

Example `githearth-prune-ips.timer`:

```ini
[Unit]
Description=Daily GitHearth IP pruning

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

## Verification

After pruning and dropping keys, verify the audit chain is still intact:

```sh
# Via the API (if the server is running):
curl -H "Authorization: Bearer <token>" https://<host>/api/v1/organizations/<slug>/audit/verify

# Or check the site chain directly (only with database access):
psql -c "..."   # or run githearth migrate first, then verify from the application
```

The chain hashes never contained the raw IP (only `key_id || digest` for v3
rows), so pruning and dropping have no effect on chain integrity.
