# Installation

## Preflight check

Before running `githearth-server serve`, validate your setup:

```
githearth-server doctor --config /path/to/config.toml
```

This runs seven read-only checks:

- **config** — validates the configuration file  
- **git** — verifies Git ≥ 2.38 is available  
- **storage** — checks the storage root exists, is a writable directory, is not a symlink, and is not world-writable  
- **disk** — reports free space on the filesystem holding the storage root; warns below 10% free or below twice `git_push_limit_bytes`  
- **database** — connects to PostgreSQL and verifies connectivity  
- **schema** — compares applied migrations against the binary; detects pending, failed, or tampered migrations  
- **lifecycle** — warns about repositories stuck in `creating` or `deleting` past the reconcile grace period (the reconciler retries them)  

Exit code is 0 when all checks pass (warnings are allowed), 1 otherwise.
The last line prints a summary: `N checks, F failed, W warnings`.

### Example output

```
ok   config: valid
ok   git: 2.45
ok   storage: /data/githearth/repositories writable
ok   disk: 47.82 GiB free
ok   database: connected
ok   schema: latest v12
ok   lifecycle: no stuck repositories
7 checks, 0 failed, 0 warnings
```
