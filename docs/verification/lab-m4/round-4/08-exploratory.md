# Round 4 — exploratory / randomized sweep (whole product)

- Date: 2026-11-14
- Instance: `main` `cd7946b`, Compose quick start, HTTP 127.0.0.1:8080
- Account: `t08` (owner of `lab08`), password `<test-password>`; also used
  `bob`/`carol` ids for grant/reviewer targets and the fresh-session path.
- Method: randomized and boundary probing across every JSON API family and every
  browser route, plus form↔API interaction, concurrency, idempotency and a
  60-page link crawl. Read-only SQL for ground truth. No repo edits.

## Verdict

**PASS — no blockers.** The M4 closure-polish changes hold: the new browser
creation forms round-trip into the API, escaping is correct everywhere markup was
injected, merge/auto-merge read authorization runs before state, blocked and
failed merges are explained, NUL in browser paths renders the styled 404 (API
paths reject it with 400 JSON), and `new` is reserved in both slugs and repository
names. Four low-severity contract/consistency defects and one informational route
rough edge were found; none is a security or data-integrity problem. No 5xx was
produced by this sweep.

## Counts

| Area | Cases | Result |
| --- | ---: | --- |
| Auth, sessions, health, sudo window | 30 | Pass except D1 (login 422) |
| Organizations (create/read/list/reserved/unicode/limits) | 32 | Pass except D1 |
| Repositories (create/limits/NUL/archive/delete/access grants) | 46 | Pass |
| Issues, comments, state, edit (API + forms) | 34 | Pass |
| Pulls, reviews, reviewer requests, merge, auto-merge | 62 | Pass (async merge and policy 403 by design) |
| Statuses, branch protection, tag protection | 32 | Pass except D4 |
| Forks (name collision, foreign org, archived, parent link) | 12 | Pass |
| Notifications (filters, read, read-all) | 12 | Pass |
| Tokens and SSH keys | 24 | Pass except D1 (422 bodies) |
| Browse, compare, blob/tree/commit | 14 | Pass |
| Audit list/verify | 6 | Pass with fresh session |
| UI pages, methods, CSRF, escaping, link crawl | 44 | Pass except D2 |
| Randomized fuzz (API + browser paths) | 30 | Pass — no 5xx |
| Concurrency, idempotency, pagination | 26 | Pass |
| **Total** | **404** | 392 scripted + 12 targeted re-runs |

Every first-pass failure was re-run once. Most were harness-expectation errors
(wrong body shape, wrong query parameter, `200` instead of `303` because form
POSTs render the result page, UUID-vs-slug path). The residual items are D1–D5
below. No 5xx is attributable to this account; the single 5xx in the window came
from a concurrent lab05 session (see Notes).

## Defects

### D1 — `/api` extractor rejections other than 400 are `text/plain` and echo serde internals (Low)

Only a 400 is normalised to the JSON envelope. The `Json` extractor's other
rejections pass through unchanged, so an API client gets a plain body that names
the internal struct fields. The extractor runs before authentication, so the
same bodies are returned to **unauthenticated** callers (no token needed).

Repro:

```bash
TOK=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"t08","password":"<test-password>"}' | sed 's/.*"token":"\([^"]*\)".*/\1/')

# 422 text/plain — no Authorization header required
curl -si -X POST http://127.0.0.1:8080/api/v1/organizations \
  -H 'Content-Type: application/json' \
  -d '{"slug":"x4-demo","name":"x","role":"owner"}'
# HTTP/1.1 422 Unprocessable Entity
# content-type: text/plain; charset=utf-8
# Failed to deserialize the JSON body into the target type: role: unknown field
# `role`, expected `slug` or `name` at line 1 column 39

# 415 text/plain
curl -si -X POST http://127.0.0.1:8080/api/v1/organizations \
  -H "Authorization: Bearer $TOK" -H 'Content-Type: text/plain' -d '{"slug":"x","name":"y"}'
# Expected request with `Content-Type: application/json`

# 413 text/plain
# same route with a >512 KiB body -> "length limit exceeded"
```

Also reproduced on `POST /api/v1/login` (422, wrong field type),
`POST /api/v1/user/tokens` and `POST /api/v1/user/ssh-keys` (422, unknown field).

Root cause: `crates/githearth-server/src/lib.rs:396-425` — `json_rejections`
rewrites only `response.status() == StatusCode::BAD_REQUEST` with a `text/plain`
content type. Axum returns 422 for data errors, 415 for a missing/incorrect
content type and 413 from `RequestBodyLimitLayer`, none of which are covered.

Impact: an API contract break for clients that always parse the `{"error": ...}`
envelope, plus minor disclosure of internal field names to unauthenticated
callers. No authorization impact.

### D2 — Browser form/query rejections bypass the styled page (Low)

The UI router has no equivalent of `json_rejections`, so a malformed `Form` or
`Query` body returns a bare plain-text response that names internal fields,
instead of the styled error page used for handler-level 400s.

Repro (signed-in browser session + CSRF from `/`):

```bash
# 422 text/plain, not the styled "Invalid request" page
curl -si -X POST http://127.0.0.1:8080/organizations/new \
  -H 'Cookie: githearth_session=...; githearth_csrf=...' \
  --data-urlencode 'csrf_token=...' --data-urlencode 'slug=x4-noname'
# Failed to deserialize form body: missing field `name`

# 400 text/plain on a browser list page
curl -si 'http://127.0.0.1:8080/organizations/lab08/repositories/t08-pulls/issues?after=abc' \
  -H 'Cookie: githearth_session=...'
# Failed to deserialize query string: after: invalid digit found in string

# 413 text/plain
# POST /organizations/new with a 600 KB `name`
# length limit exceeded
```

Contrast: `?state=bogus` on the same page is handled in the handler and returns a
styled `text/html` 400. The inconsistency is visible on one page.

Root cause: `crates/githearth-server/src/ui.rs:1330-1357` (UI router + fallback)
has no extractor-rejection middleware; `issues_ui.rs:198-203` and
`pulls_ui.rs:295-300` take `Query<...>` directly.

Impact: unstyled errors, minor internal-name disclosure; no auth or CSRF bypass
(the extractor fails before the handler, so no mutation happens).

### D3 — `/api` 405 responses are empty with no content type (Low)

```bash
curl -si -X DELETE http://127.0.0.1:8080/api/v1/me -H "Authorization: Bearer $TOK"
# HTTP/1.1 405 Method Not Allowed
# content-length: 0
# (no content-type, no body)

curl -si -X PUT http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls \
  -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{}'
# 405, empty
```

Every other `/api` error uses `{"error": "..."}`. Root cause: axum's
`MethodRouter` default 405; neither `json_rejections` (lib.rs:396) nor the UI
fallback (ui.rs:246) normalises a method mismatch.

Impact: clients get an empty body where the API contract implies JSON; low.

### D4 — Branch protection patterns have no length bound (Low)

`POST .../protection` accepts a 300-character pattern with `201`; repository
names (100), org slugs (64) and status contexts (255) are all bounded.

```bash
curl -s -X POST .../repositories/x4-prot/protection \
  -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
  -d "{\"pattern\":\"$(python3 -c 'print("a"*300)')\"}"
# 201 with pattern = "aaa…" (300 chars)
```

Root cause: `crates/githearth-server/src/protection.rs:343-351` —
`validate_rule` calls `parse_pattern` and checks approvals/checks but no length.
The column is `text`, so the only bound is the 512 KiB request body.

Impact: inconsistent limit, oversized rules can be stored and rendered; no
security impact.

### D5 — Route rough edges (Info, not a defect)

- `GET /organizations/{org}/repositories/{repo}/compare` (no query) returns a
  styled **404**. The route exists as the GET-form target
  (`browse_ui.rs:47-52`, `507-527`) and requires `?base=&head=`; a direct visit
  or bookmark gets "Not found".
- `GET /settings/tokens` and `GET /settings/ssh-keys` return **405 with an empty
  body**: those paths are POST-only form endpoints (`settings_ui.rs:21-32`); the
  pages live at `/settings`. No page links to them, so the 60-page crawl found no
  broken links.

## What held up

- **Browser forms ↔ API (the new build's focus):** org, repo, issue, comment,
  issue state, issue edit, PR, reviewer add/remove, branch rule create/delete,
  tag rule create, fork, token create and notification read-all all round-trip;
  the API sees exactly the created object (one row per submit, correct
  visibility, `parent_repository_id` on forks, reviewer list updated).
- **Form validation:** reserved `new` (org and repo) → 400 styled page; duplicate
  slug → 409; duplicate repo → 409 "repository name already taken"; foreign org
  → 404 "not found" (no existence leak); invalid visibility/empty name → 400;
  missing/incorrect CSRF → 403; idempotency replay of a repo/issue form → 200/303
  with no duplicate.
- **Escaping:** `<script>`, `<img onerror>`, `<svg onload>`, `<b onmouseover>` in
  issue titles/bodies, org display names, PR comments and token names are all
  escaped on every page I loaded; no raw payload was rendered.
- **Merge semantics:** read authorization precedes open/merged state; a blocked
  merge (code-owner rule) returns 403 and the pull page says "Merging is blocked:
  the protection requirements (approvals, checks or code owners) are not met."; a
  merge that cannot apply (already up to date) creates an intent that settles
  `state=failed, failure_reason="nothing to merge"` and the page shows it;
  successful merges settle to `merged` and the page shows "Merged by t08 at
  2026-11-14T10:01:12Z" (PR #7); two concurrent merge POSTs → one `201`, one
  `409 "an open merge intent already exists"`.
- **Fork parent:** a form-created fork returns `parent_repository_id` from the API
  and the repo page renders "Forked from lab08/t08-xrepo" as a link to the parent.
- **Concurrency/idempotency:** 8 parallel pull-page GETs all 200; 4 parallel
  same-name repo creates → exactly one 201 and three 409; 3 parallel issue
  creates with one idempotency key → one issue, the others 409 conflict (no
  duplicates); sequential replay returns the stored response.
- **Limits:** issue title 257 chars → 400, org slug 65 → 400, repo name 101 → 400,
  token name 200 → 400, 8 KB SSH key → 400, 600 KB body → 413, 8 KB cursor → 400,
  4 KB path → 400; NUL in title/body/description/name → 400, NUL in an API path →
  400 JSON, NUL in a browser path → styled 404 (checked on the org, repo, issue,
  tree and pull paths).
- **Pagination:** issues and pulls follow `after` without duplicates; `after=abc`
  → 400; `limit=0`/`limit=100000` → 400; unknown query parameter → 400.
- **Headers:** `cache-control: no-store`, `x-content-type-options: nosniff`,
  `referrer-policy: same-origin` on API responses; CSP (`default-src 'none'`) and
  `x-frame-options: DENY` on HTML.
- **No leaks:** no SQL, DSN, backtrace, panic text or file paths in any body; DB
  failures surface as `503 {"error":"database unavailable"}`.

## Notes / observations

- **Sudo mode:** sensitive actions (`/audit`, access grant/revoke,
  archive/delete, org update) return `403 {"error":"not permitted"}` when the
  session's last password entry is older than 15 minutes
  (`DEFAULT_FRESH_AUTH`, `githearth-authz/src/lib.rs:36`). A fresh login returns
  200. Correct, but API clients holding a session token will hit it; worth a line
  in the API docs.
- **Asynchronous merge:** `POST .../merge` returns `201` with a merge intent, not
  a merged pull. Clients must poll the pull or the intent. Not a defect; the
  round-4 doc should say so.
- **Statuses are keyed by SHA, not by commit existence:** posting a status for a
  40-hex SHA not present in the repository returns `201` (the test suite posts
  statuses for synthetic SHAs, so this is intentional). No action.
- **Idempotency key reuse with a different body → 422**, not 409
  (`idempotency.rs:194,245`). Consistent with the API's 422 validation family
  and asserted by tests; noted only because the status differs from the
  concurrent-conflict 409.
- **One 5xx in the server log during the window, not mine:** at 09:43:42 UTC a
  concurrent session merged on `lab05/regr4ad` and got `500` on
  `POST .../pulls/{n}/merge` after a `repository read failed` (category `git`,
  111 ms). `t08` is not a member of `lab05`, so I could not reproduce it; no
  other 5xx in 45 minutes of logs. Flagging it for the round-4 owner in case that
  repository was being deleted while a merge ran.
- **Test data left in `lab08`:** objects tagged `x4…` (orgs, repos, issues, PRs,
  protection rules, tokens are revoked). No cleanup was attempted beyond what the
  API allows; org deletion has no API route.
- The 30-case randomized fuzz (random methods/paths/bodies, percent-encoded
  Unicode, `..`, 300-char segments, 8 KB queries) produced no 5xx and no
  unhandled paths: unknown API paths return the JSON 404, unknown browser paths
  the styled 404.
