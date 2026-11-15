# M4.1 U19 evidence: pull-request pins on an upgraded installation

Date: 2026-11-15. Author: HavishJadav.
Worktree: `/Users/suniljadav/Aarav/Github/githearth-m41-b11`, branch
`m41/evidence-upgrade`, at `main` = `3f96d95` (clean when the run started).

This case is the one acceptance case that requires an installation which
*predates* pins. The installation below was built at the commit before the pin
migrations, populated through the product's API and Git transport, then upgraded
in place with the current binary. No code was changed. The scratch database was
`githearth_b11_upgrade` (never `githearth_test`); the storage root was
`/tmp/githearth-u19/storage`.

## Verdicts

| Clause | Verdict |
| --- | --- |
| Head pins exist for every existing and new pull request before the first prune | **pass** |
| A pin is not advertised by `upload-pack` | **pass** |
| A pin is charged to the owning organization | **pass** |
| A pin expires on the stated retention | **not demonstrated** (the shortest configurable window is 1 day; see below) |

---

## 1. The old installation (before pins)

The boundary is the commit that added `0045`; the old installation is its
parent.

```console
$ git log --oneline -- crates/githearth-server/migrations/0045_pinned_commits.sql
4681f08 M4.1: scope the fork re-copy promise and backfill merged bases (F1, F2)
98ca1d1 M4.1: keep the plan's pin retention default and conform the code (U11)
44205b2 M4.1: fail closed on a truncated live set and wire pins into the pull request (U11)
$ git log --oneline -1 44205b2^
0069753 Merge pull request #173 from DeviousCardi/m41/accounts-admin
```

A worktree was built at `0069753` with its own target directory.

```console
$ cd /tmp/githearth-old-u19 && CARGO_TARGET_DIR=/tmp/githearth-u19-target-old \
    cargo build -p githearth-server
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 12s
```

`setup` was run against a freshly created `githearth_b11_upgrade` with the
scratch storage root:

```console
$ printf '%s' "$GH_ADMIN_PASSWORD" | GITHEARTH_DATABASE_URL="$GH_DB_URL" \
    $GH_OLD_BIN setup --config-path /tmp/githearth-u19/config.toml \
    --http-port 18081 --no-ssh --storage-root /tmp/githearth-u19/storage
created      /tmp/githearth-u19/config.toml (mode 0600, database URL from the environment or stdin)
unchanged    /tmp/githearth-u19/storage (mode 0755, owned by the current user)
skipped      SSH is disabled; no host key is generated
created      migrated to v44
created      first administrator `admin` (12607f97-9216-44c7-a585-20498ecd8bcd)
ok   git: 2.50
ok   storage: /tmp/githearth-u19/storage writable
warn disk: 24.24 GiB free
ok   database: connected
ok   schema: latest v44
ok   lifecycle: no stuck repositories
ok   jobs: no dead, late or stuck jobs
7 checks, 0 failed, 1 warnings, 0 skipped
setup complete: start the server with `githearth-server serve --config /tmp/githearth-u19/config.toml`
```

The old schema ends at v44 and has no `pinned_commits` table:

```console
$ psql "$GH_DB_URL" -c "SELECT to_regclass('public.pinned_commits')"
 to_regclass 
-------------
 
(1 row)
```

### Fixture created through the product

The old server was served, then the following state was created with the JSON
API and `git push` (all IDs and OIDs are from this run):

| Object | Value |
| --- | --- |
| Organization | `u19-org` = `c30d0d99-b6aa-4303-98dd-6d245f3ed3ec` |
| Repository | `u19-repo` = `ba4bd0bc-867e-414d-a5b1-a8cbf871c791` (owned by the organization above) |
| Administrator | `12607f97-9216-44c7-a585-20498ecd8bcd` |
| PR #1 open, `feature-open` -> `main` | id `411d3a91-60b1-4f19-8fb0-7844bab2648a`, head `a9f6df132681400e72f62fcc1a42df622fe951cb` |
| PR #2 merged, `feature-merged` -> `main` | id `0de1b779-8a07-45ca-a49c-9040a79cff74`, head `b63348497d6c5f9f401e1517cdd344733c2d200e` |
| PR #3 closed, `feature-closed` -> `main` | id `8f9a1ae1-9e52-409c-9847-43571bfdcf44`, head `75ab06216604de3b2310ddb252a0e3202cdd9f38` |
| PR #4 closed, `feature-skip` -> `doomed-base` | id `8a8b8e85-376d-4f2a-918c-4f66615f6d29`, head `412f4bbafd70a377d833bef250601a879b82a3b5` |
| `main` before merge | `35ae0a6df3de8814465e7c2922ba366f5f313b26` |
| `main` after merge | `f4dd00e2ee7dfd9505e381be862aabaa1d321b7a` |
| `doomed-base` (deleted) | `35ae0a6df3de8814465e7c2922ba366f5f313b26` |
| Pre-upgrade reviews (#1–#4) | ids `a1d759d2-ac10-4e78-a3a4-18f5adcbc658`, `1c7490b2-b51f-44fb-a17a-7703b5ef4bbd`, `ce1f7971-d4b6-4fb6-a89f-c0dbc7d1d113`, `36814933-97f9-4abc-af02-e13ff43dadf7` |
| Unpinned control (later deleted) | commit `5939b6ae81d9b7b13db276cb5bc094191f7ff2ce`, blob `815c99912a7a9fd790bee4dad1b8c29b697e5d8b` |
| Merge intent for PR #2 | operation id `061acf43-c252-473e-a16e-1c5b54b106ce` |

Each request was given a pre-upgrade review (`verdict: commented`) at its head,
so `0045`'s own head backfill had a commit to select. PR #2 was then merged
through `POST .../pulls/2/merge`; PR #3 and PR #4 were closed through `PATCH
.../pulls/{n}` with `{"state":"closed"}`. The source branches of the merged and
closed requests (`feature-merged`, `feature-closed`) and the target branch of
PR #4 (`doomed-base`) were then deleted with `git push origin --delete`, so the
heads are not reachable from any ref and the target of PR #4 is gone.

```console
$ git ls-remote --heads origin
a9f6df132681400e72f62fcc1a42df622fe951cb	refs/heads/feature-open
412f4bbafd70a377d833bef250601a879b82a3b5	refs/heads/feature-skip
f4dd00e2ee7dfd9505e381be862aabaa1d321b7a	refs/heads/main
```

---

## 2. Upgrade in place, and the migration order

The same database and storage root were used. The new binary was built from
`3f96d95` (`CARGO_TARGET_DIR=/tmp/githearth-u19-target-new`).

```console
$ $GH_NEW_BIN --config /tmp/githearth-u19/config.toml migrate
{"timestamp":"2026-11-15T10:41:52.429862Z","level":"INFO","fields":{"message":"schema migrated"},"target":"githearth_server"}
```

The migration history shows the versions that were missing, in the order they
were applied (`44` was already present from the old installation):

```console
$ psql "$GH_DB_URL" -c "SELECT version, description, installed_on, success FROM _sqlx_migrations WHERE version >= 43 ORDER BY version"
 version |                description                |           installed_on           | success 
---------+-------------------------------------------+----------------------------------+---------
      43 | repository housekeeping                   | 2026-11-15 16:11:52.398662+05:30 | t
      44 | password reset                            | 2026-11-15 16:11:19.274426+05:30 | t
      45 | pinned commits                            | 2026-11-15 16:11:52.40557+05:30  | t
      46 | pinned commit kinds                       | 2026-11-15 16:11:52.409222+05:30 | t
      47 | pull request base backfill skips          | 2026-11-15 16:11:52.413793+05:30 | t
      48 | teams and invitations                     | 2026-11-15 16:11:52.415422+05:30 | t
      49 | account email                             | 2026-11-15 16:11:52.418407+05:30 | t
      50 | outbox fanout                             | 2026-11-15 16:11:52.419826+05:30 | t
      51 | job backoff cap                           | 2026-11-15 16:11:52.424083+05:30 | t
      52 | repository restore                        | 2026-11-15 16:11:52.424581+05:30 | t
      53 | repository transfer                       | 2026-11-15 16:11:52.425414+05:30 | t
      54 | email verification and notification email | 2026-11-15 16:11:52.426689+05:30 | t
(12 rows)
```

### Clause: head pins exist for every existing and new pull request before the first prune

Immediately after `migrate`, before the new binary had served or any
housekeeping had run, `0045` had written a head pin for all four pre-existing
requests:

```console
$ psql "$GH_DB_URL" -c "SELECT pr.number, pr.state, pr.source_branch, p.commit_oid, p.reason, p.retain_until FROM pinned_commits p JOIN pull_requests pr ON pr.id = p.pull_request_id WHERE p.base_of_pull_request_id IS NULL AND p.review_comment_id IS NULL AND p.commit_status_id IS NULL AND p.review_commit_oid IS NULL ORDER BY pr.number"
 number | state  | source_branch  |                commit_oid                |  reason  |     retain_until     
--------+--------+----------------+------------------------------------------+----------+----------------------
      1 | open   | feature-open   | a9f6df132681400e72f62fcc1a42df622fe951cb | backfill | 2026-12-15T10:41:52Z
      2 | merged | feature-merged | b63348497d6c5f9f401e1517cdd344733c2d200e | backfill | 2026-12-15T10:41:52Z
      3 | closed | feature-closed | 75ab06216604de3b2310ddb252a0e3202cdd9f38 | backfill | 2026-12-15T10:41:52Z
      4 | closed | feature-skip   | 412f4bbafd70a377d833bef250601a879b82a3b5 | backfill | 2026-12-15T10:41:52Z
(4 rows)
```

The one pin a migration cannot write is the base (a pull-request row never
stored it). `serve` ran the base backfill before the scheduler could enqueue a
run. The log records three bases written and the one request whose target branch
was deleted:

```console
$ GITHEARTH_HOUSEKEEPING_ENABLED=false GITHEARTH_HOUSEKEEPING_PIN_RETENTION_DAYS=1 \
    GITHEARTH_BIND=127.0.0.1:18082 $GH_NEW_BIN serve --config /tmp/githearth-u19/config.toml
{"timestamp":"...","level":"WARN","fields":{"message":"base pin backfill cannot reconstruct this request: its target branch is gone","repository_id":"ba4bd0bc-867e-414d-a5b1-a8cbf871c791","pull_request_id":"8a8b8e85-376d-4f2a-918c-4f66615f6d29","state":"closed","target_branch":"doomed-base"},"target":"githearth_server::housekeeping"}
{"timestamp":"...","level":"INFO","fields":{"message":"pull-request base pins backfilled; skipped requests are listed by `githearth-server doctor`","written":3,"skipped":1},"target":"githearth_server::housekeeping"}
```

`doctor` reports the skip rather than losing it:

```console
$ $GH_NEW_BIN --config /tmp/githearth-u19/config.toml doctor
ok   git: 2.50
ok   storage: /tmp/githearth-u19/storage writable
warn disk: 24.28 GiB free
ok   email: not configured (smtp_host is empty); no mail is sent
ok   database: connected
ok   schema: latest v54
ok   lifecycle: no stuck repositories
ok   jobs: no dead, late or stuck jobs
ok   outbox: 0 pending, 0 delivering, 0 delivered delivery(ies)
warn housekeeping: 1 active repo(s) overdue for maintenance
warn base backfill: 1 request(s) have no base pin and cannot be reconstructed (target branch deleted): repository ba4bd0bc-867e-414d-a5b1-a8cbf871c791 #PR-4 (closed, target doomed-base)
11 checks, 0 failed, 3 warnings, 0 skipped
```

The base pins and the recorded skip:

```console
$ psql "$GH_DB_URL" -c "SELECT pr.number, pr.state, p.commit_oid, p.retain_until FROM pinned_commits p JOIN pull_requests pr ON pr.id = p.base_of_pull_request_id ORDER BY pr.number"
 number | state  |                commit_oid                |     retain_until     
--------+--------+------------------------------------------+----------------------
      1 | open   | f4dd00e2ee7dfd9505e381be862aabaa1d321b7a | 
      2 | merged | f4dd00e2ee7dfd9505e381be862aabaa1d321b7a | 2026-11-16T10:42:04Z
      3 | closed | f4dd00e2ee7dfd9505e381be862aabaa1d321b7a | 2026-11-16T10:42:04Z
(3 rows)
$ psql "$GH_DB_URL" -c "SELECT pr.number, s.state, s.target_branch, s.reason FROM pull_request_base_backfill_skips s JOIN pull_requests pr ON pr.id = s.pull_request_id"
 number | state  | target_branch |        reason         
--------+--------+---------------+-----------------------
      4 | closed | doomed-base   | target_branch_missing
(1 row)
```

A pull request created **after** the upgrade is pinned by its own create
transaction. Branch `feature-after` was pushed, then PR #5 was created:

```console
$ curl ... /api/v1/organizations/u19-org/repositories/u19-repo/pulls
status 201
{
  "id": "a6bbb0ef-8987-4aa8-9b16-23c0dcc7f34d",
  "number": 5,
  "state": "open",
  "source_branch": "feature-after",
  "target_branch": "main",
  "created_at": "2026-11-15T10:42:25Z",
  "head_id": "d52607323c9d3d6218a13434f3004905ec04d5c9",
  "base_id": "f4dd00e2ee7dfd9505e381be862aabaa1d321b7a"
}
```

The odd-looking `xmin` column is PostgreSQL's inserting transaction id; row
versions inserted in one transaction share it. PR #5 and both of its pins have
the same `xmin` (`560639`) and the same `created_at`, so the pins committed in
the pull-request transaction:

```console
$ psql "$GH_DB_URL" -c "SELECT pr.number, pr.created_at, pr.xmin::text AS pr_xmin, p.reason, p.commit_oid, p.created_at AS pin_created, p.xmin::text AS pin_xmin FROM pull_requests pr JOIN pinned_commits p ON p.pull_request_id = pr.id OR p.base_of_pull_request_id = pr.id WHERE pr.number = 5 ORDER BY p.reason"
 number |            pr_created            | pr_xmin |      reason       |                commit_oid                |           pin_created            | pin_xmin 
--------+----------------------------------+---------+-------------------+------------------------------------------+----------------------------------+----------
      5 | 2026-11-15 16:12:25.469607+05:30 | 560639  | pull_request_base | f4dd00e2ee7dfd9505e381be862aabaa1d321b7a | 2026-11-15 16:12:25.469607+05:30 | 560639
      5 | 2026-11-15 16:12:25.469607+05:30 | 560639  | pull_request_head | d52607323c9d3d6218a13434f3004905ec04d5c9 | 2026-11-15 16:12:25.469607+05:30 | 560639
(2 rows)
```

#### The first prune really pruned, and the pins survived it

The new server had housekeeping disabled; the only maintenance run was then made
explicitly, with a zero grace period. Before it, a control branch
`discard-control` was pushed and deleted. Its commit/tree/blob are unpinned and
unreachable; the closed, merged and skip heads are pinned.

```console
$ GITHEARTH_HOUSEKEEPING_GRACE_SECONDS=0 GITHEARTH_HOUSEKEEPING_PIN_RETENTION_DAYS=1 \
    $GH_NEW_BIN --config /tmp/githearth-u19/config.toml housekeeping \
    --repository ba4bd0bc-867e-414d-a5b1-a8cbf871c791
housekeeping completed for ba4bd0bc-867e-414d-a5b1-a8cbf871c791: 2206 -> 5008 bytes, 3 expired object(s), repacked, scan/deletion complete
```

Exactly three objects expired: the control commit, its tree and its blob. The
pinned objects are still in the bare repository after the run, and no pin is a
ref:

```console
$ git --git-dir=/tmp/githearth-u19/storage/repositories/ba/ba4bd0bc867e414da5b1a8cbf871c791.git show-ref
d52607323c9d3d6218a13434f3004905ec04d5c9 refs/heads/feature-after
a9f6df132681400e72f62fcc1a42df622fe951cb refs/heads/feature-open
412f4bbafd70a377d833bef250601a879b82a3b5 refs/heads/feature-skip
f4dd00e2ee7dfd9505e381be862aabaa1d321b7a refs/heads/main
$ git --git-dir=... cat-file -e 75ab06216604de3b2310ddb252a0e3202cdd9f38 && echo closed-PRESENT
closed-PRESENT
$ git --git-dir=... cat-file -e b63348497d6c5f9f401e1517cdd344733c2d200e && echo merged-PRESENT
merged-PRESENT
$ git --git-dir=... cat-file -e 412f4bbafd70a377d833bef250601a879b82a3b5 && echo skip-PRESENT
skip-PRESENT
$ git --git-dir=... cat-file -e 5939b6ae81d9b7b13db276cb5bc094191f7ff2ce || echo discard-absent
discard-absent
$ git --git-dir=... ls-tree 75ab06216604de3b2310ddb252a0e3202cdd9f38 closed.txt
100644 blob 7b8bf4cdcad1b50384e3998582500b4676531bdf	closed.txt
```

Only one housekeeping row and one housekeeping audit event exist, so this was
the first run:

```console
$ psql "$GH_DB_URL" -c "SELECT count(*), count(*) FILTER (WHERE last_outcome='completed'), min(last_run_at) FROM repository_housekeeping"
 count | count |            min             
-------+-------+----------------------------
     1 |     1 | 2026-11-15 16:12:48.128325+05:30
```

**Verdict: pass.** All four pre-existing requests had a head pin immediately
after migration (before any housekeeping), all four were still live after the
first run, and a request created after the upgrade got its head and base pin in
the same transaction.

---

## 3. Clause: a pin is not advertised by `upload-pack`

The public ref advertisement contains no pin, and a fresh clone does not receive
the closed head:

```console
$ git ls-remote http://.../u19-org/u19-repo.git
f4dd00e2ee7dfd9505e381be862aabaa1d321b7a	HEAD
d52607323c9d3d6218a13434f3004905ec04d5c9	refs/heads/feature-after
a9f6df132681400e72f62fcc1a42df622fe951cb	refs/heads/feature-open
412f4bbafd70a377d833bef250601a879b82a3b5	refs/heads/feature-skip
f4dd00e2ee7dfd9505e381be862aabaa1d321b7a	refs/heads/main
$ git ls-remote ... | grep -q 75ab06216604de3b2310ddb252a0e3202cdd9f38 || echo not-advertised
not-advertised
$ git clone ... /tmp/githearth-u19/clone-after
$ git -C /tmp/githearth-u19/clone-after branch -a
* main
  remotes/origin/HEAD -> origin/main
  remotes/origin/feature-after
  remotes/origin/feature-open
  remotes/origin/feature-skip
  remotes/origin/main
$ git -C /tmp/githearth-u19/clone-after cat-file -e 75ab06216604de3b2310ddb252a0e3202cdd9f38 || echo absent-from-clone
absent-from-clone
```

The objects survive in the server's repository (shown above), and the closed
request's diff still renders through the API after the prune, using the pin as
the head:

```console
$ GET /api/v1/organizations/u19-org/repositories/u19-repo/pulls/3
200
{"number":3,"state":"closed","head_id":"75ab06216604de3b2310ddb252a0e3202cdd9f38","base_id":"f4dd00e2ee7dfd9505e381be862aabaa1d321b7a","closed_at":"2026-11-15T10:41:41Z"}
$ GET /api/v1/organizations/u19-org/repositories/u19-repo/compare/f4dd00e2ee7dfd9505e381be862aabaa1d321b7a...75ab06216604de3b2310ddb252a0e3202cdd9f38
200
{"base_id":"f4dd00e2ee7dfd9505e381be862aabaa1d321b7a","head_id":"75ab06216604de3b2310ddb252a0e3202cdd9f38","ahead_by":1,
 "files":[["closed.txt","A",1,0]],
 "closed_patch":["diff --git a/closed.txt b/closed.txt\nnew file mode 100644\nindex 0000000..7b8bf4c\n--- /dev/null\n+++ b/closed.txt\n@@ -0,0 +1 @@\n+closed change\n"]}
```

**Verdict: pass.** `upload-pack` advertised only refs; the closed head was in no
ref and was not fetched by a clone; its commit, tree and blob were still in the
server repository; and the API rendered the diff from the pin.

---

## 4. Clause: a pin is charged to the owning organization

Every pin row names a repository, and the repository belongs to the
organization. All 13 pins of this installation are in `u19-repo`, owned by
`u19-org`:

```console
$ psql "$GH_DB_URL" -c "SELECT o.slug AS organization, o.id AS organization_id, r.name AS repository, count(*) AS pins FROM pinned_commits p JOIN repositories r ON r.id = p.repository_id JOIN organizations o ON o.id = r.organization_id GROUP BY o.slug, o.id, r.name"
 organization |           organization_id            | repository | pins 
--------------+--------------------------------------+------------+------
 u19-org      | c30d0d99-b6aa-4303-98dd-6d245f3ed3ec | u19-repo   |   13
```

The pinned bytes are in that repository's `objects/` (the path used by the
`cat-file` checks above:
`/tmp/githearth-u19/storage/repositories/ba/ba4bd0bc867e414da5b1a8cbf871c791.git/objects`).
Quota is measured over `objects/`, so these bytes are charged to `u19-org`
through its repository; `docs/operations/quota.md` states the same rule
("Those objects remain inside `objects/`, so they count toward the quota
exactly like reachable objects").

**Verdict: pass.** The pin's repository, its objects and the quota all resolve to
the single owning organization.

---

## 5. Clause: a pin expires on the stated retention

The running configuration for the upgraded serve and the maintenance run was
`housekeeping_pin_retention_days = 1` (set with
`GITHEARTH_HOUSEKEEPING_PIN_RETENTION_DAYS=1`; the shipped default is 90 days).
The database shows that value applied to pins written after the upgrade:

```console
$ psql "$GH_DB_URL" -c "SELECT reason, count(*), min(retain_until - created_at) AS shortest, max(retain_until - created_at) AS longest, count(*) FILTER (WHERE retain_until IS NULL) AS live_while_subject_exists FROM pinned_commits GROUP BY reason ORDER BY reason"
      reason       | pins |   shortest_lifetime   | longest_lifetime | live_while_subject_exists 
-------------------+------+-----------------------+------------------+---------------------------
 backfill          |    3 | 30 days               | 30 days          |                         0
 pull_request_base |    4 | 1 day                 | 1 day            |                         2
 pull_request_head |    2 | 1 day 00:00:22.515471 | 30 days          |                         0
 review_commit     |    4 |                       |                  |                         4
```

Two facts matter for an honest verdict:

- The pre-existing head pins written by migration `0045` carry the migration's
  fixed **30-day** window (`now() + interval '30 days'`), which its own comment
  documents and which is deliberately not the configured
  `housekeeping_pin_retention_days`.
- Pins written after the upgrade use the running configuration: the base pins
  for the closed/merged requests are exactly **1 day**, and PR #5's head pin
  started at 1 day and was extended to 1 day from the housekeeping run (open
  pins are re-pinned on every run).

`retain_until` was still in the future for every pin; no pin expired during this
run:

```console
$ psql "$GH_DB_URL" -c "SELECT count(*) FILTER (WHERE retain_until IS NOT NULL AND retain_until <= now()) AS expired_pins FROM pinned_commits"
 expired_pins 
--------------
            0
```

**Verdict: not demonstrated.** The shortest retention that configuration accepts
is 1 day (`1..3650` days), so expiry cannot be observed in a real-time run. We
lowered the window from 90 to 1 day and measured the applied timestamps instead
of asserting the expiry; we did not fake elapsed time or edit pin rows. The
clause should be read as "retention is applied as configured", which the
timestamps show, not as "expiry was observed".

---

## Reproduction notes

- Scratch database: `postgres://.../githearth_b11_upgrade` (from
  `GITHEARTH_TEST_DATABASE_URL` in `env.sh`, last path component changed).
- Storage: `/tmp/githearth-u19/storage`; config: `/tmp/githearth-u19/config.toml`.
- Old binary: `0069753`, target `/tmp/githearth-u19-target-old`.
- New binary: `3f96d95`, target `/tmp/githearth-u19-target-new`.
- The fixture used the JSON API (`/api/v1/login`, `/organizations`,
  `/organizations/{org}/repositories`, `/pulls`, `/pulls/{n}/reviews`,
  `/pulls/{n}/merge`, `PATCH /pulls/{n}`) and `git push` over the HTTP
  transport. The only direct SQL was read-only inspection; no code, migration or
  application data was modified by hand.
