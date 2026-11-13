# Server-rendered UI, accessibility, error pages and documentation accuracy — M4 lab acceptance report

Tester: `t13`. Date: 2026-11-14. Instance: <http://127.0.0.1:8080> (main `b820454`).

Verdict: **FAIL** — the PR detail page/API is unavailable for every pull request whose head is not already up to date, which blocks the review/merge flow.

Cases: **70 run, 54 pass, 13 fail, 3 skipped.**

Setup: created `lab13/t13-ui` through the JSON API and pushed `main` and `feature` over HTTP Git. Seeded issue #1, pull requests #2 (`feature`→`main`), #3 and #5 (up-to-date controls), XSS issue #4, 100 issues and 100 comments, a `v13-*` tag-protection rule, an access token and an Ed25519 SSH key. Tested browser pages with a real `POST /login` session; `t13` is `owner` of `lab13` and base-role `member` of `acme`. No `acme` setting was changed; `acme/web` was only read. All failing cases were re-run once.

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| 1 | `GET /login` anonymous | 200 `text/html` | 200 `text/html; charset=utf-8`, sign-in form | PASS |
| 2 | `GET /` anonymous and signed in | 303 to `/login`; 200 home | 303 `/login`; 200 “Your organizations” | PASS |
| 3 | `GET /organizations/lab13` and `/organizations/lab13/repositories` | 200 org page | 200, h1 “Lab 13”, both aliases | PASS |
| 4 | `GET /organizations/lab13/repositories/t13-ui` | 200 repo page | 200, h1 `t13-ui` | PASS |
| 5 | `GET .../t13-ui/issues` | 200 list, 30/page | 200, 30 rows + `rel=next` | PASS |
| 6 | `GET .../t13-ui/issues/1` | 200 detail | 200, title, body, comments form | PASS |
| 7 | `GET .../t13-ui/pulls` | 200 list | 200, 30/page | PASS |
| 8 | `GET .../t13-ui/pulls/2` (`feature`→`main`) | 200 detail | **503 “Unavailable”**; API read returns **500** `{"error":"storage operation failed"}` | **FAIL** |
| 9 | `GET .../t13-ui/pulls/3` and `/5` (up-to-date heads) | 200 detail | 200; control shows the failure is specific to changed heads | PASS |
| 10 | `GET /settings` (token + SSH-key sections) | 200, both sections | 200, both tables and forms | PASS |
| 11 | `GET .../t13-ui/settings/tags` | 200 | 200 | PASS |
| 12 | `GET /notifications` | 200 | 200, empty state | PASS |
| 13 | Missing repo, issue and PR | 404 styled HTML | 404 `text/html`, styled 404 page, correct CSS | PASS |
| 14 | Private repo as anonymous visitor | 403/404 or sign-in redirect | 303 `/login` for `lab13/t13-ui`; public `acme/site` 200 | PASS |
| 15 | Private repo as signed-in non-member | 403/404 | `admin` and `t13` on `beta/app`: 404; no content leak | PASS |
| 16 | `/health/live` and `/health/ready` | 200 | 200, empty body, `nosniff`/`Referrer-Policy` | PASS |
| 17 | Method not allowed | 405 with `Allow` | `POST /` 405 `Allow: GET,HEAD`; `DELETE /login` 405 `GET,HEAD,POST` | PASS |
| 18 | Unmatched URL (`/no-such-page`) | Styled 404 page | 404 with **0-byte body**, no `Content-Type`, no page CSP or `X-Frame-Options` | **FAIL** |
| 19 | `lang`, one `h1`, heading order, `aria-current` | One h1, no skipped levels, active filter marked | All checked pages pass; filter uses `aria-current="page"` | PASS |
| 20 | Landmarks and skip link | `nav`, `main`, skip link target | `<nav aria-label="Primary">`, `<main id="main" tabindex="-1">`, skip link first in body | PASS |
| 21 | Labels and autocomplete | Every control labelled; login autocomplete | All controls have `label for`/wrapper; `username` + `current-password` | PASS |
| 22 | Buttons have text | Non-empty accessible name | All buttons have visible text | PASS |
| 23 | Table header scopes | Every `th` has `scope` | All tables pass | PASS |
| 24 | Lists | Only `ul`/`ol` with `li` | No `li` outside a list | PASS |
| 25 | Images have `alt` | `alt` on every image | No `<img>` elements rendered | PASS |
| 26 | Duplicate `id` attributes | None | PR detail has **two `id="comment"`** textareas and two `label for="comment"` | **FAIL** |
| 27 | Unclosed/mismatched tags | Valid nesting | 15 pages parsed by `html.parser`: 0 mismatches/unclosed tags | PASS |
| 28 | Focus visibility and tabindex | `:focus-visible` styles; no positive tabindex | Focus-visible rule present; 0 positive tabindex | PASS |
| 29 | Validation errors | `role="alert"` | Empty issue and comment returns 400 with `role="alert"` | PASS |
| 30 | Login error association | `aria-invalid` + `aria-describedby` | Both fields marked; `login-error`/`csrf-error` targets exist | PASS |
| 31 | Dialog/menu patterns | Accessible if present | None rendered | SKIP |
| 32 | Colour contrast | AA (4.5:1 text, 3:1 large/non-text) | Body 17.64:1, nav 18.88:1, nav muted 11.76:1, muted 5.36–5.74:1, error 7.33:1, focus 5.59:1 / 3.16:1 | PASS |
| 33 | Touch targets, viewport | ≥24×24 or spacing exception; no `user-scalable=no` | Buttons 35px high; nav links 18px high but separated by 16px gap; viewport `width=device-width,initial-scale=1` | PASS |
| 34 | CSP | CSP with `frame-ancestors 'none'`, no unsafe scripts | `default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'` | PASS |
| 35 | `X-Content-Type-Options`, `Referrer-Policy`, `X-Frame-Options` | Present on pages | `nosniff`, `same-origin`, `DENY` on `/`, `/login`, `/settings`, 404 and 503 pages | PASS |
| 36 | No inline scripts | No script tags/handlers | 0 `<script>` elements; escaped `onerror=` text only; no `script-src` allowance | PASS |
| 37 | Cookies | `HttpOnly; SameSite=Lax` | Session and CSRF cookies both `HttpOnly; SameSite=Lax`; `Secure` omitted by local `GITHEARTH_SECURE_COOKIES=false` | PASS |
| 38 | Cache control | `Cache-Control: no-store` | Present on all browser pages | PASS |
| 39 | Login CSRF | Missing/invalid token 403 | Both 403 “Invalid CSRF token.” | PASS |
| 40 | Issue CSRF | Missing/invalid/cross-session token 403 | All three 403 | PASS |
| 41 | Valid issue CSRF | 303 to new issue | 303 `/issues/43` | PASS |
| 42 | Logout CSRF | Missing 403; valid 303 + clear cookie | 403; 303 `/login` with `Max-Age=0` | PASS |
| 43 | PR form CSRF | Missing token 403 on every mutation | PR create, comment, review, draft, auto-merge: all 403 | PASS |
| 44 | Other form CSRF | Missing token 403 | Fork, tag delete, SSH-key revoke, notification read/read-all: all 403 | PASS |
| 45 | Settings forms with valid token | Token and SSH key created | Token 200 with one-time plaintext; key 303 `/settings` | PASS |
| 46 | CSRF token scope | Per-session, not cross-session | Tokens differ by session, stable within one session, cross-session rejected | PASS |
| 47 | Idempotent resubmit | Same key = same result, one issue | Two 303s to issue #44; exactly one issue with that title | PASS |
| 48 | Issue XSS | Escaped title/body/comment | 0 script elements; payloads appear as `&lt;script&gt;` / `&lt;img…` | PASS |
| 49 | PR XSS | Escaped title/body/comment/review | 0 script elements; all payloads escaped | PASS |
| 50 | Branch-name XSS | Escaped in PR page/URL | Branch `t13-<script>alert(1)</script>` rendered as text, no script element | PASS |
| 51 | Repository-description XSS | Escaped | Description rendered as `&lt;script&gt;…` on repo page | PASS |
| 52 | No raw payload anywhere | No `<script>`/`<img>` injected | 0 script and 0 img elements on issue/list/PR/settings/tags pages | PASS |
| 53 | Reviewer-name XSS | N/A — usernames constrained | Usernames are lowercase ASCII; no HTML can be stored | SKIP |
| 54 | README quick start vs compose | Ports, health URL, bootstrap, volumes, `--wait` match | 8080/2222/54329, `/health/ready`, `bootstrap-admin --username admin`, both named volumes, healthchecks + `--wait` match | PASS |
| 55 | README and `docs/operations/*.md` links | Every relative link resolves | 14 files checked, 0 missing targets; `#licensing` anchor exists | PASS |
| 56 | `docs/foundation.md` API examples | Match live status codes/fields | 8 endpoints match: health live/ready 200, login 200 token/Bearer/3600, me UUID+username+is_admin, orgs list, org create 201 owner, duplicate 409, get org 200/404, logout 204 | PASS |
| 57 | Status claims: issues, forks, protection, SSH | Live | Issue create/edit/close/reopen 303; fork 201; branch rule 201/204; tag rule 303; SSH clone succeeded | PASS |
| 58 | Status claim: pull requests | Live | List/create/comment/review work, but changed-PR detail is 503/500 (case 8) | **FAIL** |
| 59 | `docs/foundation.md` introduction | Accurate | Line 3 says the binary has no Git HTTP/SSH transport, issue/review workflow or UI; all are live | **FAIL** |
| 60 | `docs/foundation.md` issue UI claim | Accurate | Line 166 says closing, reopening and editing issues are “API-only”; UI forms work (200 get, 303 post) | **FAIL** |
| 61 | `docs/operations/tls.md` header/gap table | Accurate | Lines 121–123 and 128–131 say nosniff, Referrer-Policy and CSP are missing/never implemented; all are live; line refs point at old `ui.rs` lines | **FAIL** |
| 62 | `docs/verification/accessibility.md` claims | Accurate | Line 15 claims breadcrumbs on drill-down pages and captions on tables; blob/commits/compare/tag-settings have no Breadcrumb nav and tree/compare tables have no caption | **FAIL** |
| 63 | `docs/operations/install.md` Git minimum | Declared minimum sufficient | Line 24 says Git ≥2.38; container 2.39.5 fails the rebase mergeability command (case 8) | **FAIL** |
| 64 | 404/503 quality | Styled, no stack/SQL, correct status | Known 404 and the 503 have CSS, h1, no SQL/backtrace; request-id not documented | PASS |
| 65 | Request-id | If documented | Not documented anywhere | SKIP |
| 66 | 100 issues and 100 comments pagination | 30/page, no duplicates/omissions | 4 pages each; 100 unique issue numbers and 100 unique comment bodies; `First page`/`Next page` links work | PASS |
| 67 | Long issue title layout | Wraps/truncates | 250 unbroken characters: page `scrollWidth=5817` vs `clientWidth=756`; h1 runs off screen | **FAIL** |
| 68 | Long issue body layout | Wraps/truncates | 5000 unbroken characters: `scrollWidth=51049` vs `clientWidth=756`; card text overflows viewport | **FAIL** |
| 69 | Invalid tag-protection form input | 400 with validation message | **503 “Unavailable”** for bad pattern and bad role; API returns 400 `{"error":"invalid input"}` | **FAIL** |
| 70 | Checkbox layout | Native control next to its label | Global CSS makes checkbox **866×13px**, label text separated; affects draft/scopes/tag forms | **FAIL** |

## Defects

### PR detail and read API return 500/503 for every changed pull request
- **Severity: blocker**
- **Repro:**
  ```bash
  # browser session cookie for t13
  curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt \
    http://127.0.0.1:8080/organizations/lab13/repositories/t13-ui/pulls/2
  # 503
  TOKEN=$(curl -s -H 'Content-Type: application/json' \
    -d '{"username":"t13","password":"<test-password>"}' \
    http://127.0.0.1:8080/api/v1/login | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  curl -s -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab13/repositories/t13-ui/pulls/2
  # {"error":"storage operation failed"}  (HTTP 500)
  ```
  Same result for `acme/web` PR #1 (503 UI / 500 API) and for any PR whose head differs from its target. Up-to-date PRs #3 and #5 return 200.
- **Expected:** 200 with the PR detail, reviews, comments and mergeability, or a computed `unchecked` rebase result.
- **Actual:** UI maps the failure to the styled 503 “Unavailable” page; API returns 500. `merge_checks` contains `merge`/`squash` rows for repository `e861469b…` but no `rebase` row, so the failure is in the rebase mergeability check.
- **Evidence:** Container Git is **2.39.5**; `git merge-tree -h` does not list `--merge-base`, and `git merge-tree --write-tree --merge-base <base> <head> <head>` exits 129 `unknown option 'merge-base'`. `crates/githearth-git/src/merge.rs:284-285` passes `--merge-base=<base>` from the rebase path (`merge.rs:459-461`), and `crates/githearth-server/src/mergeability.rs:205-210` propagates the Git error from `for_pull`; `pulls::read_pull` (`pulls.rs:877-918`) then fails before rendering. The 503 page itself leaks no SQL/backtrace.
- **Scope:** Systematic for every pull request with commits to review; the list, create, comment and up-to-date detail paths are unaffected. The documented minimum Git ≥2.38 in `docs/operations/install.md:24` is not sufficient for this code (see separate docs defect).
- **Note:** The fixture PR is the only PR in `acme/web`; this makes the README's “pull requests” status claim only partially true.

### Invalid tag-protection form input is reported as 503 “Unavailable”
- **Severity: minor**
- **Repro:** Sign in, GET `/organizations/lab13/repositories/t13-ui/settings/tags`, copy the hidden `csrf_token` and the enabled `idempotency_key`, then:
  ```bash
  curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt \
    -d 'csrf_token=<token>&idempotency_key=<key>&pattern=bad//pattern&allowed_to_create=write' \
    http://127.0.0.1:8080/organizations/lab13/repositories/t13-ui/settings/tags
  # 503
  ```
  The same payload through the API (`POST …/tag-protection`) returns **400** `{"error":"invalid input"}`. A bad role (`allowed_to_create=superuser`) behaves the same.
- **Expected:** 400 with a field-level validation message and the form re-rendered (or at least “invalid request”), not a server-unavailable page.
- **Actual:** `503 Unavailable` page with no explanation; the user cannot tell what to fix.
- **Evidence:** `crates/githearth-server/src/tag_protection_ui.rs:96-106` maps every response other than 401/403/404 to `unavailable_page`. Valid input still works: `pattern=v13-*` creates the rule with 303.
- **Scope:** All validation failures submitted through the tag-protection form (pattern, role, and any other 400).

### PR detail emits duplicate `id="comment"` and ambiguous labels
- **Severity: minor**
- **Repro:** GET `/organizations/lab13/repositories/t13-ui/pulls/5` and inspect the forms:
  ```
  <textarea id="comment" name="body" …>   # inline comment form
  <textarea id="comment" name="body" …>   # general comment form
  <label for="comment">Comment on this line</label>
  <label for="comment">Add a comment</label>
  ```
  Both `<textarea>`s and both labels are present in the same document, and both forms post to `…/pulls/5/comments`.
- **Expected:** Unique ids (`comment`, `inline-comment`) and each label associated with its own control.
- **Actual:** Duplicate id; “Add a comment” points at the first (inline) textarea, so the accessible name/description is wrong for both fields.
- **Evidence:** `crates/githearth-server/src/pulls_ui.rs:1002` (inline form) and `pulls_ui.rs:1113` (general form). The PR page is otherwise valid: 0 unclosed tags. `html.parser` duplicate-id check flags only this page.
- **Scope:** Every PR detail page where the inline-comment form is rendered.

### No overflow handling: long strings and wide settings tables break the page
- **Severity: minor**
- **Repro:** Create an issue with a 250-character unbroken title and one with a 5000-character unbroken body, then load them and the issue list in a 1000px-viewport browser:
  ```bash
  # measured with Chrome headless --dump-dom and a small measurement script
  # issue title 250 'C': document.scrollWidth = 5817, clientWidth = 756
  # issue body 5000 'B': document.scrollWidth = 51049, clientWidth = 756
  # issues list (title #104 on page 1): table width = 3065, scrollWidth = 3098, clientWidth = 756
  # /settings at 1000px: scrollWidth = 1102; SSH-key table = 1035; main = 932
  ```
  The screenshot of issue #41 shows the h1 running off the right edge and the body text overflowing the card. The settings SSH-key table clips “Last used”/dates at 1000px even without long user input.
- **Expected:** `overflow-wrap: anywhere`/`word-break: break-word` for user text, `table-layout: fixed` or an `overflow-x: auto` wrapper for wide tables, and truncation for list titles.
- **Actual:** Horizontal page scroll and clipped content.
- **Evidence:** The shell CSS (`crates/githearth-server/src/ui.rs:178-192`) defines only `main{max-width:900px}`, `input{width:100%}` and no overflow rules; no `overflow-x` wrapper exists on any table. Screenshot: `/tmp/t13-audit/long.png` (tester-local).
- **Scope:** All user-controlled long text (titles, bodies, comments, descriptions) and the 7-column settings tables.

### Global `input{width:100%}` stretches checkboxes away from their labels
- **Severity: minor**
- **Repro:** GET `/organizations/lab13/repositories/t13-ui/pulls/new` (or `/settings`, or tag protection) and measure the draft checkbox:
  ```
  checkbox width = 866px, height = 13px
  label text = "Create as draft", rendered at the opposite side of the row
  ```
  The screenshot shows “Create as draft” at the left and a tiny checkbox centered in the form width.
- **Expected:** A native checkbox at its intrinsic size adjacent to its label text (the label is correctly wrapping it, but the layout is not).
- **Actual:** Every checkbox inherits `width:100%`, becoming a full-row control and separating the box from its text; the effective height is 13px, below the 24px touch target even though the label is clickable.
- **Evidence:** `crates/githearth-server/src/ui.rs:184` `input{width:100%;box-sizing:border-box}` has no `:not([type=checkbox])` guard. Affects pull-draft, settings scopes and tag-protection checkboxes.
- **Scope:** All checkbox forms.

### Unmatched URLs return an empty 404 without the styled page or page headers
- **Severity: minor**
- **Repro:**
  ```bash
  curl -i http://127.0.0.1:8080/no-such-page
  # HTTP/1.1 404 Not Found
  # content-length: 0
  # (no content-type, no content-security-policy, no x-frame-options)
  ```
  Known missing resources (`/organizations/lab13/repositories/nope`, issue 9999, PR 9999) return a styled `text/html` 404 with CSP and `X-Frame-Options: DENY`.
- **Expected:** The same styled 404 page and page security headers for unmatched routes.
- **Actual:** Empty body; the page header middleware only runs on matched UI routes, and the router fallback does not use `not_found_page`.
- **Evidence:** `curl` headers above; `ui.rs:222-235` defines `not_found_page`, while `ui.rs:924-943` only layers `page_headers` onto matched routes. `405` responses are also bodyless (note, not counted as a failure).
- **Scope:** All unknown paths.

### `docs/foundation.md` still describes the pre-UI binary
- **Severity: minor**
- **Repro:** Read `docs/foundation.md:3` and `docs/foundation.md:166`.
- **Expected:** The document should describe the shipped server, which has a browser UI, Git Smart HTTP/SSH, issues and reviews.
- **Actual:** Line 3 says the binary “has no public account registration, Git HTTP/SSH transport, issue/review workflow, CI, UI, or replicated storage”; Git HTTP/SSH, issues/reviews and the UI are live. Line 166 says “Closing, reopening and editing issues are API-only”; the pages `/issues/{n}/edit` and `/issues/{n}/state` work (GET 200, POST 303) and were exercised.
- **Evidence:** Live UI pages and SSH clone; `issues_ui.rs` routes for edit/state; tested edit/close/reopen transitions.
- **Scope:** Documentation only; operators and testers reading `foundation.md` get a false capability picture.

### `docs/operations/tls.md` reports implemented security headers as missing
- **Severity: minor**
- **Repro:** Read `docs/operations/tls.md:121-123` and `:128-131`.
- **Expected:** The header table/gaps should match the live response headers.
- **Actual:** It says `X-Content-Type-Options`, `Referrer-Policy` and `Content-Security-Policy` are “missing”, and that CSP was “never implemented”. Live `/`, `/login`, `/settings`, known 404 and 503 responses all carry `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, `X-Frame-Options: DENY` and the CSP in `ui.rs:812-813`. Gap #4 (`X-Forwarded-*` not consumed) is also stale relative to `docs/foundation.md` and the `trusted_proxies`/`client_ip` implementation. The cited `ui.rs:687/688` line references are from an older file revision; the current header code is `ui.rs:915-920`.
- **Evidence:** `curl -D -` headers above; `docs/operations/tls.md:120-131`.
- **Scope:** Documentation only.

### Accessibility review overclaims breadcrumbs and table captions
- **Severity: minor**
- **Repro:** Read `docs/verification/accessibility.md:15`, then inspect the live pages.
- **Expected:** The review should only claim what the pages do.
- **Actual:** The line says “`<nav aria-label="Breadcrumb">` on drill-down pages. Tables add a `<caption>`.” Live blob, commits, compare and tag-settings pages have no Breadcrumb nav; the tree and compare tables have no `<caption>`. (WCAG AA does not require a caption, but the stated “Pass” is not accurate.)
- **Evidence:** `GET .../blob/main/README.md`, `.../commits/main`, `.../compare/main...feature`, `.../settings/tags` — no `nav[aria-label=Breadcrumb]`; `.../tree/main` and `.../compare/...` — `<table>` without `<caption>`.
- **Scope:** Documentation/accessibility review; the missing captions are a best-practice gap, not a measured WCAG AA failure.

### `docs/operations/install.md` minimum Git version is insufficient
- **Severity: minor**
- **Repro:** `docs/operations/install.md:24` says `git` verifies “Git ≥ 2.38 is available”; the live container has Git 2.39.5. A changed-PR detail then fails because `git merge-tree` does not support `--merge-base` (see the blocker above).
- **Expected:** Either the code should not use a flag newer than the documented minimum, or the minimum should be raised and the doctor check updated.
- **Actual:** The documented minimum passes the preflight but the PR detail/rebase mergeability path fails with 500/503.
- **Evidence:** `git --version` 2.39.5; `git merge-tree --write-tree --merge-base …` exits 129; `merge.rs:284-285`.
- **Scope:** Documentation/compatibility; related to the blocker, reported separately because it is a docs-accuracy failure.

## Notes / skipped

- Skipped: dialog/menu patterns (none rendered), reviewer-name XSS (usernames are restricted to lowercase ASCII; no HTML can be stored), request-id (no request-id header or doc claim exists).
- `Secure` is absent from cookies because the Compose profile sets `GITHEARTH_SECURE_COOKIES=false` for loopback HTTP; `HttpOnly` and `SameSite=Lax` are present and the exception is documented in the README/foundation docs.
- `POST /logout` with a completely empty body and no form content type returns 415 from the extractor; with `application/x-www-form-urlencoded` and a missing/empty token it returns 403. This is an edge case, not a CSRF bypass.
- No `<header>` or `<footer>` landmark is rendered; `nav` + `main` are present. WCAG does not require header/footer, and the accessibility review only claims nav/main.
- Nav links measure 18px high; they rely on the WCAG 2.5.8 spacing exception (16px flex gap, no adjacent overlapping targets). Buttons are 35px high.
- The 405 responses have correct status and `Allow` but an empty body; the unmatched 404 is the one counted as an error-page defect.
- `GET /settings/tokens` is 405 (POST-only); tokens and SSH keys are sections of `/settings`, as documented.
- All test data is in `lab13`: `t13-ui`, `t13-xss-desc`, `t13-fork`, 100 issues, 100 comments, tag rule `v13-*`, one revoked token and one active SSH key. `acme` was read-only; `acme/web` PR #1 and issue #2 were observed but not changed.
- The README Compose quick start was not re-run because the campaign forbids restarting/rebuilding the app; its ports, health URL, bootstrap command, named volumes and `--wait`/healthchecks were checked statically, and `/health/ready` plus `githearth-server --help` were verified live.
- The source `docs/verification/accessibility.md` contrast claims were recomputed from the shell CSS and match: body 17.64:1, nav 18.88:1, nav muted 11.76:1, muted 5.36:1, error 7.33:1, focus 5.59:1 on page / 3.16:1 on nav.

## Round 2 — fixed build (2026-11-14)

Build under test: `fix/m4-lab` `7e1ded3` (container Git **2.47.3**, health/ready 200; `githearth check` present). The checkout is still on `main` `b820454`, so the fixed docs were read from `7e1ded3` with `git show`; the working tree does not contain them yet (see Notes). No repository edits and no container restarts by this tester. New fixtures are in `lab13` only: a public repo `t13-public` (README, `feature`, issue #1, PR #1) and issues #105–#107 for the overflow checks.

**Updated verdict: PASS WITH DEFECTS.** The round-1 blocker is fixed. Ten re-run cases still fail, covering eight minor defects (listed below): the five re-checked UX/docs minors are unchanged, tag-protection input still returns 503, and two TLS documentation defects remain, one of which was introduced by the section rename and also breaks a link.

Cases re-run: **47 run, 37 pass, 10 fail, 0 skipped.** Failing cases were re-run once.

### Re-run results

| R2 # | R1 # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|---|
| 1 | 1 | `GET /login` anonymous | 200 HTML | 200 `text/html` | PASS |
| 2 | 2 | `GET /` anonymous / signed in | 303 / 200 | 303 `/login`; 200 | PASS |
| 3 | 3 | Org and repo-list pages | 200 | 200 | PASS |
| 4 | 4 | Repository page | 200 | 200 | PASS |
| 5 | 5 | Issues list | 200, 30/page | 200, 30 rows + `rel=next` | PASS |
| 6 | 6 | Issue detail | 200 | 200 | PASS |
| 7 | 7 | Pulls list | 200 | 200 | PASS |
| 8 | 8 | PR #2 detail (`feature`→`main`) | 200 UI and API | UI 200; API 200; mergeability `merge`/`squash`/`rebase` all `clean` | **PASS (fixed)** |
| 9 | 9 | PR #3 and #5 (up-to-date heads) | 200 | 200 each | PASS |
| 10 | 8 | `acme/web` PR #1 (cross-org) | 200 UI and API | UI 200; API 200 | **PASS (fixed)** |
| 11 | 10 | `/settings` | 200 | 200 | PASS |
| 12 | 11 | Tag settings | 200 | 200 | PASS |
| 13 | 12 | `/notifications` | 200 | 200 | PASS |
| 14 | 13 | Known missing issue/repo/PR | styled 404 | 404 `text/html`, styled | PASS |
| 15 | 14 | Private repo anonymous | deny/redirect | 303 `/login` for `lab13/t13-ui` and `acme/web` | PASS |
| 16 | new | Public repo anonymous browse | 200 with content | repo/tree/blob/commits/compare all 200, rendered content, no login form | **PASS (new)** |
| 17 | new | Public repo anonymous clone | succeeds | `git clone http://…/lab13/t13-public.git` succeeds | **PASS (new)** |
| 18 | 15 | Private repo, signed-in non-member | 404 | `beta/app` 404, no content | PASS |
| 19 | 16 | `/health/live`, `/health/ready` | 200 | 200 both | PASS |
| 20 | 17 | Method not allowed | 405 + `Allow` | 405, `Allow: GET,HEAD` | PASS |
| 21 | 18 | Unmatched URL `/no-such-page` | styled 404 | 404 with **0-byte body**, no `Content-Type`, no CSP, no `X-Frame-Options` | **FAIL (unchanged)** |
| 22 | 19 | One `h1`, heading order | compliant | 7 pages: one `h1`, no skipped levels | PASS |
| 23 | 20 | Landmarks and skip link | present | present on signed-in and anonymous pages | PASS |
| 24 | 23 | Table header scopes | every `th` scoped | pass on all checked tables | PASS |
| 25 | 26 | Duplicate `id` | unique | PR #2/#3/#5: two `id="comment"` and two `label for="comment"` | **FAIL (unchanged)** |
| 26 | 27 | Tag nesting | valid | 0 unclosed/mismatched tags on 7 pages | PASS |
| 27 | 34 | CSP | policy present | exact round-1 policy, incl. `frame-ancestors 'none'` | PASS |
| 28 | 35 | Page security headers | nosniff/referrer/XFO | present on pages, API, Git and known 404; matched on anonymous public pages | PASS |
| 29 | 40 | Issue form without CSRF | 403 | 403 | PASS |
| 30 | 48 | Issue XSS | escaped | 0 `<script>`; payload escaped | PASS |
| 31 | 49 | PR XSS | escaped | 0 `<script>`; `&lt;img` text only | PASS |
| 32 | 55 | Docs relative links | every link resolves | **1 broken anchor**: `docs/verification/tls.md:44` → `tls.md#gaps-filed-as-issues` | **FAIL (regression)** |
| 33 | 58 | Status claim: pull requests | live | changed-head PR detail works (case 8) | **PASS (fixed)** |
| 34 | 59 | `docs/foundation.md` introduction | accurate | line 3 now lists Git HTTP/SSH, issues/PRs/reviews and UI as implemented | **PASS (fixed)** |
| 35 | 60 | `docs/foundation.md` issue UI claim | accurate | line 166 now says edit/close/reopen work in the browser; edit and close re-verified (303) | **PASS (fixed)** |
| 36 | 61 | `docs/operations/tls.md` header/gap table | accurate | nosniff, `Referrer-Policy: same-origin`, CSP and trusted-proxy text now match live; one cache row still wrong (R2 #46) | PASS (flagged rows fixed) |
| 37 | 62 | `docs/verification/accessibility.md` claims | accurate | line 15 unchanged; blob/commits/compare/tags still have no Breadcrumb nav, tree/compare/tags tables no caption | **FAIL (unchanged)** |
| 38 | 63 | `docs/operations/install.md` Git minimum | sufficient | `Git ≥ 2.40` documented (lines 15, 58); image 2.47.3; code rejects `< 2.40` | **PASS (fixed)** |
| 39 | new | `install.md` Install section | present, actionable | `## Install` with Docker Compose and direct-binary steps; commands exist in the image | **PASS (fixed)** |
| 40 | new | Image ships `githearth check` | works | `docker compose exec -T app githearth check /var/lib/githearth` → exit 0, `checked 189 repositories, 0 problems` | **PASS (fixed)** |
| 41 | 66 | 100+ issue pagination | 30/page, no dup | 4 pages, 103 unique issue links | PASS |
| 42 | 67 | Long issue title layout | no page overflow | issue #106: `scrollWidth 5781` vs `clientWidth 1000` | **FAIL (unchanged)** |
| 43 | 68 | Long issue body layout | no page overflow | issue #107: `scrollWidth 51083` vs `clientWidth 1000`, `overflow-wrap: normal` | **FAIL (unchanged)** |
| 44 | 69 | Invalid tag-protection input | 400 + message | **503 “Unavailable”** twice (bad pattern and bad role); API still 400 | **FAIL (unchanged)** |
| 45 | 70 | Checkbox layout | native size next to label | pull/new 866×13; `/settings` 834×13; tag form 866×13 | **FAIL (unchanged)** |
| 46 | new | `tls.md` Git `Cache-Control` row | matches live | doc says `no-cache` for Git; live `info/refs` and `upload-pack` POST both send `no-store` | **FAIL (new)** |
| 47 | new | `docs/verification/tls.md` header status | accurate | still says nosniff/Referrer-Policy/CSP “are not set”; its link to the renamed section is broken | **FAIL (new)** |

### Fixed since round 1

- **PR detail blocker:** `GET …/pulls/2` is 200 in the UI and 200 through the JSON API; `mergeability` now contains all three methods and `rebase` is `clean`. `acme/web` PR #1 behaves the same. The container now has Git 2.47.3, and `git merge-tree --write-tree --merge-base=<base> <head> <head>` exits 0 (2.39.5 rejected the option).
- **Public browsing:** anonymous `repo`, `tree`, `blob`, `commits` and `compare` pages return 200 with full content and no login form; anonymous clone works. Private repositories still return 303 `/login`; a signed-in non-member still gets 404. Issue and pull pages remain session-only by design (`issues_ui.rs`/`pulls_ui.rs` call `session()` unconditionally), so only code browsing is anonymous.
- **Docs:** `install.md` gained the Install section and Git ≥ 2.40 (image 2.47.3, code enforces 2.40 in `githearth-git/src/layout.rs`); `foundation.md` lines 3 and 166 now match the shipped server; the `tls.md` table now records nosniff/Referrer-Policy/CSP and trusted-proxy handling, and the stale “never implemented” gap list is replaced by “Remaining gaps”.
- **CLI:** the image contains `/usr/local/bin/githearth`; `githearth check /var/lib/githearth` exits 0 with `checked 189 repositories, 0 problems`.

### Remaining defects

1. **Unmatched URLs still return a bare 404 (minor, unchanged).** `curl -si http://127.0.0.1:8080/no-such-page` → `404`, `content-length: 0`, no `Content-Type`, no CSP, no `X-Frame-Options`. It now carries `cache-control: no-store`, `nosniff` and `referrer-policy: same-origin` from the global middleware, but the body is still empty and the router fallback still does not call `not_found_page`.
2. **Duplicate `id="comment"` (minor, unchanged).** PR detail pages render two textareas with `id="comment"` and two `label for="comment"` (inline and general comment forms); “Add a comment” still labels the first textarea. Present on PR #2, #3 and #5.
3. **No overflow handling (minor, unchanged).** At a 1000px viewport: 250-character title → `scrollWidth 5781`; 5000-character body → `51083`; issue list table 3065px; `/settings` `scrollWidth 1102` (SSH table 1035). The shell CSS still has no `overflow-wrap`/`word-break` and no table scroll wrapper.
4. **Checkbox stretched by the global `input` rule (minor, unchanged).** `input{width:100%}` still applies to checkboxes: pull/new 866×13, `/settings` 834×13, tag form 866×13, with the label text pushed to the far side.
5. **Invalid tag-protection input still 503 (minor, unchanged).** Valid CSRF + idempotency with `pattern=bad//pattern` or `allowed_to_create=superuser` returns the styled “Unavailable” page (503) twice; the API returns 400 `invalid input`. `tag_protection_ui.rs` still maps every non-401/403/404 response to `unavailable_page`.
6. **`docs/verification/accessibility.md` still overclaims (minor, unchanged).** Line 15 still says “`<nav aria-label="Breadcrumb">` on drill-down pages” and “Tables add a `<caption>`”. Live: tree has a Breadcrumb nav, blob/commits/compare/tag-settings do not; tree, compare and tag-settings tables have no caption. The file was not touched by the fix.
7. **`tls.md` Git cache row is wrong (minor, new).** Lines 119–121 state `Cache-Control: no-store` for “all JSON + git responses” and `no-cache` for `info_refs`/`upload-pack`/`receive-pack`; the live Git endpoints send `no-store` (`GIT_TRACE_CURL` on `info/refs` and POST `git-upload-pack`), because the global `observe` middleware overwrites the handler value. One of the two contradictory rows must go.
8. **`docs/verification/tls.md` is stale and now has a broken link (minor, new).** Line 44 still says `X-Content-Type-Options`, `Referrer-Policy` and `Content-Security-Policy` “are not set” (all are live) and links `../operations/tls.md#gaps-filed-as-issues`; the section was renamed to `### Remaining gaps`, so the anchor no longer exists. This is the only broken relative link in the 46 Markdown files at `7e1ded3`.

### Notes

- All docs checks were made against `7e1ded3`, the commit the running image matches. The repository checkout is still `main` `b820454`, whose `docs/operations/install.md` still says Git ≥ 2.38 and has no Install section; until `fix/m4-lab` is merged, a reader of the checkout sees the old text.
- Two further docs nits not counted as cases: `docs/operations/tls.md`’s cookie/session line references are stale (`ui.rs:131` is `format!`; the session cookie is at `ui.rs:132`), and `docs/verification/m4-signoff.md:33` still lists “Git ≥ 2.38” as the prerequisite.
- Measurements were taken with Chrome headless at a 1000×800 viewport against the authenticated session; the container was not restarted. The tester-local `t13-r2/` directory under the session temp root holds the scripts and captured pages.
- Public issue and pull pages redirect anonymous visitors to `/login`; the fix’s “browsable anonymously” applies to code browsing only. Not counted as a defect because the pages carry write forms, but it is worth stating in the docs.
- Test data added in `lab13`: public repo `t13-public` (README, `src/main.rs`, `feature`, issue #1, PR #1), issues #105 (closed, edited), #106 (250-character title), #107 (5000-character body). No `acme`, `beta` or other lab data was changed; `acme/web` PR #1 and issue #2 were only read.
