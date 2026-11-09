# Uninstall

**Data-loss warnings are explicit. Read every section before the commands you run.**

## Before you begin

1. **Final backup** — follow [backup-restore.md](backup-restore.md). Keep the
   backup and this binary version together so you can restore later.
2. **Repository mirror export** (optional) — export every repository as a bare
   `git clone --mirror` if you want standalone clones outside GitHearth.
   Loop over repositories listed by the API or read them from the storage root.

## Stop the server

```sh
# systemd
systemctl stop githearth-server   # and disable the unit afterwards

# or Compose
cd /path/to/githearth && docker compose down
```

Wait for the process to exit before continuing.

## Remove the software

```sh
rm -f /usr/local/bin/githearth-server     # binary install
rm -f /etc/githearth/config.toml          # or wherever your configuration is
```

For Compose, `docker compose down` (above) already removed the containers;
`docker image rm` the built image if you no longer need it.

### Delete the repositories

**This destroys every repository and its journal. You must have a working
backup (or the mirror export) before this step.**

```sh
rm -rf /var/lib/githearth                 # GITHEARTH_STORAGE_ROOT
```

The SSH host key is usually inside the storage root; if
`GITHEARTH_SSH_HOST_KEY_PATH` points elsewhere, delete that file too.

### Drop the database

**This destroys all metadata, issues, comments, permissions, and audit records.
You must have a working backup before this step.**

```sh
# Native PostgreSQL
dropdb githearth

# Compose: removes both named volumes, <project>_postgres-data and
# <project>_repositories (the storage root)
docker compose down --volumes
```

## Keep-data variant (software only, leave storage and database)

1. Stop the server (as above).
2. Remove the binary and configuration file:
   ```sh
   rm -f /usr/local/bin/githearth-server
   rm -f /etc/githearth/config.toml
   ```
3. Do **not** delete the storage root or drop the database.

Reinstall later by placing a new (or the same) binary, rewriting your
configuration, then:

```sh
githearth-server migrate --config /path/to/config.toml
githearth-server doctor --config /path/to/config.toml    # must exit 0
```

Start the server and your repositories, issues, and permissions are intact.
Keep the SSH host key as well; a new key makes every client warn that the
host key changed.
The new binary must be the same version or newer (downgrades are not supported).

## What is NOT removed

- Backups you created with [backup-restore.md](backup-restore.md) — these are
  files you placed; delete them yourself when you are done.
- Client-side `git clone` directories on developer machines and CI runners.
- SSH `known_hosts` entries on machines that connected to the uninstalled host.
- Reverse-proxy or TLS configuration in your web server / load balancer.
- Docker images pulled for Compose deployments (`docker rmi` them if needed).
