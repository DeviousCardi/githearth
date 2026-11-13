# Installation, restart, backup/restore, upgrade, uninstall and diagnostics — M4 lab acceptance report

Tester: t15 (owner of `lab15`). Date: 2026-11-14. Primary instance: <http://127.0.0.1:8080> (main `b820454`), left running and untouched. All disruptive drills ran on an isolated second Compose project, `ghm4ops` (HTTP 18080, SSH 12222, PostgreSQL 54330), plus a throwaway project `ghm4r` for the stop/restart drill.

Verdict: **PASS WITH DEFECTS**. The install, backup/restore, upgrade preconditions, uninstall and diagnostics drills pass. One major lifecycle defect (A) breaks the README's documented stop/keep-data path, and two major documentation defects (B, C) mean two documented steps cannot be performed as written.

Cases: **62 run, 54 pass, 7 fail, 1 skipped**.

## Host and image information

| Item | Value |
| --- | --- |
| Host OS | macOS 26.6 (Darwin 25.6.0, arm64), 10 CPU, 16 GiB RAM |
| Docker Engine | 29.9.2 (API 1.56), Docker Desktop, Linux kernel 7.0.14-linuxkit, 8.3 GB VM |
| Docker Compose | v5.6.1 |
| Git client | 2.50.1 (Apple Git-155) |
| PostgreSQL client | psql 17.11 (Homebrew), server `postgres:17-bookworm` |
| Primary app image | `sha256:b40ccb36f6d888153feef16c39f780d7a59a741240c7ff748a32d4a9ded135d5` (`githearth-app@sha256:b40ccb36…`) |
| Second app image | `sha256:9b3dcbabd6ba81df3d1af77c56062a4a28bce6cd6a721f9fd5a091019b81a4bc` (`ghm4ops-app@sha256:9b3dcbab…`), created 2026-11-13T21:09:35Z, 318,964,418 bytes, linux/arm64 |
| Second migrate / ssh-keygen images | `sha256:195957c63a38d81597d933541bd93b8a0f41441c4060a7c1455c6afa610e59f1` / `sha256:f1313744298e46d98d7a404ef73f586ec18d94c6288f6c4066c49f3e56bcffe9` |
| Second volumes | `ghm4ops_postgres-data`, `ghm4ops_repositories`; no primary volume touched |
| Repository state | `git status` before and after: only the pre-existing untracked `crates/githearth-server/examples/`; no tracked change. `target/` was used for a cached CLI build (ignored by Git). |

### Install timing (second instance)

```sh
cd /Users/suniljadav/Aarav/Github/githearth
GITHEARTH_HTTP_PORT=18080 GITHEARTH_SSH_PORT=12222 GITHEARTH_POSTGRES_PORT=54330 \
  docker compose -p ghm4ops up --build --wait
```

Wall clock: **12.52 s** (start 2026-11-13T21:14:59Z, end 21:15:11Z; Compose `real 12.48 s`). Cargo used the cache mount: `Finished release profile [optimized] target(s) in 0.12s`. All four services (`postgres`, `migrate`, `ssh-keygen`, `app`) reached the expected state; `ghm4ops-app-1` and `ghm4ops-postgres-1` were healthy. `curl -fsS http://127.0.0.1:18080/health/ready` returned 200 with an empty body; `/health/live` returned 200.

A second throwaway project (`ghm4r`, ports 18083/12224/54331) built from the same cache in **10.86 s**, confirming the build cache claim.

## Drill commands and timings

| Drill | Command | Result / wall clock |
| --- | --- | --- |
| Bootstrap | `printf '%s' 'ops-admin-password-123' | docker compose -p ghm4ops exec -T app githearth-server bootstrap-admin --username opsadmin` | `administrator created`, id `203c4ae1-280b-43d2-a6ab-63b64c50ee03`, exit 0 (not separately timed; one Argon2 hash) |
| HTTP Git | `git clone http://opsadmin:<password>@127.0.0.1:18080/opsorg/opsrepo.git` | HEAD `89e271f3914d62eb66b6e1d15f97f151665e12a3` |
| SSH Git | `GIT_SSH_COMMAND='ssh -i /tmp/lab15/ops_key -o StrictHostKeyChecking=no -p 12222' git clone ssh://opsadmin@127.0.0.1:12222/opsorg/opsrepo.git` | same HEAD; key fingerprint `SHA256:NWXwzJhW7TRhxGnRsDICU1R4WvuOduJxy0xLsrT0KAE` |
| Restart | `docker compose -p ghm4ops restart app` | exit 0, **0.26 s**; `/health/ready` 200 on the next poll; clones still work; data and SSH host key unchanged |
| Backup | `pg_dump -Fc` + `tar -czf … --exclude ./run .` | dump **0.59 s** (134 KB), tar **3.72 s** (11 KB), SHA256 manifest verified |
| Restore | `createdb ops_restore` + `pg_restore -d ops_restore …`; extract tar to new root | `pg_restore` exit 0; third app instance ready on the second poll; HTTP/SSH clone HEADs match |
| Upgrade precondition | `migrate` twice; `doctor`; pending/newer schema simulations | both `migrate` runs exit 0; schema v40; `doctor` 0 failed; `serve` refuses pending/newer schema |
| Uninstall | `docker compose down`; `docker compose down --volumes` | volumes survive the first, both named volumes removed by the second; ports refuse |

## Results

### A. Install, bootstrap and preflight

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Isolated second project: `up --build --wait` with port overrides | Builds and starts four services; primary untouched | All started healthy in 12.52 s; primary containers still `Up` and healthy | PASS |
| 2 | `/health/ready`, `/health/live`, `docker compose ps` | 200/200; app and postgres healthy | 200/200, both `(healthy)` | PASS |
| 3 | `doctor` on a fresh, uninitialized database | Exit 1; `fail schema: database not initialized; run migrate` | Exit 1; that line present, but also `fail lifecycle: cannot query repositories` and `fail jobs: cannot query jobs` (3 failed) | PASS (see defect E) |
| 4 | `migrate` on the same fresh DB, then `doctor` | Migrate exit 0; doctor exits 0 | Exit 0; `schema: latest v40`; 7 checks, 0 failed, 0 warnings, 0 skipped | PASS |
| 5 | `doctor` on the migrated instance | Exit 0, all 7 checks ok | Exit 0; 7 checks, 0 failed | PASS |
| 6 | `storage_root` too long (152-byte hook socket) | Config validation error before checks, exit 1 | `storage_root is too long: the hook socket path would be 152 bytes, the Unix limit is 107…`, exit 1 | PASS |
| 7 | Non-PostgreSQL `GITHEARTH_DATABASE_URL` | Validation error, exit 1 | `database_url must be a PostgreSQL URL`, exit 1 | PASS |
| 8 | `ssh_enabled=true` with empty host-key path | Validation error, exit 1 | `ssh_host_key_path must be set when ssh_enabled is true`, exit 1 | PASS |
| 9 | Invalid TOML: unknown field, wrong type, missing file | Sanitized error, exit 1, no secret echoed | `invalid TOML configuration: check field names and types` / `cannot read configuration file`; exit 1; `secret` never printed | PASS |
| 10 | Storage root is a regular file (`/etc/passwd`) | `fail storage`, exit 1 | 5 storage failures (root and each subdirectory), 11 checks, exit 1 | PASS |
| 11 | Database unreachable | `fail database` + three `skip`, exit 1 | `fail database: cannot connect…`; `skip schema/lifecycle/jobs`; 1 failed, 3 skipped, exit 1 | PASS |

### B. Bootstrap, API, Git and SSH host key

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 12 | First `bootstrap-admin` with password on stdin | Creates admin, exit 0, password not in argv/log | `administrator created`, id `203c4ae1…`, exit 0, JSON log | PASS |
| 13 | Second `bootstrap-admin` with a different name/password | Refuses, does not overwrite, exit 1 | `an administrator already exists; bootstrap refused without changing credentials`, exit 1 | PASS |
| 14 | Bootstrap validation: username `Ops Admin`, 11-byte password | Both rejected before creating an admin | `username must contain 3..64 lowercase…` and `password must contain 12..1024 bytes`, exit 1 | PASS |
| 15 | Original admin still works after the refused bootstrap | Login 200 and `is_admin: true` | Login 200; `/api/v1/me` `{username:"opsadmin", is_admin:true}` | PASS |
| 16 | Create organization via API | 201 with owner role | 201, `opsorg`, role `owner` | PASS |
| 17 | Create private repository via API | 201, `status: active` | 201, `opsrepo`, visibility `private`, status `active` | PASS |
| 18 | Register ed25519 SSH key | 201 with `SHA256:` fingerprint | 201, fingerprint `SHA256:NWXwzJh…` | PASS |
| 19 | Push and clone over HTTP | Push accepted; clone HEAD matches source | Push OK; clone HEAD `89e271f…` matches source | PASS |
| 20 | Push and clone over SSH | SSH key auth; clone HEAD matches | Push OK; SSH clone HEAD `89e271f…` | PASS |
| 21 | HTTP/SSH/source ref agreement | Identical HEAD and `refs/heads/main` | All `89e271f3914d62eb66b6e1d15f97f151665e12a3` | PASS |
| 22 | `ssh-keygen` rerun | Must not overwrite existing host key | `host key already exists at /var/lib/githearth/ssh_host_key; refusing to overwrite`, exit 1; sha256 and mtime unchanged | PASS (the exit 1 is the root cause of defect A) |
| 23 | `ssh-keygen` with an unreachable database URL | Succeeds without needing PostgreSQL | Refused only because the key already existed; no connection attempted | PASS |
| 24 | Host-key file permissions/checksum | Mode 0600, stable checksum | `-rw-------`, sha256 `34b89c08…`, unchanged across restart | PASS |

### C. Restart

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 25 | `docker compose -p ghm4ops restart app` | Exit 0, short outage | Exit 0, 0.26 s | PASS |
| 26 | Readiness after restart | `/health/ready` 200 | 200 on the first poll | PASS |
| 27 | Persistence after restart | Data, host key, doctor and both Git transports survive | HEAD `89e271f…` over HTTP and SSH; host-key sha256 unchanged; `doctor` 7/0/0/0 (jobs ok) | PASS |
| 28 | `docker compose -p ghm4ops stop app` then `start app` | App restarts | Exit 1: Compose reruns the one-shot `ssh-keygen`, which exits 1 because the key exists; app not started | **FAIL** (defect A) |
| 29 | `docker compose -p ghm4r down` then `up --wait --no-build` on the same volumes | Installation starts again with data intact | Exit 1: `service "ssh-keygen" didn't complete successfully: exit 1`; app container created but stopped | **FAIL** (defect A) |

### D. Backup and restore

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 30 | `pg_dump -Fc` from the second PostgreSQL | Dump succeeds, nonzero size | Exit 0, 134 KB, 0.59 s | PASS |
| 31 | Storage tar per `backup-restore.md` | Excludes top-level `run/`, includes repos and host key | `tar -tzf` contains `repositories/…`, `ssh_host_key`, no `./run`; 11 KB, 3.72 s | PASS |
| 32 | `SHA256SUMS` verification and corruption detection | Both OK; a flipped byte reported `FAILED` | Both `OK`; corrupted copy `FAILED` | PASS |
| 33 | `createdb ops_restore` + `pg_restore` | Restore exit 0 | Exit 0 | PASS |
| 34 | Metadata row counts, source vs restored | Equal | accounts 1/1, orgs 1/1, repos 1/1, ref_updates 2/2, migrations 33/33 | PASS |
| 35 | Extracted repo integrity | `git fsck` clean; log/ref match | `git fsck --full` exit 0; log `89e271f`, `ce31ac8`; `main=89e271f…` | PASS |
| 36 | Documented `githearth check <storage-root>` in the installed image | Command exists and exits 0 | `githearth-server check` → `unrecognized subcommand`, exit 2; no `githearth` binary (`sh: githearth: not found`, 127) | **FAIL** (defect B) |
| 37 | `githearth check` built from current source | Exits 0 with no problems | `checked 1 repositories, 0 problems`, exit 0 (5.64 s cached CLI build) | PASS |
| 38 | Third app instance on restored DB + restored storage | Starts and becomes ready | Ready 200 on the second poll | PASS |
| 39 | Clone restored repo over HTTP | HEAD matches pre-backup | `89e271f…` | PASS |
| 40 | Clone restored repo over SSH; restored host key | HEAD matches; fingerprint equals original | `89e271f…`; both ports serve `SHA256:QIvS6s2YQP9BqQ7N/BYq2k+ZOgImDsaum2FSLA06ako` | PASS |
| 41 | Restored metadata and doctor | Login/list org/repo; doctor exits 0 | Login 200, same admin UUID, org `opsorg`, repo `opsrepo`; `doctor` 7/0/0/0 | PASS |

### E. Upgrade preconditions

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 42 | `migrate` twice on a migrated DB | Both idempotent, exit 0 | Both exit 0; `schema migrated`; no change | PASS |
| 43 | Schema version query | Latest migration reported consistently | `_sqlx_migrations`: 33 rows, max version **40**; `doctor`: `latest v40` | PASS |
| 44 | Pending migration: `doctor`, then `serve` | `fail schema: N pending migrations`; serve refuses | `fail schema: 1 pending migrations; run githearth-server migrate`; serve exit 1 with `database schema does not match this binary: 1 pending migrations…` | PASS |
| 45 | Newer schema: `doctor`, then `serve` | `newer than this binary`; serve refuses | `fail schema: database schema is newer than this binary (version 99999)`; serve exit 1 with the same reason | PASS |
| 46 | `migrate` with unreachable database | Exit 1, sanitized error | `cannot connect to PostgreSQL; check database availability and credentials`, exit 1 | PASS |
| 47 | Full old-binary → new-binary upgrade | Old data survives, new binary migrates | Not run in this drill (no old revision built; `m4-qualification.md` s5 covers it); preconditions in 42–46 pass | SKIP |

### F. Uninstall

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 48 | `docker compose -p ghm4ops down` | Containers/network removed, volumes kept | Containers and network removed; `ghm4ops_postgres-data` and `ghm4ops_repositories` remain | PASS |
| 49 | `docker compose -p ghm4ops down --volumes` after that | Both named volumes removed | Both volumes `Removed`; no `ghm4ops` volume remains | PASS |
| 50 | Post-uninstall state | No containers; ports 18080/12222/54330 refuse | No `ghm4ops` container or network; `nc` reports all three ports closed; curl `000` / "Couldn't connect" | PASS |

### G. Diagnostics

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 51 | Structured request logs | JSON with method, route, status, elapsed, client_ip | e.g. `{"message":"http request","method":"POST","route":"/api/v1/login","status":200,"elapsed_ms":20,"client_ip":"172.19.0.1"}` | PASS |
| 52 | Logs carry no secrets/request bodies | No password, token, DB URL or body | Zero matches for the admin password, `POSTGRES_PASSWORD`, session token, `postgres://`, `Bearer `, or `password` in both app logs | PASS |
| 53 | Audit log lists admin actions | Bootstrap, session, org/repo creation, SSH key, pushes | `audit_events`: `administrator.bootstrapped`, `session.created`, `organization.created`, `repository.created`, `ssh_key.created`, two `repository.pushed` | PASS |
| 54 | `docker compose ps` before teardown | App/postgres healthy | Both `(healthy)` | PASS |
| 55 | PostgreSQL down while app runs | live 200, ready 503 | live 200, ready 503 | PASS |
| 56 | PostgreSQL back up | Ready recovers; clone still works | Ready 200 on first poll; clone HEAD `89e271f…` | PASS |

### H. Docs accuracy

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 57 | README quick start (`up`, health, bootstrap) | Commands work as written | Worked on the isolated project; `.env` generation was not rerun because a valid `.env` already existed | PASS |
| 58 | README stop/keep-data instruction (`README.md:59`) | `docker compose down` keeps data and installation can start again | Data volumes survive, but the next `up` fails in `ssh-keygen` (defect A) | **FAIL** |
| 59 | `docs/operations/install.md` as the install runbook | Contains the install commands (`migrate`, bootstrap, `ssh-keygen`, `serve`) | Contains only preflight/`doctor`; no install procedure (defect C) | **FAIL** |
| 60 | `backup-restore.md` dump/tar/restore procedure | Commands work | `pg_dump`, tar with `--exclude ./run`, `createdb`, `pg_restore` all worked (the separate `check` command is case 36) | PASS |
| 61 | `docs/operations/tls.md` header/XFF table | Matches the running server | Claims nosniff/Referrer-Policy/CSP missing and XFF unconsumed; all are present/consumed (defect D) | **FAIL** |
| 62 | `m4-signoff.md:36` fresh-DB `doctor` expectation | Only `fail schema`; every other check ok/warn | Three failures (schema, lifecycle, jobs) on an empty DB | **FAIL** (defect E) |

## Defects

### A. Compose installation cannot be restarted after `docker compose down`; `start app` also fails

- Severity: **major** (blocks the README's only stop/keep-data instruction; data remains intact and a workaround exists)
- Repro (throwaway project `ghm4r`, same `.env`):
  ```sh
  cd /Users/suniljadav/Aarav/Github/githearth
  export GITHEARTH_HTTP_PORT=18083 GITHEARTH_SSH_PORT=12224 GITHEARTH_POSTGRES_PORT=54331
  docker compose -p ghm4r up --wait --build          # first boot: OK
  docker compose -p ghm4r down                       # README "stop while keeping data": OK
  docker compose -p ghm4r up --wait --no-build       # exit 1
  ```
  Also observed on `ghm4ops`:
  ```sh
  docker compose -p ghm4ops stop app
  docker compose -p ghm4ops start app                # exit 1
  ```
- Expected: after `docker compose down` (which preserves the named volumes), `docker compose up` starts the same installation with its data and host key. `docker compose start app` should not re-run one-shot dependencies, or should tolerate an existing key.
- Actual: `docker compose start app` starts dependencies; the one-shot `ssh-keygen` service runs again, finds `/var/lib/githearth/ssh_host_key`, prints `host key already exists at /var/lib/githearth/ssh_host_key; refusing to overwrite`, and exits 1. Because `app` has `depends_on: ssh-keygen: condition: service_completed_successfully`, Compose reports `service "ssh-keygen" didn't complete successfully: exit 1` and leaves the `app` container created but stopped.
- Evidence: `compose.yaml:31-40` (`ssh-keygen` one-shot, exit 1 on rerun) and `compose.yaml:49-51` (`app` depends on its successful completion); `README.md:59` says "Stop while keeping data with `docker compose down`"; observed `docker compose -p ghm4r ps -a` after failure: `app` = `Created`, `ssh-keygen` = `Exited (1)`, postgres healthy. `/tmp/lab15/down-up-2.txt`.
- Scope: systematic for every Compose install after the first boot. Workaround: `docker compose restart app` (which passed in 0.26 s) or `docker start <app-container>`; deleting the key would rotate the SSH host key. The same non-idempotent `ssh-keygen` exit 1 was observed directly (case 22).

### B. `githearth check` is documented but absent from the container image

- Severity: **major** (the documented post-restore integrity check cannot be run in the supported Compose install)
- Repro:
  ```sh
  docker compose -p ghm4ops exec -T app githearth-server check /var/lib/githearth
  # error: unrecognized subcommand 'check'; exit 2

  docker run --rm --entrypoint sh ghm4ops-app -c 'githearth check /var/lib/githearth'
  # sh: 1: githearth: not found; exit 127

  docker compose -p ghm4ops exec -T app githearth-server --help
  # commands: serve, migrate, doctor, bootstrap-admin, ssh-keygen, audit
  ```
- Expected: `backup-restore.md:58` and `upgrade.md:40` say `githearth check <storage-root>` exits 0 with no problem lines after restore; `m4-signoff.md:43` makes that part of the restore drill.
- Actual: the image builds only `-p githearth-server` (`Dockerfile:14`) and copies only `githearth-server` (`Dockerfile:19`); the `githearth-cli` crate that implements `check` is not installed. Building it from the current source (`cargo build --release -p githearth-cli`, 5.64 s) produces a working `githearth check`: `checked 1 repositories, 0 problems`, exit 0 on the extracted restore.
- Evidence: `/tmp/lab15/cli-help.txt`; restore storage `/tmp/lab15/restore-storage`; rebuilt binary `/Users/suniljadav/Aarav/Github/githearth/target/release/githearth` (ignored build output).
- Scope: every Docker Compose install; docs need either a shipped CLI or a container-specific verification command.

### C. `docs/operations/install.md` does not contain an installation procedure

- Severity: **major** (documentation; M4 exit item 1 asks for an independent install from the operations runbooks)
- Repro/read: `docs/operations/install.md` is 50 lines. Lines 8–20 cover only the `doctor --config` preflight; lines 22–50 describe the seven checks and example output. It never gives the commands to write the configuration, run `migrate`, run `bootstrap-admin`, run `ssh-keygen`, or start `serve`.
- Expected: the file named "Installation" contains the install commands, or explicitly sends the reader to the README quick start. `m4-signoff.md:29` says the independent installer follows `install.md`, and steps 3–8 at `m4-signoff.md:35-40` require those commands.
- Actual: an installer starting at `install.md` cannot install; only the README quick start has `migrate`/bootstrap/serve implicitly via Compose. This is exactly the kind of gap the independent-install exit item is meant to catch.
- Scope: documentation; the Compose path itself works (case 1).

### D. `docs/operations/tls.md` security-header and XFF table is stale

- Severity: **minor** (docs only; the live behavior is better than documented)
- Repro:
  ```sh
  curl -sI http://127.0.0.1:18080/ | grep -Ei 'x-content-type|referrer|content-security'
  # x-content-type-options: nosniff
  # referrer-policy: same-origin
  # content-security-policy: default-src 'none'; style-src 'unsafe-inline'; ...
  ```
  With a trusted proxy configured (`GITHEARTH_TRUSTED_PROXIES=172.19.0.0/16`), a request carrying `X-Forwarded-For: 203.0.113.7` logged `client_ip:"203.0.113.7"`.
- Expected: `tls.md:121-123` says `X-Content-Type-Options: nosniff`, `Referrer-Policy` and `Content-Security-Policy` are **missing**; `tls.md:10-12` says the app does **not** consume `X-Forwarded-For`/`X-Forwarded-Proto`/`X-Forwarded-Host`.
- Actual: all three headers are present on UI/API/git responses, and `trusted_proxies` is implemented (`config.rs`, `client_ip.rs`) and honored. The table should be updated, or the docs should state the current defaults.
- Scope: documentation drift; no runtime failure in this drill.

### E. `doctor` reports three failures on an uninitialized database

- Severity: **minor** (noisy preflight; exit code is still correct)
- Repro:
  ```sh
  docker compose -p ghm4r exec -T postgres createdb -U githearth ops_none
  docker run --rm --network ghm4r_default \
    -e GITHEARTH_DATABASE_URL='postgres://githearth:<pw>@ghm4r-postgres-1:5432/ops_none' \
    ghm4r-app doctor
  ```
- Expected: per `m4-signoff.md:36`, before the first `migrate` expect `fail schema` (or `database not initialized; run migrate`) and every other check `ok`/`warn`.
- Actual:
  ```
  ok   git: 2.39
  ok   storage: /var/lib/githearth writable
  ok   disk: 391.83 GiB free
  ok   database: connected
  fail schema: database not initialized; run migrate
  fail lifecycle: cannot query repositories
  fail jobs: cannot query jobs
  7 checks, 3 failed, 0 warnings, 0 skipped
  ```
  `install.md:32-34` only defines skipping `schema`/`lifecycle`/`jobs` when the database is unreachable; the empty-schema case is undocumented. Re-run on a freshly created empty DB reproduced the same three failures (`/tmp/lab15/doctor-fresh-rerun.txt`).
- Scope: systematic on a fresh database; the command still exits 1, so it does not let a bad install proceed.

## M4 exit-item status

| Item | Status in this drill | Evidence / limits |
| --- | --- | --- |
| 2. Restart | **PASS for `docker compose restart app`; FAIL for the documented stop/keep-data path** | 0.26 s, health 200, HTTP+SSH clones and host key survive, `doctor` jobs ok. `stop app` → `start app` and `down` → `up` fail in `ssh-keygen` (defect A). |
| 3. Disk-full behaviour | **NOT TESTED** (outside this domain) | No filesystem was filled. `m4-qualification.md` s8 and `db-disk-full.md` remain the evidence; not revalidated. |
| 4. Bounded resources | **NOT TESTED** (outside this domain) | No clone burst, memory measurement or request-limit test. Config bounds validation (pool, body, git limits) is present and rejects out-of-range values; not a substitute for s10. |
| 5. Off-host backup | **NOT PROVEN** (single host only) | Same-host dump/tar/manifest/restore passed; no second host was available for the rsync/manifest/restore-from-copy drill. |
| 6. Fresh restore | **PASS** | `pg_dump -Fc` + tar → new `ops_restore` DB + new storage root → third app instance → HTTP/SSH clone HEAD `89e271f…`, metadata counts equal, host key restored, `doctor` 0 failed. The documented `githearth check` step is unavailable in the image (defect B); a CLI built from source passed on the same storage. |
| 7. Upgrade | **PASS for preconditions; full cross-version upgrade not run** | `migrate` twice idempotent (v40, 33 migrations), pending and newer-schema `doctor` + `serve` refusals, sanitized DB-unreachable error. No old binary was built, so the actual old→new data-preserving upgrade is not re-proven here. |
| 8. Controlled uninstall | **PASS** | `down` kept volumes; `down --volumes` removed both named volumes; containers/network gone; ports 18080/12222/54330 refuse. (The Compose keep-data *reinstall* path is broken by defect A.) |
| 10. Minimum hardware publication | **NOT TESTED** (no load/resource measurements) | Only the host inventory was recorded (10 CPU, 16 GiB, 8.3 GB Docker VM). The published 1 GiB/1 core figure was not re-measured. |

## Notes / skipped

- **Test mistake, not a defect:** to simulate a pending migration I deleted the highest `_sqlx_migrations` row from a migrated copy. `migrate` then failed with the sanitized `migration failed; inspect database availability and migration history`, because the DDL for that migration still existed. That is an invalid simulation, not evidence of a broken migration path. The meaningful pending-migration preconditions (`doctor` reports `1 pending migrations`; `serve` refuses) passed on that copy, and normal `migrate` is idempotent on the genuinely migrated DB (case 42).
- The first `ghm4r` attempt used `--no-build`, so its project-specific image names did not exist (`No such image: ghm4r-migrate:latest`). That was a test invocation error; the re-run with `--build` started correctly in 10.86 s.
- Skipped/not covered in this run: full old→new binary upgrade (47); off-host copy to a real second host; TLS nginx termination and cookie-flag verification (tls.md config not deployed); disk-full and bounded-resource drills; non-member denial matrix; native systemd install/stop/start; native uninstall keep-data variant; failed/tampered migration rows; `.env` generation from the README (reused the existing valid `.env`).
- Primary isolation: the primary `githearth-app-1`/`githearth-postgres-1` remained `Up ... (healthy)` before and after the drills; primary `/health/ready` ended at 200. No migrations, restarts or volume operations were run against the primary. All disruptive commands used `-p ghm4ops`/`-p ghm4r` and the alternate ports.
- Scratch evidence is under `/tmp/lab15` (timings, doctor outputs, config-error outputs, logs, backup artifacts `githearth-20261114-0247.dump` / `storage-20261114-0247.tgz` / `SHA256SUMS`, extracted restore tree). The backup artifacts are disposable lab data; the `.env` database password is not reproduced in this report.

## Round 2 — fixed build (2026-11-14)

Re-verification of the four fixes against the fixed build: branch `fix/m4-lab`, commit `7e1ded3` (worktree `…/opencode/wt-m4fix`). All disruptive drills ran on a new isolated Compose project **`ghm4ops2`** (HTTP 18080, SSH 12222, PostgreSQL 54330); the primary (`githearth`, 8080/2222/54329) was never restarted, stopped, migrated or rebuilt by this drill.

Round-2 verdict: **PASS on all four fixes**. Two minor documentation residues remain (F, G); the pre-existing fresh-database `doctor` defect (round-1 E) is still open and the new install procedure now repeats its wrong expectation.

Cases: **40 run, 38 pass, 2 fail, 0 skipped**.

### Fix verdicts

| Fix | Verdict | Evidence |
| --- | --- | --- |
| 1. Second project builds and starts on Debian trixie with 18080/12222/54330 | **PASS** | `up -d --build --wait` exit 0, **11.57 s** warm-cache build; image `sha256:96051261…`, 344,620,332 bytes, linux/arm64; `PRETTY_NAME="Debian GNU/Linux 13 (trixie)"`, `git version 2.47.3`; all four services reached expected state; health 200/200 |
| 2. `down` (keep volumes) then `up` restarts; `ssh-keygen` idempotent | **PASS** | `down` 0.53 s; `up -d --wait` 9.50 s exit 0; `ssh-keygen` exit **0** and log `SSH host key already exists; keeping it`; host-key sha256 unchanged; HTTP+SSH clone HEAD unchanged; `stop app` → `start app` now also exit 0 (1.76 s) |
| 3. `githearth check <storage-root>` exists in the image | **PASS** | `/usr/local/bin/githearth` present; `githearth check /var/lib/githearth` → `checked 1 repositories, 0 problems`, exit **0**; restored storage → exit 0; deliberately corrupted storage → exit 1 with `git fsck failed` / `1 problems` |
| 4. `install.md` install procedure + min Git 2.40; `tls.md` header table | **PASS with residues** | Install procedure present (8 steps, Compose and direct-binary paths), `Git ≥ 2.40` in prerequisites and doctor list; enforced in `githearth-git/src/layout.rs:67-71`; live headers match the updated table except the Git `Cache-Control` row (defect G); step 4's fresh-DB expectation is false (defect F) |

### Drill commands and timings (second project)

| Drill | Command | Result / wall clock |
| --- | --- | --- |
| Build + install | `GITHEARTH_HTTP_PORT=18080 GITHEARTH_SSH_PORT=12222 GITHEARTH_POSTGRES_PORT=54330 docker compose -p ghm4ops2 up -d --build --wait` | exit 0, **11.57 s** (22:17:36Z → 22:17:47Z). BuildKit reported `FROM debian:trixie-slim`; layers were `CACHED` because the fixed image had been built at 22:15:49Z by the fix session, so a cold-build time was not measured. `app`/`postgres` healthy, `migrate`/`ssh-keygen` exit 0 |
| Health | `curl -fsS http://127.0.0.1:18080/health/{ready,live}` | 200 / 200; `/health/ready` still 200 after every subsequent drill |
| Keep-data stop | `docker compose -p ghm4ops2 down` | exit 0, **0.53 s**; both named volumes remain; containers/network removed; 18080/12222 closed |
| Restart from kept data | `docker compose -p ghm4ops2 up -d --wait` | exit 0, **9.50 s** (22:19:52Z → 22:20:02Z); `ssh-keygen` exit 0, app healthy |
| `stop`/`start` app | `docker compose -p ghm4ops2 stop app`; `docker compose -p ghm4ops2 start app` | start exit 0, **1.76 s**; ready 200; ssh-keygen log `keeping it` (round-1 case 28 now passes) |
| Shipped checker | `docker compose -p ghm4ops2 exec -T app githearth check /var/lib/githearth` | `checked 1 repositories, 0 problems`, exit **0** |
| Backup | `pg_dump -Fc` + `tar -czf … --exclude ./run .` | dump 136,720 B, ~1 s; tar 11,640 B, <1 s; `SHA256SUMS` both `OK`; tar has 0 `./run` entries, contains `./repositories/…` and `./ssh_host_key` |
| Restore | `createdb ops_restore` + `pg_restore`; extract tar to new root | restore exit 0, ~1 s; third instance ready 200 after **2 s**; HTTP and SSH clone HEAD `5d97a78…`; host-key fingerprint `SHA256:fpH73Lz…` identical on 12222, 12223 and the restored key file; login 200 |
| Uninstall | `docker compose -p ghm4ops2 down`; `docker compose -p ghm4ops2 down --volumes` | `down` 0.48 s keeps both volumes and closes all three ports; `down --volumes` removes both; no container/network/volume/image residue |

### Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| RR1 | `up -d --build --wait` on new project `ghm4ops2` with port overrides | Build and start; exit 0 | Exit 0, 11.57 s; image built and tagged `ghm4ops2-app` | PASS |
| RR2 | Service end states | app/postgres healthy; migrate/ssh-keygen exit 0 | Exactly that (`docker compose ps -a`) | PASS |
| RR3 | `/health/ready`, `/health/live` | 200/200 | 200/200 | PASS |
| RR4 | Image base and Git version | Debian trixie; `git --version` 2.47 | trixie; `git version 2.47.3`; OpenSSH 10.0p2 | PASS |
| RR5 | Primary isolation during build | Primary untouched | Primary `Up 6 minutes (healthy)`, restarts 0, ready 200 throughout | PASS |
| RR6 | `down` keeps named volumes | Volumes survive; containers/network removed | Both `ghm4ops2_*` volumes listed; no container; network removed | PASS |
| RR7 | Ports after `down` | 18080/12222 refuse | Both closed | PASS |
| RR8 | `up -d --wait` after `down` on same volumes | Exit 0, installation returns | Exit 0, 9.50 s | PASS |
| RR9 | `ssh-keygen` on existing key | Exit 0 (round 1: exit 1) | Exit 0; `{"message":"SSH host key already exists; keeping it"}` | PASS |
| RR10 | Host key survives `down`/`up` | Same checksum | `296c33fe83dd…` before and after; mode 600 | PASS |
| RR11 | Data survives `down`/`up` | HTTP and SSH clones match pre-down HEAD | Both `5d97a7858ffcaadf37c9c256088532e331129200` | PASS |
| RR12 | `stop app` then `start app` (round-1 case 28) | App starts, ready 200 | Start exit 0, 1.76 s; ready 200; ssh-keygen exit 0 | PASS |
| RR13 | Direct `ssh-keygen` twice in one container | Second run succeeds and keeps the key | Both exit 0; sha256 `ce48668b…` unchanged | PASS |
| RR14 | `githearth check /var/lib/githearth` in the image | Exists, exit 0 | `checked 1 repositories, 0 problems`, exit 0 | PASS |
| RR15 | `githearth check` on restored storage | Exit 0 | `checked 1 repositories, 0 problems`, exit 0 | PASS |
| RR16 | `githearth check` on deliberately damaged storage | Nonzero, names the repository | `fe1f236b…: git fsck failed`; `1 problems`; exit 1 | PASS |
| RR17 | CLI installed at a stable path | Present in `PATH` | `/usr/local/bin/githearth`; bare invocation prints `usage: githearth check <storage-root>` | PASS |
| RR18 | `docs/operations/install.md` install procedure | Steps to configure, preflight, migrate, bootstrap, ssh-keygen, serve | Present as 8 numbered steps, plus a Compose pointer (`install.md:3-37`) | PASS |
| RR19 | Min Git 2.40 documented and enforced | Doc says 2.40; code rejects older | `install.md:15,58`; `layout.rs:67-71` rejects `major<2 or (2, minor<40)` | PASS |
| RR20 | `doctor` on the running fixed image | 7 checks, 0 failed | `ok git: 2.47`, 7/0/0/0, exit 0 | PASS |
| RR21 | `install.md:22-24` fresh-DB claim: only `schema` fails | Other checks `ok`/`warn` | `fail schema`, `fail lifecycle: cannot query repositories`, `fail jobs: cannot query jobs`; 3 failed, exit 1 | **FAIL** (defect F) |
| RR22 | `tls.md` nosniff / Referrer-Policy / CSP claims | Present on stated response classes | UI: nosniff, `Referrer-Policy: same-origin`, CSP; API and Git: nosniff + referrer, no CSP (as tabled) | PASS |
| RR23 | `X-Forwarded-For` consumed via `trusted_proxies` | Audit/log client IP is the forwarded address | With `GITHEARTH_TRUSTED_PROXIES=172.19.0.0/16`, login with `X-Forwarded-For: 203.0.113.7` logged `client_ip:"203.0.113.7"` | PASS |
| RR24 | CSP on all browser pages | Present on login and org/repo pages | Same CSP on `/`, `/login`, `/organizations/opsorg`, `/organizations/opsorg/repositories/opsrepo` | PASS |
| RR25 | `tls.md:119-121` Git `Cache-Control` row | Matches live Git responses | Live Git `info/refs` has `cache-control: no-store`; `tls.md:119` says no-store for git responses while `tls.md:121` says no-cache; `observe()` (`lib.rs:392-408`) overwrites every response with no-store | **FAIL** (defect G) |
| RR26 | `pg_dump -Fc` | Exit 0, nonzero dump | Exit 0, 136,720 B | PASS |
| RR27 | Storage tar per `backup-restore.md` | Excludes top-level `run/`, includes repositories and host key | 11,640 B; 0 `./run` entries; `./repositories/…`, `./ssh_host_key` present | PASS |
| RR28 | `SHA256SUMS` verification | Both `OK` | Both `OK` | PASS |
| RR29 | `createdb ops_restore` + `pg_restore` | Exit 0 | Exit 0 | PASS |
| RR30 | Metadata counts, source vs restored | Equal | accounts 1/1, orgs 1/1, repos 1/1, ref_updates 1/1, migrations 33/33 | PASS |
| RR31 | Restored repository integrity | `git fsck` clean; log matches | `git fsck --full` exit 0; log `5d97a78 initial round2 commit` | PASS |
| RR32 | Third instance on restored DB + storage | Starts and becomes ready | Ready 200 after 2 s | PASS |
| RR33 | Restored HTTP clone | HEAD matches backup | `5d97a78…` | PASS |
| RR34 | Restored SSH clone and host key | HEAD matches; fingerprint equals original | `5d97a78…`; `SHA256:fpH73Lz7EBUAeCNCCYh4x72otn+SPiU/RoNxNRayM04` on 12222, 12223 and the restored key file | PASS |
| RR35 | Login on restored instance | 200, same administrator | 200 | PASS |
| RR36 | `down` (keep volumes) | Volumes keep | 0.48 s; both volumes remain; 18080/12222/54330 closed | PASS |
| RR37 | `down --volumes` after that | Both named volumes removed | Both `Removed` | PASS |
| RR38 | Post-uninstall residue | No container/network/volume/image | None remain; `docker compose ls` has no `ghm4ops2` | PASS |
| RR39 | Primary instance before/after | Healthy, no restart | app and postgres `healthy`; app `restarts=0`, started 22:16:22Z (before the drills), ready 200 on three consecutive polls at the end | PASS |
| RR40 | Repository git state | No change | Main repo `b820454`, only pre-existing untracked `crates/githearth-server/examples/`; worktree `7e1ded3`, clean | PASS |

### Remaining defects

#### F. `install.md` still promises a clean `doctor` before the first `migrate` (round-1 defect E, still open)

- Severity: **minor** (exit code is correct; noisy preflight and a false runbook claim)
- Repro:
  ```sh
  docker compose -p ghm4ops2 exec -T postgres createdb -U githearth ops_none
  docker run --rm --network ghm4ops2_default \
    -e GITHEARTH_DATABASE_URL='postgres://githearth:<pw>@ghm4ops2-postgres-1:5432/ops_none' \
    ghm4ops2-app doctor
  ```
- Expected (new `install.md:22-24`): before the first migrate only `schema` fails; every other check is `ok` or `warn`.
- Actual: three failures and exit 1.
  ```
  ok   git: 2.47
  ok   storage: /var/lib/githearth writable
  ok   disk: 388.31 GiB free
  ok   database: connected
  fail schema: database not initialized; run migrate
  fail lifecycle: cannot query repositories
  fail jobs: cannot query jobs
  7 checks, 3 failed, 0 warnings, 0 skipped
  ```
- Evidence: `lab15r2/doctor-fresh.txt`; `doctor.rs` and the lifecycle/jobs checks are unchanged by `7e1ded3`. The doc paragraph was added without changing the behavior it describes.
- Scope: systematic on an empty database. Either skip `lifecycle`/`jobs` when `schema` is uninitialized, or correct the runbook.

#### G. `tls.md` Cache-Control row contradicts itself and the server

- Severity: **minor** (docs only; behavior is safe — `no-store` is stronger than `no-cache`)
- Repro:
  ```sh
  curl -sI "http://opsadmin:<pw>@127.0.0.1:18080/opsorg/opsrepo.git/info/refs?service=git-upload-pack" \
    | grep -i cache-control
  # cache-control: no-store
  ```
- Expected: one accurate row for Git responses.
- Actual: `tls.md:119` says `Cache-Control: no-store` covers "all JSON + git responses", while `tls.md:121` says Git uses `no-cache` (citing `git_http.rs`, which does set it locally). The outer `observe` middleware overwrites every response with `no-store` (`lib.rs:392-408`), so the `no-cache` value never reaches the client. The same table points at `lib.rs` `security_headers` (no such function; it is `observe`) and a stale `lib.rs:347` line reference.
- Evidence: `lab15r2/xff-log.txt`; live header capture above; source `git_http.rs:171,311,595` vs `lib.rs:408`.
- Scope: documentation drift in the table that this fix updated.

### Updated M4 exit-item status (round 2)

| Item | Status | Evidence / limits |
| --- | --- | --- |
| 1. Independent install from the operations runbook | **PASS with one doc caveat** | `install.md` now contains the procedure; the Compose path installed cleanly on the fixed build. Step 4's fresh-DB expectation is still wrong (defect F). |
| 2. Restart | **PASS** (defect A fixed) | `restart app` 0.26 s (round 1), `down` → `up -d --wait` 9.50 s, `stop`/`start app` 1.76 s; health 200 each time; data and host key unchanged. |
| 3. Disk-full behaviour | NOT TESTED (outside this domain) | No change from round 1. |
| 4. Bounded resources | NOT TESTED (outside this domain) | No change from round 1. |
| 5. Off-host backup | NOT PROVEN (single host only) | Same-host dump/tar/manifest/restore re-passed; no second host. |
| 6. Fresh restore | **PASS** (defect B fixed) | Dump 136,720 B + tar 11,640 B → `ops_restore` + new storage root → third instance ready in 2 s → HTTP/SSH clone `5d97a78…`, counts equal, host key restored, and the documented `githearth check` now runs **in the image** (exit 0; corruption detected with exit 1). |
| 7. Upgrade | **PASS for preconditions** (not re-run) | Round-1 preconditions stand; `7e1ded3` changes no migration, and `doctor` on the fixed build reports `schema: latest v40`, 0 failed. Full cross-version upgrade still not rebuilt here. |
| 8. Controlled uninstall | **PASS** | `down` keeps both volumes; `down --volumes` removes both; all three ports close; no residue. |
| 10. Minimum hardware publication | NOT TESTED | No change from round 1. |

### Isolation and cleanup

- Second project `ghm4ops2` was fully removed after the drills: containers, network, `ghm4ops2_postgres-data`, `ghm4ops2_repositories`, and the `ghm4ops2-app`/`-migrate`/`-ssh-keygen` images. Temporary one-off containers (`ghm4ops2-restore-app`, `ghm4ops2-xff`) were removed.
- Primary `githearth-app-1` and `githearth-postgres-1` stayed `healthy` with `restarts=0` for the whole window (app started 22:16:22Z, before the first drill); `/health/ready` was 200 before, during and after. No migration, restart, volume or image operation touched the primary project. Note: the primary app was already rebuilt from the fixed worktree at 22:15:49Z/22:16:22Z by the fix session, before this drill began; this drill did not cause it.
- Repository state unchanged: main checkout `b820454` with only the pre-existing untracked `crates/githearth-server/examples/`; fix worktree clean at `7e1ded3`. No commits, branches or files were changed by this drill.
- Round-2 evidence is under `…/opencode/lab15r2/`: `up-build.log`, `down-up.log`, `check.out`, `restore-check.out`, `corrupt-check.out`, `doctor-ok.txt`, `doctor-fresh.txt`, `sshkeygen-idempotent.txt`, `xff-log.txt`, `backup-info.txt`, `image-info.txt`, `ps-before-uninstall.txt`.
