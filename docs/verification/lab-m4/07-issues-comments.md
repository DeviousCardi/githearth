# Issues and comments — M4 lab acceptance report

Tester: t07. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: PASS WITH DEFECTS
Cases: 173 run, 160 pass, 10 fail, 3 skipped.

Scope: issue and comment JSON API plus the server-rendered issue pages, exercised against `lab07` repositories `t07-issues` (private), `t07-public` (public), `t07-second` (archive/reopen) and `t07-concurrent` (concurrency). Accounts: `t07` (owner), `carol` (write), `dave` (maintain), `erin` (read), `frank` (triage), `grace` (member), `t08` (non-member) and `admin` (platform admin, observation only). No other organization was modified.

## Results

### Create, read and list (API)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C01 | create minimal issue | 201 with all issue fields | 201 number=1 fields=['author_id', 'body', 'closed_at', 'created_at', 'id', 'number', 'state', 'title', 'updated_at'] | PASS |
| C02 | create with body omitted | 201, body defaults to empty string | 201 body='' | PASS |
| C03 | create title='' | 400 | 400 body={'error': 'invalid input'} | PASS |
| C04 | create title=' \t\n ' | 400 (#138 whitespace-only) | 400 body={'error': 'invalid input'} | PASS |
| C05 | create 256-char title | 201 | 201 number=3 len=256 | PASS |
| C06 | create 257-char title | 400 | 400 body={'error': 'invalid input'} | PASS |
| C07 | create 65536-char body | 201 | 201 number=4 bodylen=65536 | PASS |
| C08 | create 65537-char body | 400 | 400 body={'error': 'invalid input'} | PASS |
| C09 | create unicode title/body, round-trip | 201 and exact strings | 201 title_ok=True body_ok=True | PASS |
| C10 | create with unknown field "labels" | 400 (deny_unknown_fields) | 400 body={'error': 'invalid input'} | PASS |
| C11 | create malformed JSON | 400 | 400 body={'error': 'invalid input'} | PASS |
| C12 | create without title | 400 | 400 body={'error': 'invalid input'} | PASS |
| C13 | create as erin | 201 | 201 body={'id': '7a6a4772-3088-4dc0-bd78-8e4cc235212a', 'number': 6, 'title': 'C13 created by erin', 'body': '', 'state': 'open', 'author_id': '7fd7e… | PASS |
| C14 | create as frank | 201 | 201 body={'id': 'f8e26d80-7fe1-4df7-a314-cf60c4e7c2a7', 'number': 7, 'title': 'C14 created by frank', 'body': '', 'state': 'open', 'author_id': 'bf50… | PASS |
| C15 | create as carol | 201 | 201 body={'id': '11493190-5c93-492d-acc9-a66914867be8', 'number': 8, 'title': 'C15 created by carol', 'body': '', 'state': 'open', 'author_id': 'efa5… | PASS |
| C16 | create as dave | 201 | 201 body={'id': 'ae726862-0b39-430e-bdad-3dc771612813', 'number': 9, 'title': 'C16 created by dave', 'body': '', 'state': 'open', 'author_id': '565f9… | PASS |
| C17 | create as grace | denied (403 or 404) | 404 denied (role masking) | PASS |
| C18 | create anonymous | 401 | 401 body={'error': 'invalid or expired credentials'} | PASS |
| C19 | idempotency replay same key+body | same issue returned, no duplicate | 201/201 numbers=10/10 ids_equal=True | PASS |
| C20 | idempotency key reused with different body | 422 KEY_REUSED (or 409) | 422 body={'error': 'idempotency key reused with different request'} | PASS |
| C21 | invalid Idempotency-Key (space) | 400 | 400 body={'error': 'invalid idempotency key'} | PASS |
| C22 | duplicate titles allowed | two distinct issues | 201/201 numbers=11/12 | PASS |
| C23 | issue number sequencing | numbers 1..N with no gaps | numbers=[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] | PASS |
| C24 | per-repository numbering independent | 201 with number 1 | 201 number=1 | PASS |
| C25 | 8 concurrent issue creates | all 201, unique consecutive numbers 1..8 | results=[(201, 1), (201, 3), (201, 4), (201, 2), (201, 6), (201, 5), (201, 7), (201, 8)] | PASS |
| C26 | create in unknown repo | 404 | 404 body={'error': 'repository not found'} | PASS |
| C27 | create in another (non-member) org | 404 | 404 body={'error': 'organization not found'} | PASS |
| C28 | read issue fields | full JSON with UTC timestamps, open, author | 200 state=open closed_at=None author_ok=True ts=2026-11-13T21:16:31Z | PASS |
| C29 | read unknown issue number | 404 | 404 body={'error': 'issue not found'} | PASS |
| C30 | read issue as non-member t08 | 404 | 404 body={'error': 'issue not found'} | PASS |
| C31 | read private-repo issue anonymous | 401/404 denial without disclosing existence | 401 invalid or expired credentials | PASS |
| C32 | read issue as read-grant erin | 200 | 200 | PASS |
| C33 | read issue as org member grace (base role) | denied (403/404) | 404 body={'error': 'issue not found'} | PASS |
| C34 | read public-repo issue anonymous | anonymous public issue read (not implemented by design) | 401 invalid or expired credentials | SKIP |
| C35 | list default state=open | only open issues | open=12 closed=0 all=12 | PASS |
| C36 | list state=closed filter | only closed issues (0 before any close) | closed=0 | PASS |
| C37 | list state=all | open+closed | all=12 | PASS |
| C38 | list invalid state filter | 400 | 400 body={'error': 'invalid input'} | PASS |
| C39 | list pagination limit=2 + after | two pages, descending, no overlap | page1=[12, 11] next=11 page2=[10, 9] | PASS |
| C40 | list limit bounds | 400 for 0 and 101 | limit0=400 limit101=400 | PASS |
| C41 | list newest first | numbers strictly descending | head=[12, 11, 10, 9, 8] | PASS |
| C42 | list scoped to one repository | only t07-second issues | t07-second items=[(1, 'C24 first issue in second repo')] | PASS |
| C43 | list issues as non-member t08 | 404 | 404 body={'error': 'not found'} | PASS |

### Update and comments (API)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| C44 | PATCH title | 200, title changed, updated_at moves | 200 title='C44 patched title' updated_at=2026-11-13T21:16:32Z->2026-11-13T21:17:24Z | PASS |
| C45 | PATCH body persists | 200 and GET returns new body | 200 persisted='C45 patched body\nsecond line' | PASS |
| C46 | PATCH open -> closed | 200, state closed, closed_at set | 200 state=closed closed_at=2026-11-13T21:17:24Z | PASS |
| C47 | PATCH closed -> open | 200, state open, closed_at cleared | 200 state=open closed_at=None | PASS |
| C48 | PATCH invalid state | 400 | 400 body={'error': 'invalid input'} | PASS |
| C49 | PATCH whitespace-only title (#138 regression) | 400 | 400 body={'error': 'invalid input'} | PASS |
| C50 | PATCH empty title | 400 | 400 body={'error': 'invalid input'} | PASS |
| C51 | PATCH 257-char title | 400 | 400 | PASS |
| C52 | PATCH 65537-char body | 400 | 400 | PASS |
| C53 | PATCH empty object | 400 | 400 body={'error': 'invalid input'} | PASS |
| C54 | PATCH unknown field | 400 | 400 body={'error': 'invalid input'} | PASS |
| C55 | PATCH as erin | 403 | 403 body={'error': 'not permitted'} | PASS |
| C56 | PATCH as grace | 404 | 404 body={'error': 'issue not found'} | PASS |
| C57 | PATCH as frank | 200 | 200 body={'id': '2889c7fe-d0ff-4a02-8755-938dbef157c4', 'number': 11, 'title': 'C44 patched title', 'body': ' | PASS |
| C58 | PATCH as carol | 200 | 200 body={'id': '2889c7fe-d0ff-4a02-8755-938dbef157c4', 'number': 11, 'title': 'C44 patched title', 'body': ' | PASS |
| C59 | PATCH as dave | 200 | 200 body={'id': '2889c7fe-d0ff-4a02-8755-938dbef157c4', 'number': 11, 'title': 'C44 patched title', 'body': ' | PASS |
| C60 | PATCH unknown issue number | 404 | 404 body={'error': 'issue not found'} | PASS |
| C61 | PATCH idempotency replay | 200 both, identical updated_at | 200/200 updated_at=2026-11-13T21:17:24Z/2026-11-13T21:17:24Z | PASS |
| C62 | create issue in archived repo | refused (403 or 409) | 403 not permitted | PASS |
| C63 | PATCH issue in archived repo | refused (403 or 409) | 403 not permitted | PASS |
| C64 | comment on archived repo issue | refused (403 or 409) | 403 not permitted | PASS |
| C65 | read issue in archived repo | 200 (archived stays readable) | 200 | PASS |
| C66 | PATCH after unarchive | 200 | 200 | PASS |
| C67 | PATCH as non-member t08 | 404 | 404 body={'error': 'issue not found'} | PASS |
| C68 | create comment | 201 fields, seq=1 | 201 seq=1 keys=['author_id', 'body', 'created_at', 'id', 'seq', 'updated_at'] | PASS |
| C69 | comment body='' | 400 | 400 body={'error': 'invalid input'} | PASS |
| C70 | comment body=' \t\n ' | 400 | 400 body={'error': 'invalid input'} | PASS |
| C71 | comment 65536-char body | 201 | 201 len=65536 | PASS |
| C72 | comment 65537-char body | 400 | 400 | PASS |
| C73 | comment idempotency replay | same comment id, one row | 201/201 ids_equal=True seq=3/3 | PASS |
| C74 | comment idempotency key different body | 422 | 422 body={'error': 'idempotency key reused with different request'} | PASS |
| C75 | list comments ordered by seq | ascending sequence, fields complete | 200 seqs=[1, 2, 3] | PASS |
| C76 | comment pagination limit=1 + after | next cursor, later seq | page1=[1] next=1 page2=[2] | PASS |
| C77 | comment on closed issue + issue updated_at | 201, issue stays closed, updated_at moves | 201 state=closed updated_at=2026-11-13T21:17:24Z->2026-11-13T21:17:25Z | PASS |
| C78 | comment as non-member t08 | 404 | 404 body={'error': 'issue not found'} | PASS |
| C79 | comment anonymous | 401 | 401 | PASS |
| C80 | comment as read-grant erin | 201 (issue.comment is in read group) | 201 | PASS |
| C82 | comment on unknown issue | 404 | 404 body={'error': 'issue not found'} | PASS |
| C83 | comment list bounds | 400 for limit 0/101 and after<1 | 400/400/400 | PASS |
| C84 | API stores comment body verbatim | 201 and exact string (escaping is UI) | 201 exact=True | PASS |
| C85 | comment edit/delete endpoints | not implemented; docs list only create/list | no PATCH/DELETE route in issues.rs router() | SKIP |
| C86 | read issue as write-grant carol | 200 | 200 | PASS |
| C87 | list issues as read-grant erin | 200 | 200 | PASS |
| C88 | list comments as read-grant erin | 200 | 200 | PASS |
| C89 | read negative issue number | 404 | 404 body={'error': 'issue not found'} | PASS |

### Browser UI

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| U01 | issue list page unauthenticated | 303 redirect to /login | 303 location=/login | PASS |
| U02 | issue list page (open) | 200, table rows match API open count | 200 rows=16 api_open=16 | PASS |
| U03 | issue list closed filter | 200, one closed issue shown | 200 rows=2 api_closed=2 | PASS |
| U04 | issue list all filter | 200, rows = open+closed | 200 rows=18 api_all=18 | PASS |
| U05 | issue list pagination 30/page newest first | 30 rows + Next, page 2 has rest, #31 first | 200 rows=30 first=32 total=32 page2=['2', '1'] | PASS |
| U06 | issue detail page | 200 with title, body, comments | 200 title=True body=True comments=True | PASS |
| U07 | detail page HTML-escapes title/body | no raw script/img, escaped entities | 200 raw_script=False escaped=True | PASS |
| U07b | detail page HTML-escapes comment body | script escaped, no raw tag | 200 raw=False escaped=True | PASS |
| U08 | new issue form | 200 with CSRF, idempotency key, labels, maxlength | 200 csrf=True key=True labels=True maxlength=True | PASS |
| U09 | create via form without CSRF token | 403 Forbidden page | 403 invalid_csrf=True | PASS |
| U10 | create via form with wrong CSRF token | 403 | 403 | PASS |
| U11 | create form whitespace-only title | 400 page with alert, no redirect | 400 alert=True | PASS |
| U12 | create issue via browser form | 303 to new issue, body renders | 303 loc=/organizations/lab07/repositories/t07-issues/issues/19 body_visible=True | PASS |
| U13 | double-submit form (same idempotency key) | same redirect, single issue | 303 same_loc=True added=1 | PASS |
| U14 | create form 257-char title (maxlength bypassed) | 400 page with alert | 400 alert=True | PASS |
| U15 | edit form prefilled | 200 with current title/body and labels | 200 title_prefill=True labels=True | PASS |
| U16 | edit form without valid CSRF | 403 | 403 | PASS |
| U17 | edit form empty title | 400 alert | 400 alert=True | PASS |
| U18 | edit issue via browser form | 303 redirect and values persist | 303 title='U18 edited via UI' body='U18 body via UI' | PASS |
| U19 | close issue via button | 303 and state closed; page offers Reopen | 303 state=closed | PASS |
| U19b | closed issue page shows Reopen button | Reopen issue present | 200 reopen=True edit=True | PASS |
| U20 | state form invalid value | 400 invalid request page | 400 invalid=True | PASS |
| U21 | state form without valid CSRF | 403 | 403 | PASS |
| U22 | comment via browser form | 303 and comment visible | 303 visible=True | PASS |
| U23 | comment form blank body | 400 page with alert | 400 alert=True | PASS |
| U24 | comment form without valid CSRF | 403 | 403 | PASS |
| U25 | accessible labels/landmarks | labels, th scope, aria-label present | labels=True th_scope=True aria=True | PASS |
| U26 | issue page as non-member browser | 404 page | 404 notfound=True | PASS |
| U27 | read-grant issue page | 200, no Edit/Close controls, comment form present | 200 edit=False close=False | PASS |
| U27b | read-grant edit page | 404 (hidden) | 404 | PASS |
| U28 | triage issue page | 200 with Edit and close/reopen controls | 200 edit=True statebtn=True | PASS |
| U29 | state filter tabs | open current, closed/all links | 200 tabs_ok=True | PASS |
| U30 | UI list invalid state filter | 400 invalid request page | 400 invalid=True | PASS |
| U31 | UI unknown issue number | 404 Not found page | 404 notfound=True | PASS |
| U32 | double-submit comment form (same key) | 303 both, one comment row | 303/303 added=1 | PASS |
| U33 | UI list row count equals API count (all) | 21 rows | 200 ui_rows=21 api=21 | PASS |
| U34 | edit form escapes dangerous title | 200, escaped value attribute | 200 raw=False escaped=True | PASS |
| U34b | list page escapes dangerous title | no raw <script> tag | 200 raw=False | PASS |
| U35 | create form error redisplay escapes submitted title | 400, escaped value, alert | 400 raw=False escaped=True | PASS |
| U36 | UI comment pagination 30/page | 30 + More link, then 1 + First link | 200 page1=30 next=True page2=1 first_link=True | PASS |
| U37 | UI comment on closed issue | 303 and comment persisted/displayable | 303 api_present=True page_mentions=True | PASS |
| U38 | post comment on a >30-comment issue redirects to oldest page | new comment visible after 303 (or redirect to last page) | 303 loc=/organizations/lab07/repositories/t07-issues/issues/27 comments_shown=30 new_visible=False visible_page2=True | FAIL |

### Audit and cross-org

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A01 | owner reads org audit log | 200 and issue.created/updated/closed/reopened/commented present | 200 events=94 missing=[] | PASS |
| A02 | audit events ordered ascending by seq | seq strictly increasing | first=1 last=94 | PASS |
| A03 | issue.created actor attribution | erin appears as actor for her issue | erin distinct_issue_created_actors=5 | PASS |
| A04 | audit as carol | 403 (not owner) | 403 body={'error': 'not permitted'} | PASS |
| A05 | audit as dave | 403 (not owner) | 403 body={'error': 'not permitted'} | PASS |
| A06 | audit as erin | 403 (not owner) | 403 body={'error': 'not permitted'} | PASS |
| A07 | audit as frank | 403 (not owner) | 403 body={'error': 'not permitted'} | PASS |
| A08 | audit as grace | 403 (not owner) | 403 body={'error': 'not permitted'} | PASS |
| A09 | audit as non-member t08 | 404 | 404 body={'error': 'organization not found'} | PASS |
| A10 | audit anonymous | 401 | 401 | PASS |
| A11 | audit chain verify | 200 ok=true, rows>0, no bad seq | 200 {'rows': 94, 'ok': True, 'first_bad_seq': None} | PASS |
| A12 | cross-org audit access | 404 | 404 body={'error': 'organization not found'} | PASS |
| A13 | cross-org issue read (lab08) | 404 | 404 body={'error': 'repository not found'} | PASS |
| A14 | list comments as non-member | 404 | 404 body={'error': 'issue not found'} | PASS |
| A15 | create 256-char emoji title | 201 (counts characters, not bytes) | 201 number=20 | PASS |
| A16 | create 257-char emoji title | 400 | 400 | PASS |
| A17 | idempotency key 128 chars | 201 | 201 | PASS |
| A18 | idempotency key 129 chars | 400 | 400 body={'error': 'invalid idempotency key'} | PASS |
| A19 | platform admin reads non-member org issue | 404/403 (no tenant-admin bypass) | 404 body={'error': 'issue not found'} | PASS |
| A20 | platform admin reads non-member org audit | 404/403 | 404 body={'error': 'organization not found'} | PASS |
| A21 | audit chain verify after full campaign writes | ok=true, no bad seq | 200 {'rows': 161, 'ok': True, 'first_bad_seq': None} | PASS |
| A22 | cross-org issue read in existing lab08 repo t08-xrepo | 404 (non-member) | 404 body={'error': 'issue not found'} | PASS |
| A23 | cross-org issue read in existing lab08 repo t08-pulls | 404 (non-member) | 404 body={'error': 'issue not found'} | PASS |

### Boundary and adversarial cases

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| E01 | create issue title containing NUL | 400 invalid input, never 5xx | 503 body={'error': 'database unavailable'} | FAIL |
| E02 | create issue body containing NUL | 400 | 503 body={'error': 'database unavailable'} | FAIL |
| E03 | create comment containing NUL | 400 | 503 body={'error': 'database unavailable'} | FAIL |
| E04 | readiness after NUL errors | 200 | 200 | PASS |
| E05 | E05 zero-width/BOM-only title (re-run) | 400 blank title | 201 number=30 stored_title='\u200b\u200b\u200b' | FAIL |
| E06 | E06 zero-width/BOM-only title (re-run) | 400 blank title | 201 number=31 stored_title='\ufeff' | FAIL |
| E07 | create title of NBSP only | 400 (Unicode whitespace trimmed) | 400 | PASS |
| E08 | request body over 524288-byte limit | 413 payload too large | 413 body=length limit exceeded | PASS |
| E09 | 6 concurrent comments | all 201, unique monotonic seq | results=[(201, 13), (201, 8), (201, 11), (201, 10), (201, 9), (201, 12)] | PASS |
| E10 | idempotent replay emits one audit event | issue.created count +1 only | 201/201 created_events 66->67 | PASS |
| E11 | session cookie attributes | HttpOnly and SameSite=Lax (Secure off on loopback) | githearth_session=<redacted>; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600, githearth_csrf=; Pa… | PASS |
| E12 | CSRF token from another session refused | 403 | 403 | PASS |
| E13 | comment markdown rendering | not implemented; plain text escaped | no markdown crate or renderer; comments_ui emits <div style="white-space:pre-wrap">escaped body</div> | SKIP |
| E14 | PATCH title containing NUL | 400 invalid input | 503 body={'error': 'database unavailable'} | FAIL |
| E15 | UI create form title containing NUL | 400 page, not 5xx | 503 unavailable=True | FAIL |
| E16 | issue API Cache-Control | no-store | 200 cc=no-store | PASS |
| E17 | same idempotency key across accounts | two independent issues | 201/201 nums=28/29 distinct=True | PASS |
| E18 | issue numbers remain gapless after rejected creates | 1..N | count=29 min=1 max=29 gaps=[] | PASS |
| E19 | UI create form zero-width-only title | 400 page with alert | 303 location=/organizations/lab07/repositories/t07-issues/issues/32 | FAIL |
| E20 | comment body of zero-width spaces | 400 blank comment | 201 seq=2 | FAIL |

## Defects

### NUL byte in an issue or comment text field returns 503 "database unavailable"

- Severity: minor
- Repro:

```bash
TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"t07","password":"<test-password>"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

# title
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"title":"nul\u0000title"}'

# body
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"title":"ok","body":"a\u0000b"}'

# comment
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues/1/comments \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"body":"c\u0000d"}'

# PATCH
curl -i -X PATCH http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues/1 \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"title":"x\u0000y"}'
```

  The browser create form has the same outcome: a `title=ui%00nul` form POST returns the 503 page (E15).
- Expected: 400 `invalid input`, like every other rejected title/body, because the JSON is valid and the database is healthy. A client-controlled character must never produce a 5xx.
- Actual: `503 {"error":"database unavailable"}` for create title (E01), create body (E02), comment (E03), PATCH title (E14) and the UI form (E15). `/health/ready` stayed 200 immediately afterwards (E04), and normal issue creation and audit writes continued (E17/E18), so this is not a database outage or environment flake. Reproduced five times across four endpoints, including after re-login.
- Evidence:
  - `crates/githearth-server/src/issues.rs:187-197` (`new_issue`) validates only `trim` and length; lines 398-418 (`validate_patch`) and 554-560 (`new_comment`) do the same. No control-character rejection exists.
  - `crates/githearth-server/migrations/0006_issues.sql` stores `title`/`body` as PostgreSQL `text`; PostgreSQL rejects the NUL byte (`0x00`) as invalid UTF-8, returning a database error rather than a constraint error.
  - `crates/githearth-server/src/lib.rs:432-444` converts every non-unique `sqlx::Error` into `503 "database unavailable"`, so the driver error is misclassified as an outage and logged only as a generic `category = "database"` error.
- Scope: systematic for every issue/comment text field and every write surface (API create/update, browser create/comment). NUL alone is affected; NBSP and ordinary whitespace are handled correctly (E07).

### Blank-title and blank-comment validation is bypassed with zero-width characters

- Severity: minor
- Repro:

```bash
# title of three zero-width spaces -> 201
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"title":"\u200b\u200b\u200b"}'

# title of a BOM -> 201
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"title":"\ufeff"}'

# comment of two zero-width spaces -> 201
curl -i -X POST http://127.0.0.1:8080/api/v1/organizations/lab07/repositories/t07-issues/issues/1/comments \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  --data-binary '{"body":"\u200b\u200b"}'
```

  The browser form also accepts it: `title=%E2%80%8B%E2%80%8B` POSTs successfully and redirects 303 to the new issue (E19).
- Expected: 400 `invalid input`, as for `""`, `"   \t\n  "`, and NBSP-only titles, which are all correctly refused (C03/C04/C49/C50/C70/E07). The rule is "blank titles and comments are refused"; a visually empty title/comment is blank.
- Actual: 201 for a title of U+200B x3 (issue #30), a title of U+FEFF (issue #31), and a comment of U+200B x2 (E20). The UI creates issue #32 with a 303.
- Evidence:
  - `crates/githearth-server/src/issues.rs:190` uses `input.title.trim().is_empty()`; lines 402-406 and 556 do the same for PATCH and comments. `crates/githearth-server/src/issues_ui.rs:166-172` (`title_ok`, `comment_ok`) repeats the check.
  - Rust's `str::trim` removes Unicode `White_Space` only. U+200B ZERO WIDTH SPACE and U+FEFF ZERO WIDTH NO-BREAK SPACE are `Cf` format characters, so `trim()` leaves them and the title/comment is accepted.
- Scope: systematic for title and comment blank validation on both the JSON API and the browser forms; ordinary whitespace and NBSP are unaffected.

### Posting a comment on a thread longer than 30 comments redirects to the oldest page and hides the new comment

- Severity: minor
- Repro:
  1. Use issue #27 ("U36 comment pagination issue"), which has 31+ comments.
  2. `GET /organizations/lab07/repositories/t07-issues/issues/27`; page shows the first 30 comments and a `More comments` link.
  3. Submit the comment form with a new body (`U38 newest comment on long thread`). The POST returns `303 Location: /organizations/lab07/repositories/t07-issues/issues/27`.
  4. Follow the redirect: 30 comment articles, new body absent.
  5. `GET .../issues/27?after=30`: the new body is present.
- Expected: after a successful comment submission the browser should show the comment that was just posted (redirect to the last comment page or the comment anchor), or at least the page containing it.
- Actual: 303 to page 1, which lists comments oldest-first in pages of 30. The comment is persisted but appears to vanish on threads of 31+ comments; the user must find and click `More comments` (and repeatedly for longer threads).
- Evidence: `crates/githearth-server/src/issues_ui.rs:632` redirects to `{base}/issues/{number}` with no cursor; `issues_ui.rs:463-471` loads comments with `limit: None` and `after` from the query; `crates/githearth-server/src/issues.rs:680-687` defaults to 30 and queries `ORDER BY seq` ascending. U36 confirms the page-2 link and U38 confirms the hidden comment.
- Scope: systematic for any issue with more than 30 comments; not seen on short threads (U37 on issue #12 passes).

## Notes / skipped

- **Role ladder:** the campaign hypothesis "read cannot create" does not match the documented policy. ADR 0002 and `crates/githearth-authz/policies/githearth.cedarschema` put `issue.create` and `issue.comment` in the `read` group, while `issue.close`/edit are in `triage`. Observed: `erin` (read) creates and comments (C13/C80), `frank` (triage), `carol` (write) and `dave` (maintain) create/edit/close (C14-C16, C57-C59), `grace` (member) is denied because a base member has no `repository.read` (C17/C56). This is consistent with the ADR, so C13/C80 are recorded as PASS; the brief's expected "read cannot create" is treated as an incorrect hypothesis, not a product defect.
- **Anonymous issue reads (C34):** every issue API handler and issue page calls `authenticate` before loading the repository, so an unauthenticated request gets 401 even for a public repository. `docs/foundation.md:166` only promises issue pages for signed-in browsers, and the M4 qualification's anonymous public-read scenario is about Git HTTP and the repository page (which returns 200 anonymously for `t07-public`), not the issue pages. Recorded as SKIP rather than a defect; if anonymous issue reads are intended, this needs a separate handler path.
- **Private anonymous (C31):** 401 rather than 404; authentication is required before repository lookup, so existence is not disclosed. Treated as PASS with the note that the status differs from the campaign expectation.
- **Archived repositories (C62-C64):** mutations are refused with 403 by the Cedar `archived-read-only` / `archived-no-new-content` forbids, not the handler's 409 `not_active` branch (archived repositories keep status `active`). The requirement "refused" is met; reads still work (C65) and editing resumes after unarchive (C66).
- **Comment edit/delete (C85):** not implemented. `issues.rs` exposes only create/list for comments; `docs/foundation.md:166` mentions editing/closing issues, not comments. Skipped.
- **Markdown rendering (E13):** no Markdown renderer exists. Comment bodies are escaped and shown as plain text with `white-space:pre-wrap` (`issues_ui.rs:484`); the docs do not claim Markdown. Skipped. HTML escaping itself is verified (U07/U07b/U34/U34b/U35).
- **Re-runs:** the three defects were reproduced after the first observation (NUL five times across create/body/comment/PATCH/UI; zero-width titles twice via API plus the UI form, and a zero-width comment; comment redirect on the 31+ comment issue). All other failures during the campaign were harness expectations, corrected before recording: the UI list title check after title edits, an authenticated list request following a login redirect (200 from `/login`), and a new comment on a 31-comment issue being on page 2. No product failure was dismissed without a re-check.
- **Issue counts in the UI:** the issue list has no numeric count widget; the pages were checked against the API instead: rendered rows equaled the API count for open, closed and all filters (U02-U04, U33), and pagination showed exactly 30 rows plus a Next link (U05).
- **Health and isolation:** `/health/live` and `/health/ready` stayed 200 throughout; audit chain verification at the end of the write campaign was `rows=161, ok=true, first_bad_seq=null` (A21). `t07` could not read existing `lab08` repositories (`t08-xrepo`, `t08-pulls`) or their issues (A22/A23), and `t08`/`admin` could not read `lab07` issues or audit (C30/C43/A19/A20). Audit reads were owner-only; write/maintain/read/triage/member all got 403 and non-members 404 (A04-A09).
- Test data remains in `lab07` (`t07-issues`, `t07-public`, `t07-second`, `t07-concurrent`, issues #1-#33 and comments). No restart, migration, or out-of-org mutation was performed.

## Round 2 — fixed build (2026-11-14)

Re-verification on the rebuilt instance. Image: `githearth-app` `sha256:c0847c6e…` (built 2026-11-13T22:15:49Z, container started 22:16:22Z). Account `t07`, org `lab07`, same repositories; `/health/live` and `/health/ready` returned 200. No repository edit, restart, migration or out-of-org mutation.

**Round 2 verdict: PASS WITH DEFECTS.** The zero-width/BOM blank-title and blank-comment defect is fixed on both the JSON API and the browser forms. The NUL-byte 503 and the >30-comment post redirect defect remain; both were reproduced twice on this build. The 12-case smoke pass held with no regressions.

Cases: 44 run, 36 pass, 8 fail, 0 skipped.

### Targeted fixes

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| Z01 | create title U+200B x3 | 400 | 400 `{"error":"invalid input"}` | PASS |
| Z02 | create title U+FEFF | 400 | 400 | PASS |
| Z03 | create title U+200B/U+FEFF/U+200D/U+2060 mixed | 400 | 400 | PASS |
| Z04 | create title whitespace + U+200B + U+FEFF | 400 | 400 | PASS |
| Z05 | comment body U+200B x2 | 400 | 400 | PASS |
| Z06 | comment body U+FEFF | 400 | 400 | PASS |
| Z07 | comment body whitespace + U+200B + tab | 400 | 400 | PASS |
| Z08 | PATCH title U+200B x3 | 400 | 400 | PASS |
| Z09 | create title with U+200B embedded in real text | 201 (fix must not over-reject) | 201 number=34, stored verbatim `'R2 a\u200bb title ok'` | PASS |
| Z10 | UI create form zero-width-only title | 400 page with alert, no issue created | 400, alert present, 0 issues added | PASS |
| Z11 | UI comment form zero-width-only body | 400 page with alert, no comment created | 400, alert present, 0 comments added | PASS |
| Z12 | UI edit form zero-width-only title | 400 page with error, no change | 400, `.error` message "A title of 1 to 256 characters is required" | PASS |
| N01 | create title containing NUL | 400, never 5xx | 503 `{"error":"database unavailable"}` | FAIL |
| N02 | create body containing NUL | 400 | 503 `{"error":"database unavailable"}` | FAIL |
| N03 | comment body containing NUL | 400 | 503 `{"error":"database unavailable"}` | FAIL |
| N04 | PATCH title containing NUL | 400 | 503 `{"error":"database unavailable"}` | FAIL |
| N05 | UI create form title containing NUL | 400 page, not 5xx | 503 "database unavailable" page | FAIL |
| N06 | UI comment form body containing NUL | 400 page, not 5xx | 503 "database unavailable" page | FAIL |
| N07 | readiness after NUL attempts | 200 | 200 | PASS |
| R01 | issue #27 preconditions | >30 comments, page 1 = 30 + More | 33 comments, 30 articles, `after=30` link | PASS |
| R02 | post comment on >30-comment issue | 303 | 303 | PASS |
| R03 | new comment visible after following redirect | comment body visible on landing page | `Location: …/issues/27`; page 1 still 30 articles, new body absent (present at `?after=30`) | FAIL |
| R04 | redirect targets last comment page or anchor | `after=` or `#` in Location | no cursor, no anchor | FAIL |

NUL re-run detail (two independent runs): all four API write surfaces returned 503 in both runs, and both browser forms returned 503 in the first run, while `/health/ready` stayed 200 and surrounding writes succeeded. App log shows `{"message":"database operation failed","category":"database"}` for each attempt — the same misclassification as round 1, not an outage. Zero-width re-check: eight API rejection variants (four create titles, three comment bodies, one PATCH title) and three UI forms all rejected; the one title with an embedded U+200B in otherwise real text was accepted and stored byte-for-byte, so the fix rejects only blank-equivalent values.

### 12-case smoke (fixed build)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S01 | create issue | 201 with all issue fields | 201 number=35, 9 fields | PASS |
| S02 | PATCH title | 200, title changed | 200 `R2 smoke patched` | PASS |
| S03 | close issue | 200, `state=closed`, `closed_at` set | 200 closed at 2026-11-13T22:18:35Z | PASS |
| S04 | reopen issue | 200, `state=open`, `closed_at=null` | 200 open, null | PASS |
| S05 | issue pagination limit=2 + after | two disjoint descending pages | `[35,34]` then `[33,32]` | PASS |
| S06 | comment pagination limit=1 + after | seq 1 then seq 2 | `[1] next=1`, `[2]` | PASS |
| S07 | create form without CSRF | 403 | 403 | PASS |
| S08 | create form with wrong CSRF | 403 | 403 | PASS |
| S09 | detail + list HTML escaping of title/body | escaped, no raw tag | raw script absent, `&lt;script&gt;` present in both | PASS |
| S10 | cross-org read t07 → lab08 `t08-xrepo` | 404 | 404 `{"error":"issue not found"}` | PASS |
| S11 | non-member t08 → lab07 issue | 404 | 404 | PASS |
| S12 | audit chain verify | `ok=true` | 200 `{"rows":170,"ok":true,"first_bad_seq":null}` | PASS |

### Extra smoke assertions

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| X01 | UI list pagination vs API (all) | 30 rows + Next, then remainder | API 38; page 1 = 30 rows + Next `after=9`; page 2 = 8 rows | PASS |
| X02 | comment form without CSRF | 403 | 403 | PASS |
| X03 | comment body escaping | no raw script tag | posted 201, raw=False, escaped=True | PASS |
| X04 | cross-org issue page t07 → lab08 | 404 page | 404 not-found page | PASS |
| X05 | cross-org PATCH t07 → lab08 | 404 | 404 | PASS |
| X06 | idempotency replay same key+body | same issue, one row | 201/201, number 37 both, same id | PASS |
| X07 | UI close then reopen | 303 both, closed then open | 303 closed, 303 open, `closed_at` cleared | PASS |
| X08 | normal UI create | 303, new title visible | 303 → issue 38, title visible | PASS |
| X09 | normal UI comment on short thread | 303, comment visible | 303 → issue 35, body visible | PASS |

### Round 2 defect status

- **Zero-width/BOM blank title and comment — FIXED.** All U+200B/U+FEFF-only titles and comments return 400 on API create, PATCH, comment create, and the three browser forms; embedded zero-width inside real text remains accepted. Round-1 E05/E06/E19/E20 now pass.
- **NUL byte in a text field → 503 — REMAINS (minor).** Unchanged: N01-N06 are still 503 `database unavailable` and logged under `category = "database"`. The round-1 repro and evidence still apply.
- **Comment on a >30-comment issue redirects to page 1 — REMAINS (minor).** Unchanged: R02/R03/R04. The comment persists, but the 303 target has no cursor or anchor, so the new comment is hidden on page 2. Two probes on this build both landed on page 1 with 30 articles and the new body absent.

### Round 2 notes

- Harness corrections, not product findings: the edit-form error renders with class `error` rather than `alert` (Z12), response headers arrive lower-case so `location` must be read case-insensitively (R02-R04), and UI pagination links contain `&amp;` (X01). After correction all three behaved as expected.
- Issue numbering stayed gapless 1..38 through all rejected creates; the two comments left by the redirect probe are on issue #27. New round-2 data: issues #34-#38, plus comments on #27 and #35. No other organization was touched.
