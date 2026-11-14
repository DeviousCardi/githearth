# Issues and comments — M4 closure round 3

Tester: t05 (owner, `lab05`). Date: 2026-11-14. Instance: main `800cad4`.
Verdict: PASS
Cases: 96 run, 96 pass, 0 fail, 0 skipped.

Issue #159 re-verified end to end. Primary repository: `lab05/r3-i159-121403` (private); a second issue in the same repository repeated the 30/31/61 boundaries independently.

## Results
| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | UI new-issue form renders with CSRF | 200 and form + csrf | 200, form=True | Pass |
| 2 | UI create issue valid -> 303 issue page | 303 /issues/1 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/1 | Pass |
| 3 | UI issue page escapes title/body (XSS) | 200, escaped, no raw HTML | 200, escaped=True, raw=False | Pass |
| 4 | UI issue list shows created issue escaped | 200, #1 escaped | 200, listed=True | Pass |
| 5 | UI create blank title rejected | 400 validation | 400 | Pass |
| 6 | UI create zero-width/space title rejected | 400 | 400 | Pass |
| 7 | UI create 257-char title rejected | 400 | 400 | Pass |
| 8 | UI create missing CSRF -> 403 | 403 | 403 | Pass |
| 9 | UI create wrong CSRF -> 403 | 403 | 403 | Pass |
| 10 | UI create without session -> 303 login | 303 login | 303 /login | Pass |
| 11 | UI edit form pre-fills escaped values | 200 pre-filled | 200 | Pass |
| 12 | UI edit issue valid -> 303 | 303 /issues/1 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/1 | Pass |
| 13 | UI edit persists title/body | 200 updated | 200 | Pass |
| 14 | UI edit blank title rejected, unchanged | 400, old values | 400, stored='UI issue edited' | Pass |
| 15 | UI edit 257-char title rejected | 400 | 400 | Pass |
| 16 | UI edit wrong CSRF -> 403 | 403 | 403 | Pass |
| 17 | UI close issue -> 303 | 303 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/1 | Pass |
| 18 | UI closed issue shows closed + Reopen | closed, button Reopen | state=closed closed_at=True reopen=True | Pass |
| 19 | UI issue list state filter | closed lists #1, open not | closed has #1=True, open has #1=False | Pass |
| 20 | UI reopen issue -> 303 | 303 | 303 | Pass |
| 21 | UI reopened issue shows open + Close | open, button Close | state=open button=True | Pass |
| 22 | UI invalid state value -> 400 | 400 | 400 | Pass |
| 23 | UI state missing CSRF -> 403 | 403 | 403 | Pass |
| 24 | UI state wrong CSRF -> 403 | 403 | 403 | Pass |
| 25 | UI state on missing issue -> 404 | 404 | 404 | Pass |
| 26 | UI missing/invalid issue number -> 404 | 404/404 | 404/404 | Pass |
| 27 | UI create idempotency replay creates one issue | 303/303 same, +1 issue | 303/303, same_loc=True, issues 1->2 | Pass |
| 28 | UI edit missing issue -> 404 | 404 | 404 | Pass |
| 29 | API setup 29 comments with seq 1..29 | 201 each, seq 1..29 | seqs=1..29 | Pass |
| 30 | UI 30th comment redirect -> page 1 anchor | 303 issue URL + #comment-30 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/3#comment-30 | Pass |
| 31 | 30th comment target page shows it, 30 items | 200, seq 1..30, body visible | 200, ids=[1, 2, 3]..30, visible=True | Pass |
| 32 | Page 1 at exactly 30: all fit, no pager | 200, no More/First | more=None, first=None | Pass |
| 33 | after=30 at exactly 30 comments has no seq 30 | 200, no comments, First link | 200, ids=[], first=True | Pass |
| 34 | UI 31st comment redirect -> page 2 anchor | 303 ...?after=30#comment-31 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/3?after=30#comment-31 | Pass |
| 35 | 31st target shows comment, links agree | 200, [31], First, no More | 200, ids=[31], first=/organizations/lab05/repositories/r3-i159-121403/issues/3, more=None | Pass |
| 36 | API page 1 default 30 items, next=30 | 200, 30, next 30 | 200, n=30, next=30 | Pass |
| 37 | API after=30 returns seq 31, next null | 200, [31], next null | 200, seqs=[31], next=None | Pass |
| 38 | API setup comments 32..60 | 201 each, seq matches | seqs 32..60 | Pass |
| 39 | UI 61st comment redirect -> page 3 anchor | 303 ...?after=60#comment-61 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/3?after=60#comment-61 | Pass |
| 40 | 61st target shows it once, links agree | 200, [61], First, no More | 200, ids=[61], anchor_count=1, first=/organizations/lab05/repositories/r3-i159-121403/issues/3, more=None | Pass |
| 41 | Page 2 window 31..60, both links | 200, 31..60, More=60, First=bare | 200, ids=30 31..60, more=/organizations/lab05/repositories/r3-i159-121403/issues/3?after=60, first=/organizations/lab05/repositories/r3-i159-121403/issues/3 | Pass |
| 42 | Page 1 window 1..30, More only | 200, 1..30 | 200, ids=30 1..30 | Pass |
| 43 | All 61 comments persisted in seq order | seq 1..61 | count=61, last=61 | Pass |
| 44 | API after=60 returns 61, next null | 200, [61] | 200, n=1, next=None | Pass |
| 45 | API after=30 limit=100 returns all 31 left | 200, 31..61, next null | 200, n=31, next=None | Pass |
| 46 | API invalid after=0/-1 -> 400 | 400/400 | 400/400 | Pass |
| 47 | API invalid limit 0/101 -> 400 | 400/400 | 400/400 | Pass |
| 48 | UI after=0 -> 400 invalid page | 400 | 400 | Pass |
| 49 | UI after=abc -> 400 | 400 | 400 | Pass |
| 50 | UI after past end: empty window with First link | 200, empty, First | 200, ids=0 | Pass |
| 51 | UI comment missing CSRF -> 403, no write | 403, unchanged | 403, comments 61->61 | Pass |
| 52 | UI comment wrong CSRF -> 403 | 403 | 403 | Pass |
| 53 | UI comment without session -> 303 login | 303 login | 303 /login | Pass |
| 54 | UI empty comment rejected, no write | 400, unchanged | 400, comments 61->61 | Pass |
| 55 | UI whitespace-only comment rejected | 400, unchanged | 400, comments 61->61 | Pass |
| 56 | UI 65537-char comment rejected | 400, unchanged | 400, comments 61->61 | Pass |
| 57 | UI 65536-char comment accepted at limit | 303 #comment-62 | 303 /organizations/lab05/repositories/r3-i159-121403/issues/3?after=60#comment-62 | Pass |
| 58 | UI comment XSS escaped in redirect target | 303, escaped, no raw | 303, target 200, escaped=True, raw=False | Pass |
| 59 | UI comment idempotency replay one comment | 303/303 same, +1 | 303/303, same=True, comments 63->64 | Pass |
| 60 | UI invalid idempotency key -> 400 | 400 | 400 | Pass |
| 61 | UI comment on missing issue -> 404 | 404 | 404 | Pass |
| 62 | Comments still allowed on closed issue | 303, +1 | 303, comments 64->65 | Pass |
| 63 | API create issue -> 201 echoes fields | 201 open | 201, number=4, state=open | Pass |
| 64 | API read issue -> 200 | 200 | 200 | Pass |
| 65 | API patch title -> 200 | 200 renamed | 200 title=API renamed | Pass |
| 66 | API close -> closed_at set | 200 closed | 200 state=closed | Pass |
| 67 | API reopen -> closed_at null | 200 open | 200 state=open closed_at=None | Pass |
| 68 | API invalid issue titles -> 400 | 400 x3 | 400/400/400 | Pass |
| 69 | API missing title / unknown field -> 400 | 400/400 | 400/400 | Pass |
| 70 | API invalid state / unknown patch field -> 400 | 400/400 | 400/400 | Pass |
| 71 | API missing issue -> 404 | 404 | 404 | Pass |
| 72 | API create comment -> 201 seq 1 | 201 seq 1 | 201 seq=1 | Pass |
| 73 | API invalid comment bodies -> 400 | 400 x4 | [400, 400, 400, 400] | Pass |
| 74 | API comment on missing issue -> 404 | 404 | 404 | Pass |
| 75 | API comment without auth -> 401 | 401 | 401 | Pass |
| 76 | API create issue without auth -> 401 | 401 | 401 | Pass |
| 77 | API idempotency replay same id, reuse conflict 422 | 201/201 same/422 | 201/201/422, same_id=True | Pass |
| 78 | API 65536-char comment accepted | 201 | 201, len=65536 | Pass |
| 79 | API limit=1 pagination cursor | 200, 1 item, next 1 | 200 n=1 next=1 | Pass |
| 80 | UI cross-org private beta/app -> 404 | 404 | 404 | Pass |
| 81 | API cross-org beta/app -> 404, own 200 | 200/404 | lab05=200 beta=404 | Pass |
| 82 | API create issue in beta/app -> 404 | 404 | 404 | Pass |
| 83 | API comment in beta/app -> 404 | 404 | 404 | Pass |
| 84 | UI anonymous private issue -> 303 login | 303 login | 303 /login | Pass |
| 85 | UI missing repo -> 404 | 404 | 404 | Pass |
| 86 | member erin can view issue page | 200 | 200 | Pass |
| 87 | repo:read token can read issue | 200 | 200 | Pass |
| 88 | repo:read token cannot comment -> 403 | 403 | 403 | Pass |
| 89 | repo:read token cannot create/patch issue -> 403 | 403/403 | 403/403 | Pass |
| 90 | triage role frank can comment | 201 | 201 | Pass |
| 91 | triage issue close documented behavior | 200 or 403 | 200 | Pass |
| 92 | UI invalid issue list state -> 400 | 400 | 400 | Pass |
| 93 | API closed issue filter consistent | 200, all closed | 200, n=1 | Pass |
| 94 | API invalid issue list state/limit -> 400 | 400/400 | 400/400 | Pass |
| 95 | API issue list newest first | descending numbers | 200, [3, 2, 1] | Pass |
| 96 | API issue list after=0 returns empty 200 (observed) | 200 empty | 200, n=0 | Pass |

## Defects

None found. Issue #159 is fixed and no regression was observed in the issue/comment domain.

## Notes

### Issue #159 boundary evidence

All UI comment submissions were made through the browser form with the `githearth_session` cookie and the page CSRF token. Redirects are relative `Location` values, matching `comment_href` in `crates/githearth-server/src/issues_ui.rs:644-653`.

| Boundary | POST result | Target page | Anchors / pagination |
| --- | --- | --- | --- |
| 30th comment (exactly one full page) | `303` `.../issues/3#comment-30` | `200`; ids 1..30; body `boundary thirty` visible | No `More`/`First` links; all 30 comments fit |
| 31st comment | `303` `.../issues/3?after=30#comment-31` | `200`; ids `[31]`; body visible | `First comments` -> bare issue URL; no `More` |
| 61st comment (after 32..60) | `303` `.../issues/3?after=60#comment-61` | `200`; ids `[61]`; `id="comment-61"` occurs once; body visible | `First comments` -> bare issue URL; no `More` |
| Page 2 (`?after=30`, 61 comments) | `200` | ids 31..60 | `First comments` -> bare; `More comments` -> `?after=60` |
| Page 1 (61 comments) | `200` | ids 1..30 | `More comments` -> `?after=30`; no `First` |

A second issue (`#5`) repeated the sequence from zero: the 30th, 31st and 61st comments returned the same three redirect forms and the same link sets. PostgreSQL confirmed contiguous seqs: issue #3 had 65 comments with seq 1..65, issue #5 had 61 with seq 1..61; no gaps or duplicates.

Root of the fix: `comment_submit` redirects with the stored `seq` (`issues_ui.rs:638-639`), and `comment_href` computes the page and cursor from `ISSUE_COMMENT_PAGE = 30` (`issues_ui.rs:21`, `issues_ui.rs:644-653`).

### Regression coverage

- Browser lifecycle: create, edit, close, reopen, list filters; CSRF missing/wrong rejects with 403; missing session redirects to `/login` (`issues_ui.rs:72-82`).
- Validation: blank/zero-width/257-char titles and blank/whitespace/65537-char comments are rejected without writes (`issues.rs:202-214`, `issues_ui.rs:169-175`).
- Escaping: issue and comment HTML/script content rendered escaped by `ui::esc` (`ui.rs:54-61`); an XSS comment stayed escaped on its redirect target.
- Pagination: 30/31/61 boundaries, `?after=60`, `limit=100`, invalid cursors, invalid limits. API and UI windows agree (`issues.rs:697-735`, `issues_ui.rs:503-513`).
- Isolation: `beta/app` returns 404 through the UI and API for read, issue create and comment; anonymous access to the private lab repo redirects to login.
- Idempotency: UI replay with the same key returns the same redirect and adds one comment; API replay returns the same comment id; body change under the same key returns 422 (`idempotency.rs:182-194`).
- Authorization: a `repo:read` token reads issues but gets 403 on comment, issue create and patch.

### Test-side corrections (not product failures)

Three earlier harness iterations failed before the clean run; all were tester errors, not product defects:

1. `#1` was matched inside CSS colours (`#111`, `#666`); corrected to match the table cell `>#1</td>`.
2. Redirect `Location` and page links are relative; expectations initially used absolute URLs.
3. Idempotency keys are scoped per account, not per path (`idempotency.rs:182-190`), so fixed literal keys reused by a later run on a different repository returned 422. The final harness suffixes every key with a per-run token; within-run replay tests still reuse the key deliberately.

### Observations

- A direct request to an out-of-range comment page (`?after=999`) returns `200` with `No comments yet.` plus a `First comments` link. The wording is misleading, but the UI never generates that link; cosmetic only.
- The issue-list API accepts `after=0` and returns an empty `200`, while the comment-list API rejects `after<1` with `400` (`issues.rs:701-706`). This is an input-validation inconsistency outside the scope of #159.
- Environment fixture differs from the brief: PostgreSQL `memberships` for `lab05` records every non-owner account (`bob`, `carol`, `dave`, `erin`, `frank`, `grace`) as `member`, not read/triage/write/etc. Non-owner members could comment and close issues. Authorization checks that need a read-only principal therefore used a `repo:read` access token instead of the documented role ladder.

No repository files were modified and nothing was posted to GitHub.
