# Notifications — M4 lab acceptance report

Tester: t12. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 69 run, 65 pass, 3 fail, 1 skipped.

Scope: notification JSON API (`/api/v1/user/notifications`), server-rendered
`/notifications` page, the durable job rows that deliver notifications, and the
pull-request/review/merge events that create them. Fixtures in `lab12` only:
`t12-notify` (merged PR #1 by carol, self-merged PR #2, open PR #3 with review
requests, open PR #4 with 56 reviews), `t12-notify-access` (carol-authored open
PR, unused by the final cases), `t12-notify-loss` (grace-authored PR, grant
revoked after notification). Accounts used: `t12` (owner), `carol` (write),
`dave` (maintain), `erin` (read), `grace` (member + repo write grant). No other
lab, `acme`/`beta` settings, containers or migrations were modified; all DB
inspection was read-only.

## Results

### Delivery and deduplication

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Review by another account (carol on t12's PR #1) | exactly one `pull_request.reviewed`; `pull_request_id`=PR1, `actor_username`=carol, `created_at` UTC, `read_at` null | one row `5ebe1d0d-…`, all fields correct, `created_at=2026-11-13T21:16:50Z`, `read_at=null` | PASS |
| 2 | One row per review event/account | 1 DB row for review `8618c303-…` + t12; unique `(event_id, account_id)` | `rows=1`; unique index `notifications_event_id_account_id_key` present | PASS |
| 3 | Merge by another account (carol merges PR #1) | one `pull_request.merged`, actor carol, PR1 | one row `f1728ff6-…`, fields correct | PASS |
| 4 | One recipient per merge event | notifications for the merge event = 1 | 1 | PASS |
| 5 | Author reviews own PR (comment-only) | 201 review, no notification | 201 `565e9c88-…`, t12 count 2→2, 0 DB rows for the self review | PASS |
| 6 | Author approves own PR | refused, no notification | 403 `{"error":"not permitted"}` | PASS |
| 7 | Author merges own PR #2 | merge reaches `merged`, no notification | intent `merged`; t12 count stayed 3; 0 `pull_request.merged` rows for PR2 | PASS |
| 8 | Repeat merge of an already-merged PR | refused, no duplicate notification/job | 409 `pull_request is not open`, merged rows still 1 | PASS |
| 9 | Idempotent review replay with same `Idempotency-Key` | same review, one row, one job | 201/201 same review id; 1 notification row; 1 job | PASS |
| 10 | Self review-request (carol requests carol) | no self-notification | **FAIL** — 200 and a `pull_request.review_requested` row where `account_id = actor_id = carol` | FAIL |
| 11 | Review request by another account | reviewer notified once, actor = requester, author not notified | carol +1 `review_requested`, actor t12, PR3; t12 unchanged | PASS |
| 12 | Duplicate review request | one request, one job, one notification | carol count unchanged; `pull_request_review_requests`=1; request job=1 | PASS |
| 13 | Multi-reviewer request (carol and dave) | each reviewer one notification; author none | carol +1, dave +1, t12 unchanged | PASS |
| 14 | Author requests own review; reader requests review | both refused | t12→t12 422 "author cannot review their own pull request"; t12→erin 422 "reviewer cannot approve this pull request" | PASS |

### List

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 15 | Newest first | created_at non-increasing, id desc tie-break, same order as DB | API order equals `ORDER BY created_at DESC, id DESC`; timestamps non-increasing | PASS |
| 16 | Maximum 50 with 59 notifications | exactly 50 rows, unique ids, nothing older | 50 rows, 50 unique ids; DB total 59 | PASS |
| 17 | `?unread=true` | only `read_at IS NULL`, still capped | 50 rows, all unread; DB unread=55 | PASS |
| 18 | `?unread=false` | read + unread, capped | 50 rows (same cap) | PASS |
| 19 | Fields only | exactly `id, kind, pull_request_id, actor_username, created_at, read_at` | exact six-key set; unread expressed as `read_at: null` | PASS |
| 20 | No title/repo/org leakage | no PR titles, repo or org names in JSON | distinctive titles `SecretT12Title *`, `CarolSecretTitle`, `GraceSecretTitle`, `t12-notify`, `lab12`, `bulk ` absent | PASS |
| 21 | Empty list | 200 `[]` for an account with none | erin 200 `[]` | PASS |
| 22 | Account isolation | no t12 ids in carol's list | disjoint id sets | PASS |
| 23 | Pagination behaviour | documented: no cursor/limit parameters | `?after=`, `?limit=`, `?offset=` → 400 `unknown field`; only newest 50 reachable (see Notes) | PASS |
| 24 | Malformed `unread` | 400 | `bogus`, empty and duplicate values all 400 | PASS |
| 25 | Trailing slash | 404 | 404 | PASS |
| 26 | Cache-Control | `no-store` | `cache-control: no-store` on 200 | PASS |
| 27 | created_at format and clock | UTC `YYYY-MM-DDTHH:MM:SSZ`, equal to DB UTC text | regex pass; API == `to_char(created_at AT TIME ZONE 'UTC', …)`; read_at within created_at..now | PASS |
| 28 | Ordering inside one second | API order equals DB order even when displayed seconds tie | byte-identical id order to DB (55 reviews inside ~2 s) | PASS |
| 29 | UI and API cap agree | UI also shows at most 50, no older-row access | UI rendered 50 `<li>` for 59 rows | PASS |

### Mark read

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 30 | Mark read | 204, empty body | 204, body `null` | PASS |
| 31 | `read_at` set | non-null, DB match, between created_at and now | `2026-11-13T21:20:22Z` == DB; bounds pass | PASS |
| 32 | Idempotent re-mark | 204, first read time retained | second 204, `read_at` unchanged | PASS |
| 33 | Unknown notification id | 404 | 404 `notification not found` | PASS |
| 34 | Another account's id | 404, target unchanged | 404; carol's `read_at` still null | PASS |
| 35 | Malformed id; GET on read route | 400; 405 | 400 UUID parse error; 405 | PASS |
| 36 | `unread=true` after read | row excluded | excluded, count 3→2 | PASS |
| 37 | Mark all read | clears every unread row; idempotent; no form when none left | UI 303; unread=0; second POST 303; form absent afterwards | PASS |
| 38 | Mark read after access loss | 204 (row still owned by account) | 204 | PASS |

### Credentials

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 39 | Session bearer | list works | 200 | PASS |
| 40 | `repo:read` PAT | 403 (session-only) | 403 `access tokens cannot view notifications` | PASS |
| 41 | `repo:write` PAT | 403 (session-only) | 403 same | PASS |
| 42 | PAT on mark read | 403 | 403 both read and write PATs | PASS |
| 43 | Anonymous / invalid bearer | 401 | list/read 401; `Bearer garbage` 401 | PASS |
| 44 | Logout and cookie-only | logged-out session 401; browser cookie alone not API auth | logout 204 → 401; cookie-only GET 401 | PASS |

### Access loss

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 45 | Notify a recipient with a repo-only grant | grace gets one `reviewed` from t12; t12 gets none | grace +1 (row `63064778-…`), actor t12; t12 unchanged | PASS |
| 46 | Revoke the recipient's access | grant DELETE 204; repo and PR reads 404 | 204; repo GET 404; PR GET 404 `pull request not found` | PASS |
| 47 | Notification survives access loss | list still returns the row, fields only, no title | 200, one row, six keys, no `GraceSecretTitle` | PASS |
| 48 | UI after access loss | placeholder, no title, no PR link, mark-read still offered | "A pull request you can no longer access"; title/link absent; Mark read+CSRF present | PASS |
| 49 | Matches docs | docs promise no title leak and client-side 404 | matches `docs/foundation.md:214`; no repository data returned | PASS |

### Jobs and durable delivery

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 50 | Notification jobs terminal | all lab12 notification jobs `done`; none pending/running | 66/66 done; 0 pending; 0 running | PASS |
| 51 | No dead jobs | 0 dead, attempts within cap | 0 dead; every job attempt=1, max_attempts=5, no `last_error` | PASS |
| 52 | IDs-only payloads | no titles/body/repo names in `jobs.payload` | regex `secret|t12-notify|lab12|bulk` over all lab12 payloads = 0 matches | PASS |
| 53 | No duplicate notification rows | unique `(event_id, account_id)` holds | group-by having count>1 = 0 | PASS |
| 54 | No duplicate job dedupe keys | one row per `(kind, dedupe_key)` | group-by count>1 = 0; duplicate review request produced one job | PASS |
| 55 | Counts match events | reviewed/merged/requested notifications = non-self events | 64 notifications: t12 59 (58 reviewed + 1 merged), carol 3 requested, dave 1 requested, grace 1 reviewed; 59 non-self reviews in lab12 (58 on t12 PRs, 1 on grace's PR) | PASS |
| 56 | Self events enqueue but insert nothing | self review/merge jobs end `done` with no row | reviewed jobs 60 vs 59 reviewed rows (t12 self comment job); merged jobs 2 vs 1 row (self-merge) | PASS |

### UI

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 57 | Anonymous `/notifications`; signed-in page | 303 `/login`; 200 for session | 303 location `/login`; 200 page | PASS |
| 58 | Items, links and accessible shell | `<ul>/<li>`, PR links, `nav aria-label`, `<h1>` | `<ul>`, 50 `<li>`, PR links, `nav aria-label="Primary"`, `<h1>Notifications</h1>` | PASS |
| 59 | Mark-read form with CSRF | hidden `csrf_token`, POST action; bad/missing token 403 | form present for each unread row; bad token 403; missing token 403 | PASS |
| 60 | Mark one read through the page | 303 `/notifications`, `read_at` set, button disappears | 303 `/notifications`; API read_at set; action gone after reload | PASS |
| 61 | Empty state | "No notifications.", no Mark all read | erin page shows exactly that | PASS |
| 62 | Unread badge/count | visible unread count/badge | **FAIL** — no numeric count or badge anywhere; only presence/absence of Mark all read | FAIL |
| 63 | Lost-access row | no title/link, mark-read available | placeholder, no `GraceSecretTitle`, no `/pulls/` link, Mark read present | PASS |
| 64 | Account scope and 404s | only own rows; another/unknown id 404 | no t12 ids in grace page; other id 404; unknown id 404 | PASS |
| 65 | Page cap | 50 rows maximum | 50 `<li>` for 59 notifications | PASS |
| 66 | Mark all read through page | 303, all unread cleared, idempotent | 303; unread=0; second POST 303 | PASS |
| 67 | Timestamps | UTC `…Z` in the page, same as API | page and API both `2026-11-13T21:22:03Z` | PASS |
| 68 | Notification click-through | target PR page renders | merged PR #1 page 200; **open PR #3/#4 pages 503 "Unavailable"**; JSON PR read 500 | FAIL |
| 69 | Mentions | if implemented, tested; otherwise skip with reference | not implemented (no mention code; mentions scoped to a later milestone, `docs/plan/04-forge-roadmap.md:137`) | SKIP |

## Defects

### Self review-request notifies the requester
- Severity: minor
- Repro:
  ```bash
  BASE=http://127.0.0.1:8080
  TOK=$(curl -sS -X POST "$BASE/api/v1/login" -H 'Content-Type: application/json' \
    -d '{"username":"carol","password":"<test-password>"}' | jq -r .token)
  curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
    -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
    -d '{"reviewer":"carol"}' \
    "$BASE/api/v1/organizations/lab12/repositories/t12-notify/pulls/4/requested_reviewers"
  # 200
  curl -sS -H "Authorization: Bearer $TOK" "$BASE/api/v1/user/notifications" \
    | jq '.[] | select(.actor_username=="carol" and .kind=="pull_request.review_requested")'
  ```
  Re-ran once on PR #3 after deleting and re-adding the request: same result.
- Expected: a request whose requester is also the reviewer creates no notification (docs: handlers "skip self-notifications", `docs/foundation.md:210`), or the self request is refused.
- Actual: 200; a `pull_request.review_requested` row is inserted with `account_id = actor_id = carol` (`74f048b9-…` for PR #4, `f919e1dc-…` for PR #3). The handler checks only that the request row still exists (`notifications.rs:195-209`) and never compares `requested_by` with `reviewer_id`; the request endpoint (`pulls.rs:1968-2009`) only rejects `reviewer_id == pull.author_id`, not `reviewer_id == actor_id`.
- Evidence: DB `SELECT id,account_id,actor_id,kind FROM notifications WHERE account_id=actor_id` returned two rows (both carol, both `pull_request.review_requested`); API list showed "carol requested your review on …" to carol.
- Scope: systematic for any non-author requesting themselves; notification noise, no data leak.

### Notifications UI has no unread badge or count
- Severity: minor
- Repro: sign in as t12 (55 unread, 59 total), open `GET /notifications`.
- Expected: an unread badge/count (the domain checklist explicitly asks for one).
- Actual: the primary nav is `<a href="/notifications" style="margin-left:auto">Notifications</a>` with no count; the page shows only a "Mark all read" form when anything is unread, and no numeric unread count. No `badge`, `unread` or count marker exists in the HTML.
- Evidence: `ui.rs:156` renders the bare nav link; `notifications_ui.rs:177-254` tracks `any_unread` solely to show/hide Mark all read. The JSON API can compute the count (`?unread=true`) but the page does not use it.
- Scope: systematic UI gap; notifications themselves and read state are correct. (The docs do not specify a badge, but the acceptance checklist does.)

### Open pull-request pages and JSON detail are unavailable (notification click-through fails)
- Severity: major (cross-domain; reported for consolidation — likely belongs to the pull-request/mergeability domain)
- Repro:
  ```bash
  # JSON: any open PR, including the acme fixture
  TOK=$(curl -sS -X POST http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' \
    -d '{"username":"alice","password":"<test-password>"}' | jq -r .token)
  curl -sS -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $TOK" \
    http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/pulls/1   # 500
  # UI: open lab PR vs merged lab PR
  #   /organizations/lab12/repositories/t12-notify/pulls/4 -> 503
  #   /organizations/lab12/repositories/t12-notify/pulls/1 -> 200
  ```
- Expected: 200 so a notification's target PR can be opened.
- Actual: `GET /pulls/{n}` JSON returns 500 `{"error":"storage operation failed"}` for every PR tested (open and merged, acme and lab12). The HTML PR page returns 503 "Unavailable" for open PRs and 200 for already-merged PRs. A notification row on PR #3/#4 links to a page that returns 503, so only notifications for merged PRs are clickable.
- Evidence: `docker logs githearth-app-1` shows `ERROR … "repository read failed","category":"git","repository_id":"83ca9b35-…"` then `GET /organizations/{slug}/repositories/{name}/pulls/{number} 503`, from `browse::map_storage_error` (`browse.rs:179`). The HTML page fails only for open PRs, while the JSON detail fails for merged PRs too, so more than one Git read path (the open-PR mergeability check at `mergeability.rs:130` and/or the code-owner read) reaches the same storage error; the exact failing Git command was not isolated without instrumenting the running binary. Acme/open PR #1 reproduces as alice, so it is not a lab12 fixture issue.
- Scope: systematic on this installation; blocks notification click-through for open PRs and any API client that resolves a notification via the PR API (which `docs/foundation.md:214` says is the intended path). No notification-list or delivery code is implicated.

## Notes / skipped

- **No pagination (documented limitation).** `ListQuery` accepts only `unread`; `after`/`limit`/`offset` are rejected with 400 and the SQL hard-codes `LIMIT 50` (`notifications.rs:276-300`). With 59 notifications the API and UI show the newest 50; the other 9 are unreachable through any endpoint. The foundation docs say "up to 50" but do not define a cursor, so this is recorded as a limitation, not a defect.
- **Unread representation.** The JSON has no `unread` boolean; unread is `read_at: null` and read is a timestamp, matching the documented field set.
- **Timezone rendering.** Store/API/UI all use UTC (`created_at AT TIME ZONE 'UTC'`), rendered as `YYYY-MM-DDTHH:MM:SSZ`; there is no local-time conversion or relative formatting in the page. Ordering uses the full-precision timestamp then id, so second-truncated display can look tied while the order is still correct.
- **Mentions.** Not implemented: no mention handling in `crates/githearth-server/src`, and mentions are assigned to a later milestone (`docs/plan/04-forge-roadmap.md:137`). Skipped.
- **Self review request is also a PR-validation gap.** A non-author can request themselves as reviewer (200). That is what makes the self-notification reachable; the notification defect is reported above, the permission question is left to the pull-request reviewer.
- **Access-loss vector.** Revoking a repo grant from carol would not remove access because org membership grants it; the test used `grace` (org `member` base role, which has no implicit repo read) with a write grant. Revoking that grant genuinely removed access (repo 404) and exercised the notification retention path.
- **Data snapshot at the end (read-only):** 64 notifications in lab12 (`t12` 59 = 58 reviewed + 1 merged; `carol` 3 requested; `dave` 1 requested; `grace` 1 reviewed). 66 notification jobs, all `done`, attempts 1/5, 0 dead, 0 pending/running, no duplicate `(event_id, account_id)` or `(kind, dedupe_key)` rows, no title/repo strings in payloads. `t12` unread 55, read 4; `grace` read 1. Two self rows total, both from the defect above.
- **Environment.** No restarts, migrations or writes to `acme`/`beta`/other labs. Repos created: `lab12/t12-notify`, `lab12/t12-notify-access`, `lab12/t12-notify-loss`. PATs were created only to verify the session-only 403s.
- **Re-runs.** The two notification defects and the PR-page failure were each reproduced twice before recording; the D13/I1-style false alarms (header names are lowercase) were re-tested with case-insensitive headers and pass.

## Round 2 — fixed build (2026-11-14)

Re-verification on the rebuilt image: `githearth-app:latest` built 2026-11-14 03:45 IST,
one minute after `fix/m4-lab` `7e1ded3` ("Fix M4 lab defects: … Self review requests do
not notify the requester") in worktree `wt-m4fix`; the observed behaviour matches that
commit. Container `githearth-app-1` was recreated by the campaign lead; I did not
restart, rebuild or migrate anything. Instance, accounts and lab12 fixtures as in
round 1; all DB access was read-only (`SELECT`).

Round 2 result: **13 cases re-run (10-case smoke + 3 defect checks): 12 pass, 1 fail,
0 skipped.** Both functional round-1 defects are fixed; the minor unread-badge defect
remains. Overall verdict unchanged: **PASS WITH DEFECTS**.

### Fix verification

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R1 | Self review-request (carol requests carol on PR #3) | no notification row; request may be accepted or refused | 200; carol API list 18→18; new job `bcdce0cb-…` `done` attempt 1; 0 rows for that event; lab12 self-pairs still 2 (both round-1 rows) | **PASS (fixed)** |
| R2 | Self review-request repeated (carol requests carol on PR #4) | no second self notification | 200; new job `ed3b22e5-…` `done` attempt 1; 0 notification rows; self-pairs still 2 | **PASS (fixed)** |
| R3 | PR pages / notification click-through | 200 for open and merged PR pages | session GET `pulls/1,2,3,4` → all 200 (open #3/#4 were 503); JSON detail open #3/#4 → 200 (was 500), merged #1/#2 → 200, `acme/web` #1 as alice → 200; t12 notification link to `t12-notify/pulls/4` → 200; carol's new reviewed/merged notification links to `t12-notify-access/pulls/1` → 200 | **PASS (fixed)** |
| R4 | Unread badge/count | visible unread count or badge | still none: nav is `<a href="/notifications" style="margin-left:auto">Notifications</a>` on `/notifications` and `/`; no numeric count/badge with t12 at 55 unread (DB), 50 unread returned by the capped API, 49 "Mark read" buttons + "Mark all read" on the page | **FAIL (unchanged)** |

Evidence for R1/R2: the fixed handler returns `JobOutcome::Done` when
`reviewer_id == requested_by` (`wt-m4fix` `crates/githearth-server/src/notifications.rs:215-219`).
The request is still inserted (two live self rows in `pull_request_review_requests`,
`dba80cc2-…` PR3 and `8d23e4c7-…` PR4) — only the notification is suppressed, which
matches the documented "skip self-notifications" rule. The round-1 self rows
(`74f048b9-…`, `f919e1dc-…`) remain as historical data; no cleanup is expected.

Evidence for R4: `crates/githearth-server/src/ui.rs:156` still renders the bare nav
link; `notifications_ui.rs:178,245` still uses `any_unread` only to show/hide
"Mark all read".

### 10-case smoke re-run

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Review notification: t12 approves carol's open PR in `t12-notify-access` | one `pull_request.reviewed`, actor t12, carol +1 | 201 review `ca0524c5-…`; carol 18→19; one row `c3202a56-…`, actor t12, PR `c9456fd0-…` | PASS |
| 2 | Merge notification: t12 merges that PR | one `pull_request.merged`, actor t12, carol +1 | merge intent 201 → state `merged`; carol 19→20; one row `51e36fdb-…`, actor t12 | PASS |
| 3 | Dedupe: replay review with same `Idempotency-Key: smoke-r2-k1` | same review, one row, no new notification | 201 same review `ca0524c5-…`; 1 review row; carol count unchanged at 19 | PASS |
| 4 | Dedupe: duplicate review request (dave already requested on PR #3) | one request, one job, one notification | 200; dave list 1→1; `pull_request_review_requests` row=1; no new `requested` job | PASS |
| 5 | Dedupe: repeat merge of the already-merged PR | 409, no extra notification | 409 `pull_request is not open`; carol count unchanged (24 before/after, concurrent cross-lab rows excluded by timing) | PASS |
| 6 | Unread filter (`?unread=true`) | only `read_at IS NULL`, capped | carol: 20 rows, all `read_at: null`; after marking one read → 19; that id excluded; `?unread=false` 20 | PASS |
| 7 | Mark read | 204, empty body, `read_at` set | 204, 0 bytes; `read_at=2026-11-13T22:21:51Z`, within created_at..now | PASS |
| 8 | Mark-read idempotency | second 204, first read time retained | second 204; `read_at` unchanged `2026-11-13T22:21:51Z` | PASS |
| 9 | Access-loss behaviour (grace on `t12-notify-loss`) | notification delivered, then survives revoked access without title/link | grant write 201; t12 reviews grace's PR → grace 1→2, row `d19509fc-…` actor t12; revoke 204; grace repo/PR GET 404; list still 200, six fields, no `GraceSecretTitle`; UI shows "A pull request you can no longer access", no title, no `/pulls/` link, Mark read present | PASS |
| 10 | PAT/session separation | PATs 403, anonymous 401 | `repo:read` and `repo:write` on list → 403 `access tokens cannot view notifications`; `repo:read` mark-read → 403 `read-only token`; `repo:write` mark-read → 403 session-only; no auth → 401 | PASS |

### Round 2 state snapshot (read-only)

- lab12 notifications: 68 total — t12 59 (55 unread), carol 5 (4 unread), dave 2, grace 2. The
  four new lab12 rows are the smoke review/merge (carol) and the review request to dave and
  review to grace; the only self-pairs are the two historical round-1 rows.
- lab12 notification jobs: 72, all `done`; 0 dead, 0 pending/running; max attempt 1/5; no
  duplicate `(kind, dedupe_key)`; no duplicate `(event_id, account_id)`; every event has one
  recipient.
- Concurrent campaign traffic was visible in DB/API counts (carol also holds lab08/lab10
  notifications); per-case assertions were therefore made by event/PR id, not global counts.
- Fixture changes this round: `t12-notify-access` PR #1 was merged by t12 during the smoke
  (previously open); grace's write grant on `t12-notify-loss` was granted and revoked again
  (back to no access). No repository/source changes, restarts or migrations.

### Updated verdict

**PASS WITH DEFECTS.** Self review-request notification and open-PR page/JSON availability
are fixed and verified. The notifications page still has no unread badge/count (minor,
unchanged from round 1).
