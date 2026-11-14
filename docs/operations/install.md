# Installation

## Install

### Docker Compose (recommended for a first install)

Follow the repository [quick start](../../README.md#quick-start-docker-compose).
It generates `.env`, starts PostgreSQL, migrates, generates the SSH host key,
waits for `/health/ready`, and then bootstraps the first administrator. Data
lives in Compose volumes; `docker compose down` keeps it and
`docker compose down --volumes` deletes it.

### Direct binary on Linux

Prerequisites: a Linux host, **Git ≥ 2.40**, `curl`, `openssl`, Python 3,
`openssh-client` (for SSH-signed commit verification), and a local PostgreSQL 17
(or a reachable server). The Docker image ships Git 2.47 and `openssh-client`.

#### One command: `setup`

`githearth-server setup` takes a clean host with a reachable PostgreSQL to a
`doctor`-clean installation. It performs the manual steps below in one run:

1. writes the configuration file (mode `0600`) from its flags and safe defaults;
   an existing file must be a regular file owned by the current user that group
   or others cannot read, otherwise the run stops and prints the mode it
   actually read;
2. creates the storage root and the host key's directory (mode `0700`), refusing
   a path that is group- or world-writable or owned by another user;
3. generates the SSH host key (Ed25519, mode `0600`) when it is absent;
4. runs migrations;
5. creates the first administrator, `admin`;
6. runs the `doctor` checks and exits non-zero if any fails.

```bash
# The database URL is never a command argument. Put it in the environment
# (preferred), or pipe it on stdin for a first install. The administrator
# password is piped on stdin as well: setup never prompts, because a terminal
# would echo what is typed. Piped input is not printed.
export GITHEARTH_DATABASE_URL='postgres://githearth:PASSWORD@127.0.0.1:5432/githearth'
read -r -s -p 'New administrator password (12+ bytes): ' ADMIN_PASSWORD; echo
printf '%s' "$ADMIN_PASSWORD" | githearth-server setup --config-path /etc/githearth/config.toml
unset ADMIN_PASSWORD

githearth-server serve --config /etc/githearth/config.toml
curl --fail http://127.0.0.1:8080/health/ready
```

Flags:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config-path PATH` | `/etc/githearth/config.toml` | configuration file to write; the global `--config PATH` is accepted too, but not together with this flag |
| `--storage-root PATH` | `/var/lib/githearth/repositories` | Git storage root, created mode `0700` |
| `--http-port PORT` | `8080` | HTTP listener port on `127.0.0.1` |
| `--ssh-port PORT` | `2222` | SSH listener port on `127.0.0.1`, recorded as `ssh_bind` |
| `--ssh-host-key-path PATH` | `<config dir>/ssh_host_key` | host key file and its directory |
| `--no-ssh` | off | install without the embedded SSH server |

`--http-port` and `--ssh-port` must differ; `setup` refuses a configuration
whose `bind` and `ssh_bind` share a port, both on a first install and when it
resumes from an existing file. The rule compares ports only, so two different
interfaces on the same port are refused as well.

Re-running `setup` is safe and is the documented way to finish an interrupted
run: every step detects completed work, prints `created`, `unchanged` or
`skipped`, and leaves an existing configuration file, host key or administrator
untouched. When `GITHEARTH_DATABASE_URL` is not set, a run with an existing
configuration uses the URL recorded in it and reads nothing from stdin, so a
resumed install needs only the password (and none once the administrator
exists). A run that fails names the step that stopped it and lists the steps
already completed; fix the reported problem and run the same command again.

Once the configuration file exists it is the source of truth: its settings are
used for the remaining steps, so an operator's later edits (`bind`,
`secure_cookies`, `storage_root`, proxies) are kept and a repeated run does not
overwrite them. The file is inspected before it is used: a symlink, a file
owned by another user, or a file that group or others can read is refused with
the mode that was actually read, rather than silently fixed or reported as
`0600`. A run whose database URL differs from the one recorded in the file is
refused, so a database is never migrated while the file records another; use
the recorded URL, or edit the file, to change databases.

`setup` keeps most defaults from `githearth.example.toml` and writes
`secure_cookies = true`. A browser on plain HTTP does not send those cookies
back, so a loopback-only development install that browses the UI over
`http://127.0.0.1:8080` must set `secure_cookies = false` in the file after
`setup` (as Compose does) or terminate TLS at a proxy ([tls.md](tls.md)).

The secrets the server needs are the SSH host key, which `setup` generates, and
the per-deployment rows it creates on first use in PostgreSQL (session tokens,
personal access tokens, and the audit IP-retention keys), which never appear in
configuration. The database password belongs to PostgreSQL: create the role and
its database first, as in the manual path below.

#### Manual steps (what `setup` automates)

1. Install `githearth-server` somewhere on `PATH` and make it executable.
2. Create a dedicated database and role, and note the connection URL.
3. Write `/etc/githearth/config.toml` from `githearth.example.toml`; set
   `database_url`, `storage_root`, `ssh_enabled = true` and
   `ssh_host_key_path`, and keep the file mode `0600`.
4. Preflight: `githearth-server doctor --config /etc/githearth/config.toml`.
   Before the first migrate, `schema` fails (and the dependent `lifecycle` and
   `jobs` checks fail or skip) as documented below; the remaining checks should
   be `ok` or `warn`.
5. `githearth-server migrate --config /etc/githearth/config.toml`, then re-run
   `doctor` — it must exit 0.
6. Bootstrap the first administrator (password on stdin only, 12+ bytes):
   `printf '%s' "$PASSWORD" | githearth-server bootstrap-admin --username admin`.
   Repeating this command never changes an existing account.
7. `githearth-server ssh-keygen --config /etc/githearth/config.toml`. It is
   idempotent: an existing host key is kept and the command succeeds.
8. `githearth-server serve --config /etc/githearth/config.toml`, then confirm
   `curl --fail http://127.0.0.1:8080/health/ready` returns 200.

Each command prints only paths, statuses and reasons: a configuration error, a
refused directory or an unreachable database never echoes the database URL or
its password.

See [upgrade.md](upgrade.md) for the upgrade runbook,
[uninstall.md](uninstall.md) for removal, [tls.md](tls.md) for TLS setup,
[audit-retention.md](audit-retention.md) for audit IP retention, and
[hardware.md](hardware.md) for the measured minimum and recommended hardware.

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

- **git** — verifies Git ≥ 2.40 is available
- **storage** — checks the storage root exists, is a writable directory, is not a symlink, is not world-writable, and every subdirectory (`repositories/`, `tmp/`, `trash/`, `orphans/`) that already exists is a real writable directory
- **disk** — reports free space on the filesystem holding the storage root; warns below 10% free or below twice `git_push_limit_bytes`. It does not see PostgreSQL's disk: monitor that separately, because a full database disk leaves `doctor` and `/health/ready` reporting ok while writes fail (see [db-disk-full](../verification/db-disk-full.md))
- **database** — connects to PostgreSQL and verifies connectivity
- **schema** — compares applied migrations against the binary; detects pending, failed, or tampered migrations
- **lifecycle** — warns about repositories stuck in `creating` or `deleting` past the reconcile grace period (the reconciler retries them)
- **jobs** — warns about dead jobs; warns about late pending jobs; fails when running jobs have an expired lease, meaning the runner isn't running

When the database cannot be reached the `schema`, `lifecycle` and `jobs`
checks are skipped (`skip <name>: database unavailable`) rather than counted
as failures, so one outage counts as one failure, not three.

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
