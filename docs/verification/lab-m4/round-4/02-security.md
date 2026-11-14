# 02 — Security verification (M4 closure, round 4)

- Date: 2026-11-14
- Instance: `main` `cd7946b` (PR #163 merged), Docker Compose quick start,
  HTTP `http://127.0.0.1:8080`
- Tester account: `t02` / `<test-password>`, organization `lab02` (owner)
- Cross-tenant probes: `lab03`–`lab18`, `beta`, `acme`; secondary accounts
  `erin` (read-only in `lab02`), `frank` (triage-only after fixture revocation),
  `dave` (admin), `grace` (member only; admin grant revoked in the fixture),
  `bob` (write), `t05` (no `lab02` access), `t03` (read in `acme`)
- Method: black-box HTTP only, no repo edits, no GitHub posts. Read-only SQL for
  fixture facts. Harness: `sec_harness.py` (kept beside this report);
  raw results `sec_results.json`, console log `run_final.log`.

## Verdict

**PASS — no security defects found.** All 259 cases pass. The #153 class is
closed: merge and auto-merge answer `404 {"error":"not found"}` on a pull
request the caller cannot read, before any open/merged/draft state is
consulted, for both the JSON API and the browser forms. NUL never reaches the
database (400/404, zero 503s). Tenant 404s are byte-identical across
missing/unreadable/private targets. Extractor rejections and unknown `/api`
paths keep the JSON envelope with `X-Frame-Options: DENY`. Every new browser
form is CSRF-checked and session-bound, and a read-only member cannot create a
repository, protection rule, reviewer request or merge.

## Counts

| Area | Cases | Pass | Fail |
|---|---|---:|---:|
| A. Merge / auto-merge read-auth before state (#153) | 24 | 24 | 0 |
| B. NUL in UI and API path/field | 26 | 26 | 0 |
| C. Uniform tenant 404 (API + UI) | 14 | 14 | 0 |
| D. JSON envelope: extractor + unknown `/api` | 25 | 25 | 0 |
| E. Security headers on rewritten 400 / UI 400 | 7 | 7 | 0 |
| F. CSRF on org / repo / reviewer / merge forms | 25 | 25 | 0 |
| G. Read-only member and read-only token | 17 | 17 | 0 |
| H. Existence oracle through the new pages | 13 | 13 | 0 |
| I. Adversarial sweep lab03–18 / beta / acme | 56 | 56 | 0 |
| J. Extractor / CSRF / oracle extras | 15 | 15 | 0 |
| K. Escaping and page headers on new pages | 18 | 18 | 0 |
| L. Adjacent checks (fork, 405, reviewer visibility) | 10 | 10 | 0 |
| M. Reviewer-form permissions | 5 | 5 | 0 |
| N. Org member without a read grant | 4 | 4 | 0 |
| **Total** | **259** | **259** | **0** |

Observed status distribution: 404×104, 400×40, 403×36, 200×60, 201×2, 204×3,
303×6, 409×4, 401×2, 405×1, 422×1. **503×0.**

Every failure seen while building the suite was a harness-side expectation
error (wrong `expected_head_oid`, comparing bodies that contain the
per-request idempotency key or the caller-supplied slug/PR number, a `GET` on a
`PATCH`-only route, and fixture grants that prior rounds had revoked). Each was
re-run once after correction and confirmed against the source; the consolidated
final run is 259/259. No product defect was re-run into a pass.

## Results

### A. Merge and auto-merge authorize the read before reporting state (24)

All 17 cases against a target `t02` cannot read returned **404
`{"error":"not found"}`**, never 409:

| Case | Target | Underlying state | Result |
|---|---|---|---|
| A01 | `lab04/r3-ui#1` merge | merged | 404 |
| A02 | `lab04/r3-ui#2` merge | open | 404 |
| A03 | `lab04/r3-ui#14` merge | closed | 404 |
| A04 | `lab08/t08-pulls#35` merge | draft | 404 |
| A05 | `lab09/t09-merge#33` merge | draft | 404 |
| A06 | `lab12/r3-methods-1794639714#19` merge | draft | 404 |
| A07–A09 | missing PR / repo / org | — | 404 |
| A10–A11 | `GET`/`DELETE` merge intent | merged PR | 404 |
| A12–A17 | auto-merge enable/read/cancel | merged/open/draft | 404 |

Controls on a readable repository still report state: A18 (draft) and A20
(auto-merge draft) → `409 "pull request is a draft"`, A19 (merged) → `409
"pull request is not open"`. A read-capable caller who may not merge gets
**403 `not permitted`** (A21 `erin`, A22 `t05` on a public repo), and A23/A24
(no read) get 404. `acme/web#1` (readable) → 409 head-moved, not 404.

Root cause: `pulls::require_pull` (pulls.rs:455–495) returns 404 when the
repository read is denied; `create_intent` calls it at merge_intents.rs:526
*before* the `state != "open"` / `draft` checks at merge_intents.rs:529–535,
and `enable_auto_merge` does the same at auto_merge.rs:235 before
auto_merge.rs:237–242. GET/DELETE reuse the same gate.

### B. NUL in every UI and API path/field (26)

23 cases → 400, 3 UI path cases → styled 404, **zero 503**. Covered: NUL in
org/repo/PR/comment/access/status/compare paths; `%00` in `limit`/`after`
query values; NUL in org slug/name, repo name/description, PR title/body,
comment body, review body, `expected_head_oid`, auto-merge method, tag and
branch protection patterns, issue title; and the browser forms for create-org,
create-repo and reviewer. API bodies are `{"error":"invalid input"}`; UI NUL
paths render the styled 404 page (ui.rs:229–242) instead of a database 503.

Root cause: `has_nul`/`valid_path_param` (lib.rs:493–501) guard every path
parameter (`repository_api.rs:97`, `pulls.rs` path handlers, `pulls_ui.rs`,
`ui.rs:845`) and every text field before binding.

### C. Uniform tenant 404 (14)

Seven API 404s (org UUID not a member; org UUID missing; repo in unreadable
org; missing repo in unreadable org; missing repo in readable org; PR in
unreadable repo; missing PR in readable repo) are **byte-identical**:
`{"error":"not found"}`, `application/json`. Five UI 404s (unreadable org,
missing org, repo in unreadable org, missing repo, missing org+repo) are
byte-identical styled pages (`<h1>Not found</h1>`).

Root cause: one `not_found()` constructor per module with the same static
message, and `require`/`require_pull` collapse "denied" to 404 unless the
caller can already read (repository_api.rs:198–202, pulls.rs:490–494).

### D. JSON envelope for malformed extractors and unknown `/api` (25)

15 requests + 10 envelope assertions. Malformed `limit`/`after` (non-numeric,
`%00`), non-UUID org path, non-int PR number, non-UUID comment/notification/
SSH-key ids, and bad percent-encoding all return 400
`{"error":"invalid input"}` with `Content-Type: application/json`. `/api`,
`/api/`, `/api/v1/nope` (GET and POST), `/api/unknown?x=1` and `/api/v9/...`
all return 404 `{"error":"not found"}` as JSON.

Root cause: `json_rejections` (lib.rs:396–426) rewrites plain-text 400s under
`/api` (including `/api` itself), and `fallback_page` (ui.rs:246–273) answers
unknown `/api` paths with JSON while other paths get the styled page.

### E. Headers on rewritten 400s (7)

All six rewritten API 400s carry `X-Frame-Options: DENY`,
`X-Content-Type-Options: nosniff`, `Cache-Control: no-store` and
`Referrer-Policy: same-origin`. The UI 400 page from a rejected form carries
`X-Frame-Options: DENY` **and** the page CSP
(`default-src 'none'; frame-ancestors 'none'; …`, ui.rs:1216–1218).

Note: API JSON responses do not carry a CSP — that is true of every API
response, not just the rewritten 400, and CSP has no effect on a JSON body.
If the M4 contract literally requires CSP on the rewritten 400, that single
header is absent; it is not a security gap.

### F. CSRF on the new and changed forms (25)

- `POST /organizations/new`: no token → 403, wrong token → 403, another
  session's token → 403, valid → 303 (org created).
- `POST /organizations/lab02/repositories/new`: no/wrong token → 403, missing
  idempotency key → 400, valid → 303 (repo created, then deleted).
- Reviewer add/remove, comment, review, merge, auto-merge, ready/draft,
  merge-cancel, resolve: all 403 without a valid token.
- Branch and tag protection settings forms: 403 without a token.
- Fork form: 403 without a token; NUL fork name → 400.
- The create-org, create-repo, repo, pull and pull-list pages all emit a
  `csrf_token` hidden field.

Root cause: `form_session`/`csrf_matches` (pulls_ui.rs:164–176,
ui.rs:770/872, branch_protection_ui.rs:59, tag_protection_ui.rs) compare
`sha256(supplied)` with `sha256(session-token-derived expected)`, so a token
minted for one session is refused by another (F05, J06).

### G. Read-only member and read-only token (17)

- `erin` (read-only): create repo via API and UI → 403; branch protection →
  403; tag protection → 403; merge → 403; reviewer request/remove → 403.
  Her org page omits the "New repository" link.
- `frank` (triage only) create repo → 403.
- `dave` (admin) create repo → 201, delete own repo → 204 (positive control).
- A `repo:read` personal access token: GET repo → 200; create repo/org/
  protection, merge → 403 (the `enforce_read_only_boundary` layer,
  lib.rs:330–367, fails closed before the handler).

Root cause: `repository_api::create` checks `RepositoryCreate`
(repository_api.rs:244–257) and keeps 403 for a member without the grant while
returning 404 to a stranger (#133); protection create requires
`RepositoryUpdate` (protection.rs:395, tag_protection.rs:348).

### H. No existence oracle through the new pages (13)

`/organizations/{slug}/repositories/new` returns 200 for an existing but
unreadable org **and** for a missing org, with identical pages apart from the
caller-supplied slug echo and the per-request idempotency key. The page leaks
no org name, repo name or repository list. Submitting to either returns the
same 404 page body. A read-only member of a readable org gets 403 (documented
#133 distinction) while a stranger gets 404. The pull page for an unreadable
PR, a missing PR and a missing repo returns the same 404 page.

### I. Adversarial sweep (56)

49 of 56 probes across `lab03`–`lab18` and `beta` returned 404 — private
repository reads, org pages, repo lists, pull lists, protection lists, issue
lists, access lists, and org audit/verify. `t02` is not a member of any of
them. Controls behaved correctly: `acme/web` and `acme/site` → 200 (t02 has
read), `lab02/r3-priv` and `r3-upstream` → 200, `beta/app` → 404, anonymous
reads → 401, `acme/web#1` merge → 409 (readable, not 404).

### J–N. Extras (52)

Non-UUID ids on `PATCH comments`, notifications and SSH keys → 400 JSON;
branch/tag protection forms without CSRF → 403; cross-session CSRF → 403;
reserved `new` slug/repo name via UI and API → 400; anonymous new-org/new-repo
pages → 303 to `/login`; reviewer and merge forms on unreadable vs missing
PRs → identical 404 pages. HTML in a repo description and in PR titles
(`<b>x</b>`, `<script>alert(1)</script>`) is escaped on the repo page, pull
page and pull list; every page carries XFO `DENY` + CSP. A `lab02` member
without a read grant (`grace`) gets 404 on the private repo and pulls, 404 on
merge, 200 on the public repo.

## Defects

None.

## Observations (not defects)

1. **Reviewer form is visible to a read-only viewer.** `erin` sees "Request a
   review from" on `r3-upstream#8` because the section is gated on
   `may_comment` (pulls_ui.rs:875), which the `read` group includes; the
   submit is correctly refused (M01, 403) because approval needs the `write`
   group (pulls.rs:2177, schema `pull_request.approve in ["write"]`). This is
   the same class the round fixed for the auto-merge control; consider gating
   the reviewer form on the same `require_review_permission` predicate.
2. **Reviewer errors distinguish account existence.** Requesting `bob`
   (cannot approve) and `nosuchuser` both return 422 but with different
   messages ("reviewer cannot approve…" vs "reviewer not found"). Only a
   caller who can already comment sees it, so the exposure is small; it is an
   account-existence hint, not a tenant oracle.
3. **`PUT /api/v1/me` → 405 with an empty body**, not a JSON envelope. 405 is
   outside the stated contract (malformed extractors, unknown paths); flagging
   for consistency only.
4. **API 400s have XFO but no CSP** (see E). Page CSP is present everywhere it
   matters.
5. **Sudo window.** An aged session token gets `403 not permitted` on a
   sensitive delete (repository delete). This is the documented fresh-auth
   forbid, not an authorization failure; re-authentication fixes it.

## Notes and fixture impact

- Harness-side corrections during the run (all re-run once): use the real
  head oid to reach merge authorization; normalise the per-request idempotency
  key and caller-echoed slug/PR number when comparing pages; use `PATCH` for
  the comment-id route; reviewer must hold `write` (bob, not erin); the
  fixture's `grace`/`frank` grants were revoked by earlier rounds, so `dave`
  was used for the admin positive control.
- Artifacts: one repository per write case was created and deleted (all show
  `deleted`). Seven empty organizations `r4sec-*` remain because the product
  has no organization-delete endpoint; they contain only the creator's owner
  grant and no repositories.
- No source files were modified; no GitHub API calls were made.
