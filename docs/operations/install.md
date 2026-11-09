# Installation

See [upgrade.md](upgrade.md) for the upgrade runbook and
[uninstall.md](uninstall.md) for removal.

## Preflight check

Before running `githearth-server serve`, validate your setup:

```
githearth-server doctor --config /path/to/config.toml
```

An invalid configuration file makes the command exit 1 with the validation error
before any check runs. One such error: `storage_root` must be short enough (at most
84 bytes) for the hook socket `<storage_root>/run/hook-XXXXXXXX.sock` to fit the
107-byte Unix socket path limit. The checks below are only reached when the configuration
is valid.

This runs seven read-only checks:

- **git** — verifies Git ≥ 2.38 is available
- **storage** — checks the storage root exists, is a writable directory, is not a symlink, is not world-writable, and every subdirectory (`repositories/`, `tmp/`, `trash/`, `orphans/`) that already exists is a real writable directory
- **disk** — reports free space on the filesystem holding the storage root; warns below 10% free or below twice `git_push_limit_bytes`
- **database** — connects to PostgreSQL and verifies connectivity
- **schema** — compares applied migrations against the binary; detects pending, failed, or tampered migrations
- **lifecycle** — warns about repositories stuck in `creating` or `deleting` past the reconcile grace period (the reconciler retries them)
- **jobs** — warns about dead jobs; warns about late pending jobs; fails when running jobs have an expired lease, meaning the runner isn't running

When the database cannot be reached the `schema` and `lifecycle` checks are
skipped (`skip <name>: database unavailable`) rather than counted as failures,
so one outage counts as one failure, not three.

Exit code is 0 when all checks pass (warnings and skips are allowed), 1 otherwise.
The last line prints a summary: `N checks, F failed, W warnings, S skipped`.

### Example output

```
ok   git: 2.45
ok   storage: /data/githearth/repositories writable
ok   disk: 47.82 GiB free
ok   database: connected
ok   schema: latest v12
ok   lifecycle: no stuck repositories
ok   jobs: ok
7 checks, 0 failed, 0 warnings, 0 skipped
```
