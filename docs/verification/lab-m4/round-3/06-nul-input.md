# NUL input rejection (#152) — M4 closure round 3

Tester: t06 (owner, `lab06`). Date: 2026-11-14. Instance: main `800cad4`.
Verdict: PASS WITH DEFECTS
Cases: 161 run, 159 pass, 2 fail, 0 skipped.

The #152 fix holds for every API field named in the issue: a decoded U+0000 is
rejected with 400 (or 401 where credentials are checked first) and no
NUL-bearing value reaches a query. Two browser-path cases remain inconsistent:
`/organizations/{slug}/repositories/{name}` with a NUL in `{name}` renders
the 503 "Unavailable" page, and `/organizations/{slug}` with a NUL in `{slug}`
renders 404 rather than the 400 the fix commit claims. Neither executes a query
with the NUL; both are minor defects.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | issue create title NUL | 400 | 400 | PASS |
| 2 | issue create body NUL | 400 | 400 | PASS |
| 3 | issue create title NUL alone | 400 | 400 | PASS |
| 4 | issue patch title NUL | 400 | 400 | PASS |
| 5 | issue patch body NUL | 400 | 400 | PASS |
| 6 | issue comment body NUL | 400 | 400 | PASS |
| 7 | issue comment NUL only | 400 | 400 | PASS |
| 8 | pull create title NUL | 400 | 400 | PASS |
| 9 | pull create body NUL | 400 | 400 | PASS |
| 10 | pull create source_repository NUL | 400 | 400 | PASS |
| 11 | pull patch title NUL | 400 | 400 | PASS |
| 12 | pull patch body NUL | 400 | 400 | PASS |
| 13 | pull comment body NUL | 400 | 400 | PASS |
| 14 | pull comment path NUL | 400 | 400 | PASS |
| 15 | pull review body NUL | 400 | 400 | PASS |
| 16 | repo create name NUL | 400 | 400 | PASS |
| 17 | repo create description NUL | 400 | 400 | PASS |
| 18 | repo patch description NUL | 400 | 400 | PASS |
| 19 | org create name NUL | 400 | 400 | PASS |
| 20 | org create slug NUL | 400 | 400 | PASS |
| 21 | fork organization NUL | 400 | 400 | PASS |
| 22 | fork description NUL | 400 | 400 | PASS |
| 23 | fork name NUL | 400 | 400 | PASS |
| 24 | status context NUL | 400 | 400 | PASS |
| 25 | status context NUL only | 400 | 400 | PASS |
| 26 | status description NUL | 400 | 400 | PASS |
| 27 | status target_url NUL | 400 | 400 | PASS |
| 28 | status state enum with NUL | 400 | 400 | PASS |
| 29 | token name NUL | 400 | 400 | PASS |
| 30 | ssh key name NUL | 400 | 400 | PASS |
| 31 | protection pattern NUL | 400 | 400 | PASS |
| 32 | protection required check NUL | 400 | 400 | PASS |
| 33 | protection patch check NUL | 400 | 400 | PASS |
| 34 | tag protection pattern NUL | 400 | 400 | PASS |
| 35 | review request reviewer NUL body | 400 | 400 | PASS |
| 36 | review request reviewer NUL path | 400 | 400 | PASS |
| 37 | audit org path NUL | 400 | 400 | PASS |
| 38 | audit verify org path NUL | 400 | 400 | PASS |
| 39 | ui org path NUL | 400 | 404 | FAIL |
| 40 | ui repo path NUL | 400 or 404 | 503 | FAIL |
| 41 | repo read repo path NUL | 400 | 400 | PASS |
| 42 | repo read org path NUL | 400 | 400 | PASS |
| 43 | repo list org path NUL | 400 | 400 | PASS |
| 44 | repo archive repo path NUL | 400 | 400 | PASS |
| 45 | repo access repo path NUL | 400 | 400 | PASS |
| 46 | issue list repo path NUL | 400 | 400 | PASS |
| 47 | pull list org path NUL | 400 | 400 | PASS |
| 48 | forks list repo path NUL | 400 | 400 | PASS |
| 49 | statuses list repo path NUL | 400 | 400 | PASS |
| 50 | merge repo path NUL | 400 | 400 | PASS |
| 51 | protection list repo path NUL | 400 | 400 | PASS |
| 52 | tag protection list repo path NUL | 400 | 400 | PASS |
| 53 | browse tree rev NUL | 400 | 400 | PASS |
| 54 | browse tree path NUL | 400 | 400 | PASS |
| 55 | browse blob path NUL | 400 | 400 | PASS |
| 56 | browse commit id NUL | 400 | 400 | PASS |
| 57 | browse compare spec NUL | 400 | 400 | PASS |
| 58 | browse refs (control) | 200 | 200 | PASS |
| 59 | merge method NUL | 400 | 400 | PASS |
| 60 | merge expected_head_oid NUL | 400 | 400 | PASS |
| 61 | auto-merge method NUL | 400 | 400 | PASS |
| 62 | login username NUL | 401 | 401 | PASS |
| 63 | login password NUL | 401 | 401 | PASS |
| 64 | git basic-auth username NUL | 401 | 401 | PASS |
| 65 | git basic-auth password NUL | 401 | 401 | PASS |
| 66 | git org path %00 | 400 | 400 | PASS |
| 67 | git repo path %00 | 400 | 400 | PASS |
| 68 | git valid control | 200 | 200 | PASS |
| 69 | double-encoded org %2500 (literal %00) | 404, 400 | 404 | PASS |
| 70 | double-encoded ui org %2500 | 404, 400 | 404 | PASS |
| 71 | overlong UTF-8 NUL %C0%80 | 400, 404 | 400 | PASS |
| 72 | double-encoded browse path %2500 | 404, 400, 422 | 404 | PASS |
| 73 | raw NUL byte in JSON body | 400 | 400 | PASS |
| 74 | lone surrogate escape \ud800 | 400 | 400 | PASS |
| 75 | literal backslash-u0000 text accepted | 201 | 201 | PASS |
| 76 | issue emoji title+body | 201 | 201 | PASS |
| 77 | issue zero-width space inside title | 201 | 201 | PASS |
| 78 | issue 1200-char body | 201 | 201 | PASS |
| 79 | issue patch emoji title | 200 | 200 | PASS |
| 80 | repo patch emoji description | 200 | 200 | PASS |
| 81 | issue comment emoji+ZWSP | 201 | 201 | PASS |
| 82 | status unicode context | 201 | 201 | PASS |
| 83 | pull comment emoji | 201 | 201 | PASS |
| 84 | pull review commented emoji | 201, 422 | 201 | PASS |
| 85 | repo create emoji description | 201 or 409 | 409 | PASS |
| 86 | ui org valid | 200 | 200 | PASS |
| 87 | ui repo valid | 200 | 200 | PASS |
| 88 | health ready | 200 | 200 | PASS |
| 89 | health live | 200 | 200 | PASS |
| 90 | issue list after NUL | 400, 422 | 400 | PASS |
| 91 | audit query after NUL | 400, 422 | 400 | PASS |
| 92 | notifications after NUL | 400, 422 | 400 | PASS |
| 93 | cleanup protection rule | 204, 200 | 204 | PASS |
| 94 | cleanup tag rule | 204, 200 | 204 | PASS |
| 101 | JSON key name NUL | 400 | 400 | PASS |
| 102 | ssh public_key NUL | 400 | 400 | PASS |
| 103 | token scope NUL | 400 | 400 | PASS |
| 104 | pull draft non-bool string | 400 | 400 | PASS |
| 105 | pull comment in_reply_to NUL | 400 | 400 | PASS |
| 106 | review verdict NUL | 400 | 400 | PASS |
| 107 | tag allowed_to_create NUL | 400 | 400 | PASS |
| 108 | issue patch state NUL | 400 | 400 | PASS |
| 109 | pull patch state NUL | 400 | 400 | PASS |
| 110 | repo patch visibility NUL | 400 | 400 | PASS |
| 111 | repo patch name NUL | 400 | 400 | PASS |
| 112 | issue create raw NUL (not escaped) | 400 | 400 | PASS |
| 113 | wrong type with NUL number | 400 | 400 | PASS |
| 114 | pull number path NUL | 400 | 400 | PASS |
| 115 | issue number path NUL | 400 | 400 | PASS |
| 116 | commit status sha path NUL | 400 | 400 | PASS |
| 117 | org read id path NUL | 400 | 400 | PASS |
| 118 | notification id path NUL | 400 | 400 | PASS |
| 119 | token id path NUL | 400 | 400 | PASS |
| 120 | ssh key id path NUL | 400 | 400 | PASS |
| 121 | access grant id path NUL | 400 | 400 | PASS |
| 122 | protection id path NUL | 400 | 400 | PASS |
| 123 | tag protection id path NUL | 400 | 400 | PASS |
| 124 | issue list state filter NUL | 400 | 400 | PASS |
| 125 | pull list state filter NUL | 400 | 400 | PASS |
| 126 | audit limit NUL | 400 | 400 | PASS |
| 127 | issue list after=NUL text | 400 | 400 | PASS |
| 128 | notifications after NUL | 400 | 400 | PASS |
| 129 | git anon public org %00 | 400 | 400 | PASS |
| 130 | git anon public repo %00 | 400 | 400 | PASS |
| 131 | git basic token-with-NUL | 401 | 401 | PASS |
| 132 | ui repo path NUL anonymous public | 400, 404, 303 | 303 | PASS |
| 133 | ui org path NUL anonymous | 400, 404, 303 | 303 | PASS |
| 134 | ui login form username NUL | 303 (bad credentials) | 303 | PASS |
| 135 | ui login form password NUL | 303 (bad credentials) | 303 | PASS |
| 136 | browse refs org path NUL | 400 | 400 | PASS |
| 137 | compare spec raw NUL via %00 | 400 | 400 | PASS |
| 138 | tree rev encoded slash + NUL | 400 | 400 | PASS |
| 139 | fork path repo NUL | 400 | 400 | PASS |
| 140 | status path repo NUL, body valid | 400 | 400 | PASS |
| 141 | status sha NUL, body valid | 400 | 400 | PASS |
| 142 | merge number path NUL | 400 | 400 | PASS |
| 143 | reviewer path NUL encoded slash | 400 | 400 | PASS |
| 144 | double-encoded reviewer %2500 | 400, 404 | 404 | PASS |
| 201 | ui issues list | 200/400/404/422 | 200 | PASS |
| 202 | ui issue page | 200/400/404/422 | 200 | PASS |
| 203 | ui issue number NUL | 200/400/404/422 | 404 | PASS |
| 204 | ui pulls list | 200/400/404/422 | 200 | PASS |
| 205 | ui pull page | 200/400/404/422 | 200 | PASS |
| 206 | ui pull number NUL | 200/400/404/422 | 404 | PASS |
| 207 | ui tree | 200/400/404/422 | 200 | PASS |
| 208 | ui tree rev NUL | 200/400/404/422 | 404 | PASS |
| 209 | ui tree path NUL | 200/400/404/422 | 404 | PASS |
| 210 | ui blob path NUL | 200/400/404/422 | 404 | PASS |
| 211 | ui commits rev NUL | 200/400/404/422 | 404 | PASS |
| 212 | ui commit id NUL | 200/400/404/422 | 404 | PASS |
| 213 | ui compare spec NUL | 200/400/404/422 | 404 | PASS |
| 214 | ui settings | 200/400/404/422 | 200 | PASS |
| 215 | ui unknown org path NUL | 200/400/404/422 | 404 | PASS |
| 216 | ui org repositories path NUL | 200/400/404/422 | 404 | PASS |
| 217 | ui issue form title NUL | 200/400/404/422 | 400 | PASS |
| 218 | ui issue form body NUL | 200/400/404/422 | 400 | PASS |
| 219 | ui pull form title NUL | 200/400/404/422 | 400 | PASS |
| 220 | ui pull comment NUL | 200/400/404/422 | 400 | PASS |
| 221 | ui issue comment NUL | 200/400/404/422 | 400 | PASS |
| 222 | ui token name NUL | 200/400/404/422 | 400 | PASS |
| 223 | ui ssh name NUL | 200/400/404/422 | 400 | PASS |

## Defects

### Browser repository path with NUL returns 503 "Unavailable", not 400/404
- Severity: minor
- Repro: authenticated browser request
  `GET /organizations/lab06/repositories/t06%00browse` with
  `Cookie: githearth_session=<session token>`.
- Expected / Actual: 400 (invalid input) or 404 (not found); actual
  `503 Service Unavailable` with the generic "Unavailable" page.
- Evidence: response is `503 Service Unavailable`, `content-type: text/html`,
  body title `Unavailable`, `content-length: 1615`; request log shows
  `route=/organizations/{slug}/repositories/{name} status=503`, 0 ms.
  Root cause: `ui.rs:662-670` maps any `load_repo` error that is not 404 to
  `unavailable_page`, while `load_repo` returns 400 for a NUL path parameter
  (`repository_api.rs:96`) before acquiring a connection (`:97`). No query runs,
  but the response is indistinguishable from a database outage and reintroduces
  the 503 symptom #152 is meant to remove, on a user-controlled field.
- Same class as the fixed "browser organization path" case; the browser
  repository handler was not updated in `da7c068`.

### Browser organization path with NUL returns 404, not the claimed 400
- Severity: minor (contract/documentation mismatch; behaviour is safe)
- Repro: authenticated browser request `GET /organizations/lab06%00` with the
  session cookie.
- Expected / Actual: 400 per issue #152 and the `da7c068` commit message
  ("NUL bytes are rejected with 400 for ... the browser organization path");
  actual `404` "Not found".
- Evidence: `ui.rs:543-545` returns `not_found_page` for an invalid slug. The
  guard does run before the organization query (`ui.rs:547`), so no NUL reaches
  PostgreSQL. 404 is defensible browser behaviour, but it does not match the
  stated contract or the commit's claim.

## Notes

- No bypass found. `%2500` is decoded once to the literal three-character string
  `%00`, which correctly misses and returns 404 (`/api/v1/...` and UI); it is
  not re-decoded into NUL. Overlong UTF-8 `%C0%80` is rejected by the path
  extractor with 400. Raw `0x00` bytes in JSON bodies and lone surrogates
  (`\ud800`) are rejected by `serde_json` with 400.
- JSON `\u0000` is rejected wherever text is validated: issue/comment, pull,
  pull comment/review, repository name/description, organization name/slug,
  fork organization/description/name, status context/description/target_url,
  token name/scope, SSH key name/public_key, branch/tag protection
  pattern/checks, review-request reviewer (body and path), merge/auto-merge
  enums, and typed enum fields (state/visibility/draft/verdict).
- Basic-auth and login NULs are rejected before credential lookup binds text:
  `verify_password` enforces `valid_username` before the query
  (`lib.rs:589-606`), so the API returns 401 and the browser login redirects to
  `/login?error=1` (303). A `gth_` token containing NUL is hashed and looked up
  as bytes, returning 401 with no text bind.
- The Git smart-HTTP path guard runs first in `admit`, `authorize_fetch`,
  `admit_anonymous` and `admit_account` (`git_admission.rs:103-110`), giving 400
  for `%00` in either segment on fetch and receive-pack paths, authenticated or
  anonymous.
- Statuses deliberately authenticate before body validation (`statuses.rs:153`,
  changed in `76aee63`): unauthenticated NUL/valid bodies both return 401, and
  only authenticated NUL bodies return 400. This avoids a pre-auth input oracle
  and is not a regression.
- Guard placement: most handlers validate the NUL-bearing value before the
  first query that uses it (`lib.rs:458-466`). Some handlers authenticate first
  (a credential lookup unrelated to the field) or run idempotency lookup before
  validating, so "before any database call" is not literal; the NUL value
  itself is never bound. Confirmed by source and by the absence of any
  ERROR/database log entry and of 5xx except the UI case above.
- Valid UTF-8 regression: emoji and zero-width characters in issue titles,
  bodies and comments (201), a 1200-character body (201), emoji repository
  descriptions (200/201), a Unicode status context `café✓` (201), pull comments
  and reviews (201), a literal backslash-u sequence (`\u0000` as text) accepted
  with 201 (no over-rejection), and
  `/health/ready` returning 200 after every batch. Valid UI organization and
  repository pages return 200; `GET .../refs` returns 200; a valid Git
  `info/refs` fetch returns 200.
- UI form submissions (issue/pull/comment/token/SSH-key) with NUL return 400
  error pages; UI browse rev/path NULs return 404; UI typed path segments
  (`{number}`, `{id}`) return 404/400. No UI handler returned 500.
- Query parameters are typed or enum-checked: `after`, `limit`, `unread`, and
  `state` with `%00` return 400 (`state=open%00` on the UI lists renders a 400
  page).
- Test mistakes corrected during the run (not product defects): the first pass
  sent the session cookie in a wrongly named header (303 instead of 200),
  overrode the Git Basic `Authorization` header with the bearer token (401),
  and omitted the CSRF token on UI form posts (403). Case 131 was initially
  expected to be 400 but 401 is correct.
- Fixtures: `lab06/t06-browse` PR #1 and issue #2, plus repository
  `r3-nul-ok`; branch/tag protection rules used for the PATCH probes were
  deleted. No repository files or GitHub issues were touched. The primary
  compose project was not restarted.
