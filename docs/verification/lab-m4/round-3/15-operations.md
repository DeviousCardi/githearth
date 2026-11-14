# Operations — M4 closure round 3

Tester: t15 (owner of `lab15`). Date: 2026-11-14. Instance: main `800cad4` (primary
`http://127.0.0.1:8080`, SSH `127.0.0.1:2222`, PostgreSQL `127.0.0.1:54329`).
All disruptive drills ran on an isolated second Compose project, **`ghm4r3`**
(HTTP 18080, SSH 12222, PostgreSQL 54330), then that project was removed.

Verdict: **PASS WITH DEFECTS**. Every install, restart, integrity, backup/restore,
migrate, uninstall and diagnostics drill passed. One minor defect remains, in the
M4 verification harness rather than the product: `scripts/verify-m4.py` still
asserts the pre-fix `ssh-keygen` behaviour (exit 1 on an existing key), which now
contradicts both the code and `docs/operations/install.md`.

Cases: **84 run, 83 pass, 1 fail, 0 skipped.** One numbered case failed on its
first attempt for a test-environment reason (missing Compose port overrides on the
`up` after `down`); it passed on the single re-run and is not counted as a defect.

## Host and image information

| Item | Value |
| --- | --- |
| Host | macOS (Darwin), arm64, Docker Desktop |
| Docker Engine / Compose | 29.x / v5.6.1 |
| Git client | 2.50.1 (Apple Git-155) |
| Primary app image | `sha256:740b6af2fa2ae3f485087cde2c514dfa623f668243bfef1d0b3bcd8f6c067345` (345,162,519 B), created 2026-11-14T06:34:36Z |
| Second-project app image | `ghm4r3-app` = `sha256:a8219f523432b427b19cd53f7f8f84d5b24b100d00cef85917b3ec2070e3844f` (repo digest `ghm4r3-app@sha256:a8219f52…`), 345,162,519 B |
| Second-project migrate / ssh-keygen images | `sha256:9e5b1bd0f9610d2a7855acdfc8408b92b6314eb811c11aef8175e6b25f59e4fb` / `sha256:e6c082d60c064253f4c384311720ee70cfcd329854e388d17e74ed5f8f738f13` |
| Image content | Debian trixie-slim, `git version 2.47.3`, OpenSSH_10.0p2, curl, uid 10001 (`githearth`), `/var/lib/githearth` mode 0700, `/usr/local/bin/githearth` present |
| Build provenance | `docker compose up --build --wait` ran all three BuildKit builds from cache (same rootfs layers as the primary image), 10 s wall clock |

### Drill timings

| Drill | Command | Wall clock |
| --- | --- | --- |
| Build + install | `GITHEARTH_HTTP_PORT=18080 GITHEARTH_SSH_PORT=12222 GITHEARTH_POSTGRES_PORT=54330 docker compose -p ghm4r3 up -d --build --wait` | exit 0, **10 s** |
| Keep-data stop | `docker compose -p ghm4r3 down` (same env) | exit 0, **0.61 s**; both named volumes remain |
| Restart from kept data | `docker compose -p ghm4r3 up -d --wait` (same env) | exit 0, **9.58 s** |
| Health | `curl http://127.0.0.1:18080/health/{live,ready}` | 200 / 200 before and after every drill |
| HTTP push / clone | `git push` / `git clone` over `http://admin:…@127.0.0.1:18080/r3-org/r3-repo.git` | 0.13 s / 0.26 s |
| SSH clone / push | `ssh://git@127.0.0.1:12222/r3-org/r3-repo.git` with registered ed25519 key | 1.18 s / < 1 s |
| `githearth check` clean | `docker compose … exec -T app githearth check /var/lib/githearth` | 0.05 s, `checked 2 repositories, 0 problems`, exit 0 |
| Backup | `pg_dump -Fc` + `tar -czf … --exclude=./run .` | dump 137,960 B / 96 ms; tar 23,039 B / 34 ms |
| Restore | `createdb r3restore`, `pg_restore`, extract tar to new volume | 111 ms / 197 ms; restored app ready in ~1 s |
| Uninstall | `docker compose -p ghm4r3 down --volumes` | exit 0, **0.48 s**, both volumes and the network removed |
| Fresh re-install on wiped volumes | `up -d --wait` | exit 0, **9.50 s**; health 200, no admin (login 401), 0 repos |

## Results

### A. Install, Compose and image

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Primary isolation before the drill | Primary healthy, not restarted | `/health/live` 200, `/health/ready` 200; app started 06:34:39Z, restarts 0 | PASS |
| 2 | Second project names and ports | Isolated project, 18080/12222/54330 | `ghm4r3-*` containers; host bindings `127.0.0.1:18080->8080`, `12222->2222`, `54330->5432` | PASS |
| 3 | README `.env` snippet | 64-hex password, mode 0600 | `POSTGRES_PASSWORD=<64 hex>`, mode 600 | PASS |
| 4 | Re-run of the `.env` snippet | Refuses to overwrite an existing secret | `FileExistsError`, exit 1 | PASS |
| 5 | `up -d --build --wait` on new project | Builds and starts all four services | exit 0 in 10 s; all layers `CACHED` | PASS |
| 6 | Service end states | app/postgres healthy; migrate/ssh-keygen exit 0 | `docker compose ps -a`: app `running (healthy)`, postgres `running (healthy)`, migrate `exited 0`, ssh-keygen `exited 0` | PASS |
| 7 | Health endpoints | `/health/live` 200, `/health/ready` 200, empty body | 200/200; container healthcheck also 200 | PASS |
| 8 | Port exposure | Loopback only | Every published port has `host_ip: 127.0.0.1` (compose render) | PASS |
| 9 | Image identity and contents | Trixie, Git ≥ 2.40, curl, openssh-client, non-root | git 2.47.3, OpenSSH 10.0p2, curl present; uid 10001; storage 0700; image digest recorded above | PASS |
| 10 | Image ships docs and the checker | `githearth`, LICENSE, notices | `/usr/local/bin/githearth`; `githearth check` works; `/usr/share/doc/githearth/{LICENSE,THIRD_PARTY_NOTICES}` | PASS |
| 11 | `docker compose config` | Valid; defaults 8080/2222/54329; overrides honoured | `--quiet` exit 0; default render 8080/2222/54329; override render 18080/12222/54330 | PASS |

### B. Bootstrap, sessions and repository creation

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 12 | First `bootstrap-admin` (password on stdin) | Creates the admin, exit 0 | `administrator created`, exit 0; one `accounts` row, `is_admin = t` | PASS |
| 13 | Second `bootstrap-admin`, same/other name | Refuses, changes nothing, exit 1 | `an administrator already exists; bootstrap refused without changing credentials`, exit 1; still one admin | PASS |
| 14 | Short password | Rejected before creation, exit 1 | `password must contain 12..1024 bytes`, exit 1 | PASS |
| 15 | Password passed on the command line | Rejected by argument parsing, never echoed | clap usage error for unknown `--password`, exit 2 | PASS |
| 16 | Login with the exact password | 200 with a 64-character bearer token | 200, token length 64 | PASS |
| 17 | Login with wrong password / unknown user | 401 for both | 401 / 401 | PASS |
| 18 | Unauthenticated organization creation | 401 | 401 | PASS |
| 19 | Create org and private repo; duplicate repo | 201 / 201 / 409 | 201 `r3-org`, 201 `r3-repo` (private), 409 duplicate | PASS |
| 20 | Create public repo | 201 | 201 `r3-public` | PASS |

### C. HTTP and SSH Git

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 21 | HTTP push seed commit | Accepted | `* [new branch] main -> main`, exit 0, 0.13 s | PASS |
| 22 | HTTP clone | HEAD matches source | HEAD `4f8810e18940b703ccf2449bbcedbce8209399ed`, 0.26 s | PASS |
| 23 | HTTP clone with wrong password | Denied | `fatal: Authentication failed`, exit 128 | PASS |
| 24 | Anonymous clone of a private repo | Denied | `fatal: Authentication failed` | PASS |
| 25 | HTTP request for a missing repo (with credentials) | 404 | 404 | PASS |
| 26 | Anonymous clone of the public repo | Allowed, HEAD matches | clone exit 0, HEAD `4f8810e1…` | PASS |
| 27 | Register ed25519 key; host-key fingerprint | 201; `SHA256:` fingerprint | 201, `SHA256:7ID832CZ…`; served host key `SHA256:P1L/ckYC…` equals the container key file | PASS |
| 28 | SSH clone | HEAD matches | exit 0, HEAD `4f8810e1…`, 1.18 s | PASS |
| 29 | SSH push; ref agreement | Accepted; HTTP and SSH refs identical | push exit 0; `git ls-remote` outputs identical (`main = c0dce439…`) | PASS |
| 30 | SSH clone with an unregistered key | Denied | `Permission denied (publickey)` / "correct access rights" | PASS |

### D. `doctor`, `migrate` and schema versions

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 31 | `doctor` on the running install | 7 checks, 0 failed, exit 0 | `ok git 2.47`, storage, disk, database, `schema: latest v40`, lifecycle, jobs; `7 checks, 0 failed, 0 warnings, 0 skipped`, exit 0 | PASS |
| 32 | `migrate` a second time | Idempotent, exit 0 | exit 0, `schema migrated`; nothing applied | PASS |
| 33 | Schema version query | Consistent with `doctor` | `_sqlx_migrations`: 33 rows, max version 40; `doctor: latest v40` | PASS |
| 34 | `doctor` on a fresh, un-migrated DB | Exit 1 with `fail schema`; dependants fail or skip | `fail schema: database not initialized; run migrate`; lifecycle/jobs also fail (`cannot query`), `7 checks, 3 failed`, exit 1 — now documented at `install.md:25-27` | PASS |
| 35 | `migrate` that fresh DB, `doctor`, `migrate` again | Exit 0 / 0 failed / idempotent | exit 0; `latest v40`, 0 failed; rerun exit 0, still 33 rows | PASS |
| 36 | Pending migration | `doctor` fails pending; `serve` refuses | `fail schema: 1 pending migrations; run githearth-server migrate`; serve exit 1 `database schema does not match this binary: 1 pending migrations…` | PASS |
| 37 | Tampered migration checksum | `doctor` fails | `fail schema: migration 40 was changed after it was applied`, exit 1 | PASS |
| 38 | Database newer than binary | `doctor` fails; `serve` refuses | `fail schema: database schema is newer than this binary (version 99999)`; serve exit 1 with the same reason | PASS |
| 39 | Failed migration row | `doctor` fails and names it | `fail schema: migration 99998 failed`, exit 1 | PASS |
| 40 | `storage_root` too long | Config error before any check, exit 1 | `storage_root is too long: the hook socket path would be 142 bytes, the Unix limit is 107…`, exit 1 | PASS |
| 41 | Non-PostgreSQL URL | Config error before checks, exit 1 | `database_url must be a PostgreSQL URL`, exit 1 | PASS |
| 42 | Unreachable database | `fail database` + 3 skips, exit 1 | `1 failed, 0 warnings, 3 skipped`; schema/lifecycle/jobs `skip: database unavailable`; no credentials in output | PASS |
| 43 | World-writable storage root | `fail storage`, exit 1 | `fail storage: /var/lib/githearth is world-writable`, `1 failed`; mode restored to 700 | PASS |
| 44 | Missing storage root | Warn but exit 0 | `warn storage: … does not exist; will be created by serve`, `0 failed, 1 warning`, exit 0 | PASS |

### E. Restart (keep data)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 45 | `down` | Removes containers/network, keeps named volumes | exit 0, 0.61 s; `ghm4r3_postgres-data` and `ghm4r3_repositories` remain | PASS |
| 46 | `up -d --wait` after `down` | Installation returns on the same data | exit 0, 9.58 s, all services expected. First attempt omitted the port env vars, so Compose tried `127.0.0.1:54329` and failed against the primary; re-run with the documented env passed (test mistake, not a product defect) | PASS |
| 47 | `ssh-keygen` on an existing key | Keeps it, exit 0, same file | exit 0, JSON log `SSH host key already exists; keeping it`; fingerprint `SHA256:P1L/ckYC…` unchanged | PASS |
| 48 | Readiness after restart | `/health/live` and `/health/ready` 200 | 200 / 200 | PASS |
| 49 | Persistence of data and sessions | Repo, issue and session survive | HTTP clone HEAD `c0dce439…`; issue 1 `{"number":1,"title":"R3 ops issue","state":"open"}`; pre-restart bearer token still 200 | PASS |
| 50 | SSH after restart | Same refs and key | `git ls-remote` HEAD/main `c0dce439…`; host key unchanged | PASS |

### F. `githearth check` and storage integrity

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 51 | Clean storage | Zero exit, no problem lines | `checked 2 repositories, 0 problems`, exit 0 (0.05 s) | PASS |
| 52 | Empty storage root (with `repositories/`) | Not a clean pass | `repositories: no repositories found`; `checked 0 repositories, 1 problems`; exit 1 | PASS |
| 53 | Missing storage root | Error, exit 1 | `storage root does not exist or is not a directory`, exit 1 | PASS |
| 54 | Root without `repositories/` | Error, exit 1 | `storage root has no repositories/ directory`, exit 1 | PASS |
| 55 | Symlinked storage root | Error, exit 1 | `storage root must not be a symlink`, exit 1 | PASS |
| 56 | Corrupted loose object | Names the repo, exit 1 | `f940d5bd…: error: inflate: data stream error`; `1 problems`, exit 1 (first attempt hit the object's 0444 mode; re-run after `chmod u+w` — test mistake) | PASS |
| 57 | Deleted loose object | Names the repo, exit 1 | `error: refs/heads/main: invalid sha1 pointer 4f8810e1…`; exit 1 | PASS |
| 58 | Corrupted journal body | `journal:` problem, exit 1 | `journal: journal corrupt at byte offset 366`; exit 1 | PASS |
| 59 | Torn journal tail, then server start | Check reports torn tail; one server start truncates it; re-check clean | `journal torn tail at byte offset 274`, exit 1; after a short `serve` run, `checked 2 repositories, 0 problems`, exit 0 — matches `backup-restore.md:50-55` | PASS |
| 60 | Documented tar exclusion anchoring | Excludes only top-level `./run`, keeps `refs/heads/run` | Tar contains `./repositories/x/refs/heads/run` and no `./run/…` | PASS |

### G. Backup and restore to a new database and root

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 61 | `pg_dump -Fc` | Nonzero dump | exit 0, 137,960 B, 96 ms; archive lists 285 TOC entries | PASS |
| 62 | Storage snapshot per the runbook | Excludes `run/`, includes repos and host key | exit 0, 23,039 B, 34 ms; top level has `repositories`, `ssh_host_key`, `hooks`, `orphans`, `tmp`, `trash`, no `run` | PASS |
| 63 | `SHA256SUMS` manifest | Both files hashed | dump `a6d2ddba…`, storage `93ed5621…` | PASS |
| 64 | `createdb` + `pg_restore` into a new DB | Exit 0 | exit 0, 111 ms, `r3restore` | PASS |
| 65 | Metadata counts, source vs restored | Equal | accounts 1/1, repositories 2/2, issues 1/1, sessions 2/2, ssh_keys 1/1, audit_events 11/11 | PASS |
| 66 | Extract into a new root; integrity check | Ownership preserved, `check` exit 0 | root 0700 uid 10001, host key 0600 uid 10001; `checked 2 repositories, 0 problems`, exit 0 | PASS |
| 67 | Start a server on the restored DB/root | Ready; `doctor` clean | ready 200 in ~1 s; `7 checks, 0 failed` | PASS |
| 68 | Clone restored repo over HTTP and SSH; host key | Matching HEADs; same fingerprint | both `c0dce439…`; served fingerprint `SHA256:P1L/ckYC…` equals the pre-backup key | PASS |
| 69 | Restored metadata, access and writes | Login/issue intact; private denied anonymously; public allowed; push works | issue 1 open; anonymous private clone denied; public clone `4f8810e1…`; push of a new commit `edc78f7c…` accepted | PASS |

### H. Uninstall and primary verification

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 70 | `down --volumes` | Removes containers, network and both volumes | exit 0, 0.48 s; both `ghm4r3_*` volumes `Removed`, network removed | PASS |
| 71 | Post-uninstall residue | No container, volume, network or restore volume | 0 containers, 0 volumes, 0 networks for `ghm4r3`; throwaway restore container/volume removed | PASS |
| 72 | Fresh install on wiped volumes | Empty installation, no admin | `up` exit 0, 9.50 s; health 200/200; login 401; 0 accounts; 0 repositories; `doctor` 7/0/0/0 | PASS |
| 73 | Final teardown and image cleanup | Project and its images gone | `down --volumes` exit 0; second-project app/migrate/ssh-keygen images removed; primary image untouched | PASS |
| 74 | Primary after all drills | Healthy, same container, t15 usable | `/health/live` 200, `/health/ready` 200; app started 06:34:39Z, restarts 0; t15 login 200, `acme` and `lab15` listed, org page 200; primary `doctor` 7/0/0/0 | PASS |
| 75 | Other tester's containers | Untouched | `r161fix` still up; primary postgres up 9 h | PASS |

### I. Diagnostics and documentation

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 76 | Application logs | Structured JSON with method/route/status/latency | 20/20 sampled lines parse as JSON, e.g. `{"message":"http request","method":"GET","route":"/health/ready","status":200,"elapsed_ms":1,…}` | PASS |
| 77 | One-shot service logs | Show migration and host-key actions | `{"message":"schema migrated"}`; `{"message":"SSH host key generated",…}` then `already exists; keeping it` on the second boot | PASS |
| 78 | Secret leakage in logs | No DB password, token or URL | 0 matches for the `.env` `POSTGRES_PASSWORD` value across all project logs | PASS |
| 79 | PostgreSQL logs and `ps` JSON | Ready message; health fields present | `database system is ready to accept connections`; `ps --format json` reports service/state/health/exit code | PASS |
| 80 | README quick start as written | Build, health, bootstrap, stop/keep-data all work | All commands executed on the isolated project; `down` → `up` restored the installation | PASS |
| 81 | `docs/operations/install.md` | Complete procedure; 7 checks; truthful pre-migrate and `ssh-keygen` claims | 8 steps present; `doctor` printed 7 checks; fresh-DB note (`install.md:25-27`) matches the observed 3 failures; `ssh-keygen` idempotent claim (`install.md:33-34`) matches exit 0 and the kept key | PASS |
| 82 | `backup-restore.md` and `uninstall.md` commands | Match observed behaviour | Dump/tar/restore/check all worked; `down` keeps volumes, `down --volumes` deletes both; tar exclusion anchored as documented | PASS |
| 83 | README "bootstrap runs once" claim | Re-run refused without changing credentials | Refused, exit 1, existing account unchanged | PASS |
| 84 | `scripts/verify-m4.py` S1 `ssh-keygen` assertion | Assert the documented idempotent success | Rerun correctly exits 0, but the harness raises `HarnessError` for exactly that (`scripts/verify-m4.py:330`) | **FAIL** (defect D1) |

## Defects

### D1. `scripts/verify-m4.py` still expects `ssh-keygen` to fail on an existing key

- Severity: **minor** (verification tooling only; no runtime or documentation defect)
- Repro (what the harness does in S1):
  ```sh
  docker compose -p ghm4r3 exec -T app githearth-server ssh-keygen   # creates the key
  docker compose -p ghm4r3 exec -T app githearth-server ssh-keygen   # existing key
  # actual: exit 0, log "SSH host key already exists; keeping it"
  ```
  The Compose `ssh-keygen` one-shot service showed exactly this on the second
  `up` (exit 0, key fingerprint unchanged).
- Expected: the harness comment says "rerun does not overwrite"; `install.md:33-34`
  says the command "is idempotent: an existing host key is kept and the command
  succeeds"; the implementation returns `Ok(())` for `AlreadyExists`
  (`crates/githearth-server/src/ssh.rs:58-63`).
- Actual: `scripts/verify-m4.py:330` does
  `if p.returncode == 0: raise HarnessError("ssh-keygen rerun should refuse to overwrite")`,
  so the S1 install scenario now fails on a correct build.
- Evidence: `scripts/verify-m4.py:330`; `crates/githearth-server/src/ssh.rs:58-63`;
  `docs/operations/install.md:33-34`; observed second-boot log in the `ghm4r3`
  ssh-keygen service. Root cause: commit `7e1ded3` made `ssh-keygen` idempotent
  (the round-1 defect A fix) but left the harness assertion from the old
  "refuse to overwrite" behaviour.
- Fix: change the assertion to require exit 0 and an unchanged key (checksum)
  on the rerun; the comment already describes that behaviour.

## M4 exit-item status

| M4 item | Status in this drill | Evidence / limits |
| --- | --- | --- |
| 1. Independent install from the runbook | **PASS (agent drill)** | README quick start and all 8 `install.md` steps executed on a second project; the `.env` snippet, build, migrate, bootstrap, host key and health all worked. Still an agent, not a human. |
| 2. Restart | **PASS** | `down` 0.61 s keeps volumes; `up` 9.58 s; `ssh-keygen` exit 0 with unchanged key; health 200; HTTP+SSH HEAD, issue and sessions survive. Round-2 defect A is closed. |
| 3. Disk-full behaviour | **NOT TESTED** (outside this domain) | No filesystem was filled; `m4-qualification.md` s8 and `db-disk-full.md` remain the evidence. |
| 4. Bounded resources | **NOT TESTED** (outside this domain) | No load or RSS measurement; config-bound validation was observed indirectly through the invalid-config cases. |
| 5. Off-host backup | **NOT PROVEN** (single host) | Dump, tar, manifest and restore were exercised locally; no second host was available for rsync/verify/restore-from-copy. |
| 6. Fresh restore | **PASS** | New `r3restore` DB + new volume/root from dump and tar; `githearth check` clean; restored app ready and `doctor` clean; HTTP/SSH HEADS match, host key preserved, metadata counts equal, writes work. |
| 7. Upgrade | **PASS for preconditions; full cross-version upgrade not run** | `migrate` idempotent (33 rows, v40); pending, tampered, newer and failed schema refusals pass for both `doctor` and `serve`. No old binary was built this round. |
| 8. Controlled uninstall | **PASS** | `down` keeps data; `down --volumes` removes both named volumes and the network; re-install starts empty (login 401, 0 accounts/repos); final teardown leaves no residue. |
| 9. HTTP/SSH authorization agreement | **PASS (smoke only)** | Private anonymous and wrong-password denied on both; registered key allowed on both; refs identical. Full role matrix is another tester's domain. |
| 10. Minimum hardware publication | **NOT TESTED** | No measurements this round. |
| 11. Licence/notices in image | **PASS (presence)** | `LICENSE` and `THIRD_PARTY_NOTICES` shipped at `/usr/share/doc/githearth/`. Content/staleness not re-audited. |
| 12. TLS/session defaults | **NOT TESTED** | Plain HTTP on loopback only, as the brief specifies. |
| 13. Accessibility | **NOT TESTED** | No UI audit. |

## Notes

- **Test mistake, not a defect:** after `down`, a plain `up` (without
  `GITHEARTH_HTTP_PORT`/`GITHEARTH_SSH_PORT`/`GITHEARTH_POSTGRES_PORT`) fell back
  to the defaults 8080/2222/54329 and failed with `Bind for 127.0.0.1:54329
  failed: port is already allocated`. Re-running with the documented overrides
  passed. This is expected Compose env behaviour for a second project, not a
  GitHearth defect; for repeatable multi-instance drills, export the three
  variables for every `docker compose -p` command.
- **Test mistake, not a defect:** the first corrupted-object run could not write
  the object because packed loose objects are mode 0444; re-running after
  `chmod u+w` produced the expected `git fsck` failure.
- **Invocation note:** `docker run … <image> githearth check …` fails because the
  image entrypoint is `githearth-server`; use
  `docker run --entrypoint githearth …` or run the command inside the running
  container (`docker compose exec app githearth check <root>`), which the
  runbook implies and which passed.
- The build produced three image tags (`-app`, `-migrate`, `-ssh-keygen`) with
  identical rootfs layers; BuildKit deduplicated the work (`CACHED`) and the
  layers are shared with the primary image. The extra tags are only tag clutter,
  not a runtime issue; they were removed with the project.
- Combined stdout/stderr redirection can print `doctor reported failures; see
  output above` before the check lines because the summary is on stderr. Exit
  codes and individual lines are correct; cosmetic only, not counted.
- Same-host restore only; off-host copy, TLS termination, disk-full,
  bounded-resource and the full old→new binary upgrade were not re-run this
  round (see the M4 item table).
- Evidence (timings, doctor outputs, backup artifacts, logs) is under
  `…/opencode/lab-reports-m4-final/work15/`.
  `git status` in the repository was clean before and after; no repository file
  was modified and nothing was posted to GitHub.
