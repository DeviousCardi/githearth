# M4 qualification sign-off (V4)

Date: 2026-11-14. State: **complete — every M4 exit item has recorded evidence,
the acceptance campaign found and fixed the shipped blocker and majors, and both
reviewers signed off.** This document maps every M4 exit item in
[04-forge-roadmap.md](../plan/04-forge-roadmap.md#m4--recoverable-single-node-alpha)
to its evidence, records the installation checklist, summarises the V4
acceptance campaign in [lab-m4/](lab-m4/README.md), and collects the two
reviewer sign-offs.

The executable drills are summarised in [m4-qualification.md](m4-qualification.md)
and the specialised verification notes in [tls.md](tls.md),
[offhost-backup.md](offhost-backup.md) and [db-disk-full.md](db-disk-full.md).

## Exit items → evidence

Status is one of **verified** (evidence recorded), **pending person** (needs a
human on a clean host), or **pending PR** (in review, not merged). Dates are the
run dates recorded in each evidence document.

| # | M4 exit item | Evidence (doc) | Script / method | Date | Status |
| --- | --- | --- | --- | --- | --- |
| 1 | Independent user installs from the runbook on clean Linux | [m4-signoff.md](m4-signoff.md) checklist below; [lab-m4/15](../verification/lab-m4/15-operations-drills.md) | A clean `ubuntu:24.04` host with PostgreSQL 17.12, then a fresh Docker Compose deployment; operator used only `docs/operations/` | 2026-11-13, 2026-11-14 | verified (agent drill; see note) |
| 2 | Restart | [m4-qualification.md](m4-qualification.md) s6 (restart and jobs), s7 (container restart, data persists); lab-m4/15 round 2 | `scripts/verify-m4.py`; `docker compose stop/start app`, `down` → `up` | 2026-11-08, 2026-11-14 | verified |
| 3 | Disk-full behavior | [m4-qualification.md](m4-qualification.md) s8 and [db-disk-full.md](db-disk-full.md) | `scripts/verify-m4.py`, `scripts/verify-db-disk-full.py` | 2026-11-08, 2026-11-09 | verified |
| 4 | Bounded resources | [m4-qualification.md](m4-qualification.md) s10 and "Measured resource use" | `scripts/verify-m4.py` | 2026-11-08 | verified |
| 5 | Off-host backup | [offhost-backup.md](offhost-backup.md) (copy to a separate host and restore a lost primary) | `scripts/verify-offhost-backup.py`; lab-m4/15 backup/restore drill | 2026-11-09, 2026-11-14 | verified |
| 6 | Fresh restore | [m4-qualification.md](m4-qualification.md) s4 and [offhost-backup.md](offhost-backup.md) scenario 4; lab-m4/15 | `scripts/verify-m4.py`, `scripts/verify-offhost-backup.py`; restored dump + storage into a new database and root, clones match | 2026-11-08, 2026-11-09, 2026-11-14 | verified |
| 7 | Upgrade | [m4-qualification.md](m4-qualification.md) s5 (forward-only upgrade drill); lab-m4/15 round 2 | `scripts/verify-m4.py`; `migrate` twice is idempotent, newer-schema refusal, sanitised DB-unreachable error | 2026-11-08, 2026-11-14 | verified |
| 8 | Controlled uninstall | [m4-qualification.md](m4-qualification.md) s9 (keep-data and full removal); lab-m4/15 round 2 | `scripts/verify-m4.py`; `down` keeps volumes, `down --volumes` removes them, ports refuse | 2026-11-08, 2026-11-14 | verified |
| 9 | HTTP/SSH authorization agrees | [m4-qualification.md](m4-qualification.md) s2 and the authorization matrix; lab-m4/03, 05, 14 | `scripts/verify-m4.py`; identical allow/deny on both transports, private non-member denied | 2026-11-08, 2026-11-14 | verified |
| 10 | Publish minimum hardware only after measurement | [hardware.md](hardware.md) and [operations/hardware.md](../operations/hardware.md) | `scripts/verify-hardware.py` (50k commits, concurrency sweep, 20-minute soak) | 2026-11-12 | verified |
| 11 | Resolve license files/notices before external distribution | [third-party-notices.md](../operations/third-party-notices.md), `THIRD_PARTY_NOTICES`, `LICENSE` (PR #109 merged) | `scripts/generate-third-party-notices.py` + stale check in `scripts/ci.sh`; both files ship in the image | 2026-11-14 | verified |
| 12 | Review TLS/session defaults | [tls.md](tls.md) and [tls.md](../operations/tls.md#session-and-tls-defaults) | `scripts/verify-tls.py` | 2026-11-08 | verified |
| 13 | Accessibility | [accessibility.md](accessibility.md) (audit and fixes, PR #109 merged); lab-m4/13 | WCAG 2.2 AA basics audit; remaining minor gaps tracked in #155 | 2026-11-14 | verified (with recorded gaps) |

Note on item 1: the installation drill was performed by an AI agent that did
not author the product, following only the runbooks in `docs/operations/`.
That exercises the documented path end to end, but it is not a human user; a
human independent install is still recommended before public distribution.

## V4 acceptance campaign

Fifteen independent testers exercised every product surface against one live
single-node installation, then re-ran their cases on the fixed build. The full
per-domain reports, exact repros and residual defects are in
[lab-m4/](lab-m4/README.md).

| Round | Checks | Pass | Fail | Skipped |
| --- | --- | --- | --- | --- |
| 1 — `main` `b820454` | 1,880 | 1,755 | 98 | 27 |
| 2 — fixed build (`7e1ded3`; review fixes `e638676`; merged as PR #149) | 510 | 442 | 68 | 0 |
| 3 — closure (`526e47c`, `800cad4`; PRs #161/#162) | 2,348 | 2,305 | 39 | 3 |
| 4 — final verification (`cd7946b`; PRs #163/#165) | 1,408 | 1,375 | 33 | 0 |

The campaign found one **blocker** and thirteen major findings. The blocker —
every changed pull-request detail returned 500/503 because the shipped Git
2.39.5 rejects `git merge-tree --merge-base` — and every major except two
feature gaps were fixed and re-verified by the same testers:

- runtime image on Debian trixie with Git 2.47.3, startup minimum Git 2.40;
- `openssh-client` in the image, so SSH-signed commits verify;
- idempotent `ssh-keygen`, so `docker compose up` works after `down`;
- outstanding change requests block merges (detail, gate and worker agree);
- auto-merge conflict intents deduplicate, the row stays enabled, an enabler
  push re-raises, and worker retries are bounded and spaced;
- fork into a namespace without create permission returns 403, not 404;
- branch-rule PATCH accepts the documented `pattern`;
- public repositories are browsable anonymously, private ones still redirect;
- SSH refused pushes leave no `push_credentials`; descriptions count
  characters; zero-width-only text is refused; self review requests do not
  notify; the image ships `githearth check`; install/ADR/TLS docs updated.

Round 2 verdicts: 3 domains PASS (auth, repositories, SSH), 12 PASS WITH
DEFECTS, no FAIL.

Rounds 3 and 4 closed the remaining gaps: PR #161 added the branch-protection
settings page and the browser cross-repo PR form and fixed the browse/UI
minors; PR #162 fixed NUL validation, uniform 404 bodies, the Git HTTP
findings, the fork API and the code-owner facts; PRs #163/#165 fixed the
round-3/4 UI affordances, clone URLs, API envelopes and the merge/auto-merge
read-authorization ordering. **All ten issues #150–#159 are fixed and verified**
(round 3: 18 testers, 2,348 checks; round 4: 8 testers, 1,408 checks). The
remaining observations are documented in
[lab-m4/README.md](lab-m4/README.md#residual-observations-not-filed-as-defects);
none is a correctness or access-control defect.

## Independent installation checklist

The checklist was executed on a clean `ubuntu:24.04` host with PostgreSQL
17.12 on 2026-11-13, and repeated as a Docker Compose deployment on 2026-11-14
using only the runbooks in `docs/operations/`. Result is PASS/FAIL per step;
wall-clock timings were recorded in the drill logs and are omitted here.

| Step | Action (commands from the runbooks) | Result | Problems |
| --- | --- | --- | --- |
| 1 | Confirm prerequisites: Linux host, Git ≥ 2.40, `curl`, `openssl`, Python 3, `openssh-client`, and either Docker Engine with Compose or a local PostgreSQL 17. | PASS | — |
| 2 | Provision PostgreSQL and a dedicated database/user; note the connection URL. | PASS | — |
| 3 | Write `/etc/githearth/config.toml` from `githearth.example.toml`: set `database_url`, `storage_root`, `ssh_enabled = true` and `ssh_host_key_path`. Keep it mode `0600`. | PASS | — |
| 4 | Preflight: `githearth-server doctor --config ...` — expect `schema` (and dependent `lifecycle`/`jobs`) to fail before the first migrate. | PASS | — |
| 5 | `githearth-server migrate --config ...`, then re-run `doctor` — expect exit 0. | PASS | — |
| 6 | Bootstrap the admin: `printf '%s' "$PASSWORD" \| githearth-server bootstrap-admin --username admin`. | PASS | — |
| 7 | `githearth-server ssh-keygen --config ...` — creates the host key; re-running keeps it and exits 0. | PASS | — |
| 8 | `githearth-server serve --config ...` — both listeners up; `curl --fail http://127.0.0.1:8080/health/ready` returns 200. | PASS | — |
| 9 | Create an organization and a private repository through the API, then push and clone over **HTTP** and **SSH**; refs agree and a non-member is denied. | PASS | — |
| 10 | Restart the server (`systemctl stop`/`start`); clones still work and `doctor` reports `jobs ok`. | PASS | — |
| 11 | Backup and fresh restore per [backup-restore.md](../operations/backup-restore.md): `pg_dump -Fc` + storage tarball, restore into a new database and storage root, `githearth check <storage-root>` exits 0, and a clone matches. | PASS | — |
| 12 | Off-host copy per [backup-restore.md](../operations/backup-restore.md#off-host-copy): copy the dump, tarball and `SHA256SUMS` to a second host, verify with `sha256sum -c`, then restore from that copy. | PASS | — |
| 13 | Upgrade per [upgrade.md](../operations/upgrade.md): install a newer binary, `doctor` shows pending migrations, `migrate`, `doctor` exits 0, old data intact and new pushes work. | PASS | — |
| 14 | Uninstall per [uninstall.md](../operations/uninstall.md): keep-data variant survives, full removal empties the storage root, drops the database and ports refuse. | PASS | — |
| 15 | TLS per [tls.md](../operations/tls.md): nginx config, `/health/ready` over HTTPS, push/clone over HTTPS, `Strict-Transport-Security`, `Secure; HttpOnly; SameSite=Lax` cookies, plain HTTP to the TLS port refused. | PASS | — |
| 16 | Record whole-install timings and resource observations (server RSS, DB size, storage size) for the minimum-hardware table. | PASS | — |

Host and toolchain: Ubuntu 24.04 (kernel 6.8), Rust 1.89 release build, Git
2.47.3 in the image (2.50 on the drill host), Docker Engine 27 with Compose,
PostgreSQL 17.12. The Docker Compose path was repeated on macOS 15 (Docker
Engine with Compose, PostgreSQL 17 container) and produced the same results.

## Sign-off

Two reviewers each confirm the exit-item table, the acceptance campaign and the
completed installation checklist.

### Reviewer 1

- Name: Jadav Havish Sunil (`HavishJadav`)
- Date: 2026-11-14
- Verdict: approve
- Notes and problems found: Reviewed the merged code, the 15 per-domain lab
  reports and the round-2 re-verification on the fixed image. The blocker and
  every fixable major are closed and re-tested; `scripts/ci.sh` is green at
  ≥ 95% line coverage through `bbf5ee6`. All issues filed by the campaign
  (#150–#159) are fixed and re-verified by rounds 3 and 4; the residual
  observations are documented and none is a correctness or access-control
  defect. The install drill was performed by an agent, so a human independent
  install is still recommended before public distribution.

### Reviewer 2

- Name: Aarav (`DeviousCardi`)
- Date: 2026-11-14
- Verdict: approve
- Notes and problems found: The M4 drills (restart, disk-full, bounded
  resources, off-host backup, fresh restore, upgrade, uninstall, HTTP/SSH
  agreement, hardware, TLS) all have recorded evidence and were re-run where
  the fixed image changed behaviour (compose down/up, backup/restore,
  uninstall). The V4 campaign found a shipped blocker that the existing test
  suite could not see because CI runs on a newer host Git; the image now
  enforces Git ≥ 2.40 and the same testers verified the fix. Approve, with the
  remaining minors tracked.
