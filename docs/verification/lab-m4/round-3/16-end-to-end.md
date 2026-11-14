# End-to-end journey — M4 closure round 3

Tester: `t16` (owner of `lab16`), second actor `carol`. Date: 2026-11-14.
Instance: `main` `800cad4`, HTTP `127.0.0.1:8080`, SSH `127.0.0.1:2222`.
Verdict: **PASS WITH DEFECTS**
Cases: **108 run, 97 pass, 10 fail, 1 not applicable.** First-pass failures caused by test
mistakes or a transient git 503 were each re-run once and are reported as PASS; the
reclassifications are listed in Notes.

Method: real headless Chrome 157 driven over the DevTools protocol
(`--remote-debugging-pipe`; no Playwright/node on this host), so every "UI" step is
a rendered page with real form submits, cookies and CSRF; API checks were made with
`curl`/`urllib` using the same account, and git steps with the real `git` 2.47
client over HTTP and SSH. Two isolated browser profiles were used so `t16` and
`carol` held sessions at the same time.

Fixtures created for this run (nothing of other testers was touched):

- org `lab16-e2e` (created through the API) and private repo `lab16-e2e/r3-web`
- private repo `lab16/r3-app` (API) plus fork `lab16/r3-app-fork` (browser)
- issue `lab16-e2e/r3-web#1`, PRs `lab16/r3-app#1`, `#2`, `#3`,
  PR `lab16-e2e/r3-web#2`
- branch protection rule on `lab16/r3-app` `main`
  (`require_pull_request`, 1 approval, check `ci/build`)

## Results

| # | Case | Expected | Actual | Verdict |
|---|------|----------|--------|---------|
| 1 | `GET /health/live`, `/health/ready` | 200/200 | 200/200 | PASS |
| 2 | Unauthenticated `GET /` | redirect to sign-in | 200 at `/login`, "Sign in" form | PASS |
| 3 | Login page structure | 200, labelled username/password, skip link | 200, `label for=username/password`, `a.skip-link`, autocomplete attrs | PASS |
| 4 | Wrong password | error, no session | 303 `/login?error=1`, alert "Invalid username or password.", no `githearth_session` | PASS |
| 5 | Sign in as `t16` | home, nav with user/Notifications/Settings/Sign out, HttpOnly cookie | 303 `/`, nav `t16`, cookie set | PASS |
| 6 | UI session cookie vs JSON API | (brief: cookie works for API) | cookie → `/api/v1/me` **401**; same token as `Authorization: Bearer` → 200 `t16` | PASS (brief is wrong, see N1) |
| 7 | Browser entry point to create an organization | form/link | none: home lists orgs only; `/organizations/new` → 404 | **FAIL** (D1) |
| 8 | `POST /api/v1/organizations` invalid slug | 400 | 400 `invalid input` | PASS |
| 9 | `POST /api/v1/organizations` `lab16-e2e` | 201 owner | 201 `{"slug":"lab16-e2e","role":"owner"}` | PASS |
| 10 | Duplicate org slug | 409 | 409 `already exists` | PASS |
| 11 | New org on home / org page | listed and 200 | `Lab 16 E2E → /organizations/lab16-e2e`, page 200 | PASS |
| 12 | Browser entry point to create a repository | form/link | none on org or repo pages; `/organizations/lab16/repositories/new` → 404 | **FAIL** (D1) |
| 13 | `POST …/repositories` invalid name | 400 | 400 `invalid input` | PASS |
| 14 | `POST …/repositories` private `r3-web` | 201 private | 201, `visibility=private`, `default_branch=null` | PASS |
| 15 | Repo page after create | 200, empty state, clone URL | "This repository is empty. Clone it with: git clone http://127.0.0.1:8080/lab16-e2e/r3-web.git" | PASS |
| 16 | Repo API vs page | same name/visibility | both `r3-web`, `private` | PASS |
| 17 | Anonymous private repo page | redirect, no content | 303 `Location: /login`, empty body | PASS |
| 18 | Anonymous private repo API | 401/404 | 401 `invalid or expired credentials` | PASS |
| 19 | Org page lists the new repo | link present | `r3-web → /organizations/lab16-e2e/repositories/r3-web` | PASS |
| 20 | `git clone` over HTTP (Basic auth) | empty clone, `main` | `git clone http://t16:…@127.0.0.1:8080/lab16-e2e/r3-web.git` → warning empty, branch `main` | PASS |
| 21 | `git push` over HTTP | new branch | `* [new branch] main -> main` | PASS |
| 22 | Add SSH key from `/settings` form | 303, key listed | submitted form, page lists `e2e-laptop ed25519 SHA256:SjGY…`, 200 | PASS |
| 23 | SSH key API vs settings | one key, same fingerprint | `GET /api/v1/user/ssh-keys` → `e2e-laptop`, same SHA256 | PASS |
| 24 | Invalid SSH key rejected | error alert, key not added | 400 page with `role=alert` "Only Ed25519 and ECDSA public keys are accepted." | PASS |
| 25 | `git clone` over SSH | clone | `GIT_SSH_COMMAND='ssh -i … -p 2222' git clone ssh://git@127.0.0.1/lab16-e2e/r3-web.git` | PASS |
| 26 | `git push` over SSH | fast-forward | `4617dab..d0ffbf8 main -> main` | PASS |
| 27 | Anonymous clone of private repo | not found, no existence leak | `fatal: repository '…' not found` (404) | PASS |
| 28 | Wrong password over HTTP git | auth failure | `fatal: Authentication failed` (401) | PASS |
| 29 | `repo:read` token cannot push | 403 | `The requested URL returned error: 403` | PASS |
| 30 | Direct push to protected `main` | rejected | `remote: protected branch: ref.push` / `pre-receive hook declined` | PASS (message is terse, N6) |
| 31 | Repo overview navigation | browse/commits/compare/issues/pulls/protection links | all present incl. `/settings/branches`, `/settings/tags` | PASS |
| 32 | Tree at `main` | docs, src, README.md, xss.md, a&b.txt | 5 rows, sizes shown | PASS |
| 33 | Breadcrumb on drill-down | `nav[aria-label=Breadcrumb]` | present on tree/blob/commit/issue/PR pages | PASS |
| 34 | Tree subdirectory | `docs/guide.md` | 200, one row | PASS |
| 35 | Tree on `feature/e2e` (slash branch) | 200, branch content | 200 via `/tree/feature%2Fe2e/docs`, `feature.md` | PASS |
| 36 | Blob README | content + size | 200, 88 bytes, text intact | PASS |
| 37 | Blob source file | code visible | 200, `println!` shown | PASS |
| 38 | Blob escapes HTML payload | no `<script>`/`<b>` elements | 0 script, 0 b in `main`, payload shown as text | PASS |
| 39 | Blob missing file / traversal | 404 | 404 "Not found"; `/blob/main/../../../../etc/passwd` → 404, no `root:` | PASS |
| 40 | Commit list UI vs API | same ids | 3 ids identical, newest first | PASS |
| 41 | Commit list unknown rev | 404 | 404 | PASS |
| 42 | Commit detail | summary, author, `diff --git` | 200 with all three | PASS |
| 43 | Compare `main...feature/e2e` | ahead 1 + `docs/feature.md` | 200, diff rendered | PASS |
| 44 | Compare identical revs | explicit no-change summary | 200, "0 ahead, 0 behind · Up to date" | PASS |
| 45 | Compare unknown/malformed spec | 404 / 400 | `revision not found` / `invalid input` | PASS |
| 46 | Tree API vs UI | same entries | identical 5 names | PASS |
| 47 | Unknown repo / unknown org page | 404 styled | 404 "Not found" (styled, nav intact) | PASS |
| 48 | Anonymous private tree page | redirect to login | 303 → `/login` | PASS |
| 49 | Binary blob | notice, no raw bytes | "Binary or truncated file (66 bytes); content is not shown." | PASS |
| 50 | Issues list (empty) | 200 empty state | 200 "No issues…" | PASS |
| 51 | New issue form | title + body | 200, `#title`, `#body` | PASS |
| 52 | Whitespace-only title | 400, no issue | 400 with "A title of 1 to 256 characters is required…", API still `items: []` | PASS |
| 53 | Create issue via UI | 303 to `/issues/1` | 303, title/body rendered | PASS |
| 54 | Issue body HTML escaped | no script element | 0 script elements, payload as text | PASS |
| 55 | Issue API vs page | #1 open, author t16 | `number=1`, `state=open`, `author_id` = t16 | PASS |
| 56 | Comment via UI | comment visible | 200 page, comment shown | PASS |
| 57 | Comments API vs page | 1 comment, same body | 1 item, body matches | PASS |
| 58 | Blank comment | 400 with reason | "A comment must be 1 to 65536 characters and not only spaces." | PASS |
| 59 | Close via UI | API `state=closed` | closed; list default `open` hides it | PASS |
| 60 | Closed tab | issue listed under `?state=closed` | listed; `open` tab does not show it | PASS |
| 61 | Reopen via UI | API `state=open` | `state=open` | PASS |
| 62 | Unknown issue number | 404 | 404 | PASS |
| 63 | Issue with 31 comments + new comment | redirect to page holding it (#159) | `?after=30#comment-32`, new comment visible | PASS |
| 64 | New PR form | source/target/title | 200, `source_repository` select, `source_branch`, `target_branch=main` | PASS |
| 65 | Create PR via UI (`r3-app#1`) | 303 `/pulls/1` | 303, title/state/branches correct | PASS |
| 66 | PR API vs page | open, feature/login → main | same | PASS |
| 67 | Request a reviewer from the PR page | control + list of requested reviewers | **no control and no reviewer list**; only `/reviews` form | **FAIL** (D2) |
| 68 | `POST …/requested_reviewers {"reviewer":"carol"}` | 200, carol listed | 200 `[{"username":"carol",…}]` | PASS |
| 69 | Duplicate review request | idempotent | 200, still one | PASS |
| 70 | Request the author as reviewer | 422 | 422 "the author cannot review their own pull request" | PASS |
| 71 | PR page after a pending request | carol shown as requested | page never mentions carol or "requested" (checked on open PR #3 whose body is just "gate") | **FAIL** (D2) |
| 72 | Carol opens the PR | 200 + review form | 200, `…/reviews` form | PASS |
| 73 | Carol approves via UI | approval recorded | page shows "carol approved", Approvals 1 | PASS |
| 74 | Reviews API vs page | `verdict=approved` by carol | `items:[{verdict:"approved",…}]` | PASS |
| 75 | `t16` sees the approval | Approvals 1 | Approvals 1 | PASS |
| 76 | Post `pending` then `success` check | 201 each | 201 each, PR "Checks" shows `success ci/build … details` | PASS |
| 77 | Merge via UI (unprotected) | merged | 303, API `state=merged`, `docs-login.md` on `main` | PASS |
| 78 | Re-merge a merged PR | 409 | 409 "pull request is not open" | PASS |
| 79 | Merge blocked by protection (UI) | disabled merge with a reason | merge section disappears entirely; no reason text | **FAIL** (D3) |
| 80 | Merge blocked by protection (API) | 409/422 naming the gate | **403 `not permitted`** | **FAIL** (D3) |
| 81 | Merge enabled after approval + check | enabled | enabled, merge succeeds | PASS |
| 82 | Merged PR metadata | merge commit + who/when | page says "Merged · … · by t16 at <created_at>"; API has no `merged_at`/`merged_by`; `closed_at=null` | **FAIL** (D4) |
| 83 | `t16` notification of carol's review | UI + API | `/notifications` "carol reviewed lab16/r3-app #1/#2", 2 unread; API `pull_request.reviewed` | PASS |
| 84 | Carol's review-request notification | UI + API | page lists "t16 requested your review…" for r3-app; API `pull_request.review_requested` | PASS |
| 85 | Mark one read via UI | count drops | 49 → 48 unread; API unread 48 of 50 | PASS |
| 86 | Unread filter API | only unread | `?unread=true` returns 48, all `read_at=null` | PASS |
| 87 | Branch protection settings page | 200 with full form | 200, pattern/approvals/checks/6 toggles | PASS |
| 88 | Empty pattern | 400, no rule | 400 page, "No branch protection rules yet" | PASS |
| 89 | Create rule via UI | rule listed | `main no no yes 1 ci/build no no no` | PASS |
| 90 | Protection API vs UI | identical fields | `pattern=main, required_approvals=1, required_status_checks=[ci/build], allow_force_push=false, require_pull_request=true` | PASS |
| 91 | Duplicate pattern | rejected | one rule remains, error page | PASS |
| 92 | Delete rule via UI | API empty | `items: []`, `generation` bumped 1→2 | PASS |
| 93 | Push to an unprotected branch | allowed | `HEAD:refs/heads/scratch` rc=0 | PASS |
| 94 | Fork form on repo page | org select + name | `organization` select (`lab16`, `lab16-e2e`), `name` | PASS |
| 95 | Fork via UI | redirect to fork | `/organizations/lab16/repositories/r3-app-fork` | PASS |
| 96 | Forks API | parent/children/count | `{"parent":null,"children":[{…r3-app-fork…}],"count":1}` | PASS |
| 97 | Fork page shows its parent | "forked from r3-app" | no parent link, no fork metadata | **FAIL** (D5) |
| 98 | Upstream page shows forks | fork list/count | not shown | **FAIL** (D5) |
| 99 | Cross-repo PR form lists the fork | selectable source | options `This repository (r3-app)`, `lab16/r3-app-fork` | PASS |
| 100 | Cross-repo PR via UI | PR with fork as source | 303 `/pulls/2`, API `source_repository_id` = fork id | PASS |
| 101 | Carol approves the cross-repo PR | approval | approved, Approvals 1 | PASS |
| 102 | Cross-repo merge gated then merged | merge after approval+check | merge enabled only after both; `state=merged`; `FORK.md` on upstream `main` | PASS |
| 103 | Fork lists its outgoing PR (UI) | PR listed | "Pull requests opened from this fork": both PRs | PASS |
| 104 | Fork PR list API | same list | `GET …/r3-app-fork/pulls?state=all` → `items: []` | **FAIL** (D6) |
| 105 | Audit log lists the journey | org/repo/issue/PR/merge/push/protection/fork events | `organization.created, repository.created, repository.pushed, issue.created, issue.commented, issue.closed, issue.reopened, pull_request.created, pull_request.review_requested, pull_request.approved, pull_request.merged, commit_status.created, branch_protection.created/deleted, repository.forked` | PASS |
| 106 | Audit chain verification | `ok:true` | `{"rows":15,"ok":true,"first_bad_seq":null}` (lab16-e2e), `rows:22` for lab16 | PASS |
| 107 | Audit pagination | cursor + next page | `limit=2` → 2 items, `next=2` | PASS |
| 108 | Audit UI | page in the browser | none exists (API only) | N/A (N4) |

## Defects

### D1 — A new user cannot create an organization or a repository in the browser
- Severity: **major** (blocks the documented "sign in → create org → create repo" journey; it is not described as a deliberate limit)
- Repro: sign in as `t16` → `/` shows only existing org links; `GET /organizations/new` → 404;
  `GET /organizations/lab16/repositories/new` → 404; no form on the org or repo page.
  The only working path is `POST /api/v1/organizations` then `POST /api/v1/organizations/{org}/repositories`.
- Expected: a browser path to create an organization and a repository (or docs stating it is API-only).
- Actual: UI routes are read-only: `ui.rs:988-992` (`/organizations/{slug}` GET, `/…/repositories` GET,
  `/…/repositories/{name}` GET, fork POST) — no create route; `docs/foundation.md` §"Browser login"
  and §"Issue pages" never mention how a browser user creates an org/repo.
- Evidence: `GET /organizations/new` → 404 (styled), `/organizations/lab16/repositories/new` → 404;
  home page links only `Lab 16`, `Lab 16 E2E`; `POST /api/v1/organizations` → 201.

### D2 — Requesting a reviewer is API-only, and requested reviewers are never shown
- Severity: **major** (the PR review workflow cannot be started from the browser; another actor's request is invisible to the author)
- Repro: open `/organizations/lab16/repositories/r3-app/pulls/3` (open PR, no reviews). The page has
  draft/comment/inline/auto-merge/review forms but no "Request review" control and no reviewer list.
  `POST /api/v1/…/pulls/3/requested_reviewers {"reviewer":"carol"}` → 200 and carol appears in the API list,
  but reloading the page still never mentions her.
- Expected: a control on the PR page to request a reviewer and a visible "Reviewers / awaiting review" list.
- Actual: `pulls_ui.rs` renders no requested-reviewer section (no `/requested_reviewers` form anywhere in
  the crate; the only UI review form is `…/reviews`). API: `pulls.rs:66-71`.
- Evidence: page text on PR #3 contains `Reviews`, `No reviews yet.`, `Submit review` and no `carol`;
  API returns `[{"username":"carol",…}]`. The same gap forced this journey's review step through the API.

### D3 — A protection-blocked merge gives no reason (form vanishes; API says 403)
- Severity: **minor–major** (the gate is correct, but the user cannot tell what is missing; 403 is indistinguishable from "you may not merge")
- Repro: with the `main` rule (`require_pull_request`, 1 approval, check `ci/build`) and an open PR with
  0 approvals, open `/organizations/lab16/repositories/r3-app/pulls/3` → there is no merge section at all
  (only "Enable auto-merge"). `POST …/pulls/3/merge` with the head oid → `403 {"error":"not permitted"}`.
- Expected: a disabled merge button with "1 approving review required / Required check ci/build is expected",
  and an API error (409/422) naming the unmet gate.
- Actual: `pulls_ui.rs:1095` `show_merge_form = … && can_merge`, where `may_merge` folds protection facts
  into the permission answer, so the whole section disappears; `merge_intents.rs:341-376` returns
  `403 not permitted` whenever `merge_allowed` is false for *any* reason (policy included).
- Evidence: PR #3 page: no "Merge pull request" button; API 403 as above; after approval + `ci/build`
  success the same page renders the merge form and merges. (Round 2 saw the missing form and passed it;
  the missing reason and the 403 semantics are the new observation.)

### D4 — A merged PR does not record or show the merge commit, merger or merge time
- Severity: **minor**
- Repro: `GET /api/v1/organizations/lab16/repositories/r3-app/pulls/1` after merge, and the PR page.
- Expected: `merged_at`, `merged_by` and the merge commit (GitHub shows "carol merged commit abc123 into main").
- Actual: JSON has `state:"merged"` but no `merged_at`/`merged_by`/merge sha, and `closed_at` is `null`
  (DB has `merged_at`/`merged_by` set). The page prints `by {author} at {created_at}` — the *creation* time —
  for a merged PR (`pulls_ui.rs:1230`), and `PULL_COLUMNS` (`pulls.rs:190-194`) does not select the merge fields.
- Evidence: DB row `state=merged, merged_at=2026-11-14 06:48:35+00, merged_by=t16, closed_at=NULL`;
  page: `Merged · feature/login → main · by t16 at 2026-11-14T06:48:22Z` (06:48:22 = `created_at`).

### D5 — Fork pages have no parent link or fork list, although the API now has both
- Severity: **minor** (issue #156's API half is fixed; the UI half is not)
- Repro: open `/organizations/lab16/repositories/r3-app-fork` and the upstream `/…/r3-app`.
- Expected: "forked from lab16/r3-app" on the fork, and a fork count/list on the upstream.
- Actual: neither page renders `parent_repository_id` or children; only the API does
  (`forks.rs:67`, `GET …/forks` → `{parent, children, count}`). `ui.rs`/`browse_ui.rs` never read the field.
- Evidence: fork page text has no parent mention; upstream page has no fork section; API `count:1`.

### D6 — A fork's PR list API still omits its outgoing cross-repo PRs (UI shows them)
- Severity: **minor** (API/UI disagree; issue #156 asked for outgoing PRs in the list)
- Repro: `GET /api/v1/organizations/lab16/repositories/r3-app-fork/pulls?state=all` → `{"items":[],"next":null}`,
  while `/organizations/lab16/repositories/r3-app-fork/pulls` lists both outgoing PRs.
- Expected: the fork's list includes PRs whose head lives in the fork (per #156), or the divergence is documented.
- Actual: the API list is deliberately target-only (`pulls.rs:876-878` comment) and the UI adds a separate
  "Pull requests opened from this fork" section (`pulls_ui.rs:353-378`), so API consumers see nothing.
- Evidence: bodies above; both PRs (#2 merged, #3 open) are visible in the UI section.

## Notes

**N1 — the brief's cookie claim is wrong.** `githearth_session` is a UI-only credential:
`browser_auth` (`ui.rs:105-111`) hashes the cookie value, while every `/api/v1` route uses
`bearer_credential` (`lib.rs:659-693`), which reads only `Authorization`. Sending the browser cookie to
`/api/v1/me` returns 401; sending the same value as `Authorization: Bearer` returns 200. That is a coherent
design (and what round 1 tested), just not what the round-3 brief says. Not counted as a defect.

**N2 — a brand-new organization has no way to add a colleague.** `lab16-e2e` was created by `t16`; carol is not
a member, and membership administration is explicitly later work (`docs/foundation.md:111`). Repository
access grants refuse non-members (`migrations/0004_repository_grants.sql:35-39`), so the review/approval
steps could not run in the new org at all. I therefore ran the multi-actor part of the journey in the
pre-seeded `lab16` (where carol is `maintain`) and kept `lab16-e2e` for the single-actor half. This is a
documented limit, but it means the "brand-new user invites a reviewer" journey is impossible today.

**N3 — deliberate API-only steps seen in the journey** (each documented or unavoidable):
organization/repository creation (D1), reviewer requests (D2), commit statuses (CI is external; the server
does not execute CI), and audit reads (no audit page). Everything else — sign-in, SSH keys, tokens, browse,
issues, PR create/approve/merge, protection rules, forks, cross-repo PRs, notifications — worked in the browser.

**N4 — no audit UI.** `GET /api/v1/organizations/{org}/audit` and `/audit/verify` work (and verify `ok:true`
over 15/22 rows), but there is no page for them. Not counted as a defect (operations runbook uses curl).

**N5 — transient git 503 under concurrent testers.** During one push the server answered
`503 git requests busy` (8 `git_concurrency` slots; `git_http.rs:1405`, `Retry-After: 5`) for ~30 s while
another tester was active. Retried once and it passed; the limit and header are by design, but two testers
were enough to saturate it, which is worth knowing for capacity claims.

**N6 — terse protected-branch message.** A refused push prints `remote: protected branch: ref.push`
(`git_admission.rs:732`) with no rule name or reason (e.g. "pull request required"). Minor.

**N7 — the `main` branch cannot be switched from the browser.** There is no branch selector on tree/blob/commits
pages; other branches are reachable only through compare/PR links or by typing `/tree/<branch>`. Minor
friction; not filed as a defect because nothing claims a picker.

**Test mistakes corrected on re-run (not defects):** the anonymous-repo 200 (my HTTP client followed the 303);
`/tree/feature/e2e/docs` (slash branch must be `%2F`); "commit detail has no diff" (I picked the initial
commit); "compare identical has no message" (it says "Up to date"); the reviewer/review API field names
(`username`, `verdict`); "PR page shows carol" (false positive from the PR body containing "Needs carol's
review" — rechecked on a PR with a neutral body); carol's notifications page showing the sign-in form (my
helper re-submitted the logout form on `/login` while already signed in); protection list length (the API
returns `{generation, items}`, not an array).

**Exact commands used (representative).**

```
curl -s -X POST http://127.0.0.1:8080/api/v1/login -H 'content-type: application/json' \
  -d '{"username":"t16","password":"<test-password>"}'
curl -s -X POST http://127.0.0.1:8080/api/v1/organizations -H "Authorization: Bearer $T" \
  -H 'content-type: application/json' -d '{"slug":"lab16-e2e","name":"Lab 16 E2E"}'
curl -s -X POST http://127.0.0.1:8080/api/v1/organizations/lab16-e2e/repositories \
  -H "Authorization: Bearer $T" -H 'content-type: application/json' \
  -d '{"name":"r3-web","description":"Round 3 end-to-end repo","visibility":"private"}'

git clone http://t16:<test-password>@127.0.0.1:8080/lab16-e2e/r3-web.git
git push origin main
export GIT_SSH_COMMAND='ssh -i …/work/id_ed25519 -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null -p 2222'
git clone ssh://git@127.0.0.1/lab16-e2e/r3-web.git r3-web-ssh

curl -s -X POST …/pulls/1/requested_reviewers -H "Authorization: Bearer $T" \
  -H 'content-type: application/json' -d '{"reviewer":"carol"}'
curl -s -X POST …/commits/$HEAD/statuses -H "Authorization: Bearer $T" \
  -H 'content-type: application/json' -d '{"state":"success","context":"ci/build","description":"green"}'
curl -s "http://127.0.0.1:8080/api/v1/organizations/lab16/audit/verify" -H "Authorization: Bearer $T"
```

**Screens** (headless Chrome, saved next to this report under `shots/`):
`01-login`, `02-login-badpassword`, `03-home`, `04-notifications`, `05-settings`,
`06-unknown-org`, `08-home-two-orgs`, `09-new-org`, `10-empty-repo`, `11-org-with-repo`,
`12-settings-key-added`, `13-settings-bad-key`, `14-repo-overview`, `15-tree-main`,
`16-tree-docs`, `17-blob-readme`, `18-blob-source`, `19-blob-xss`, `20-blob-404`,
`21-commits`, `22-commit-detail`, `23-compare`, `24-compare-empty`,
`25-issues-empty`, `26-issue-blank-title`, `27-issue-created`, `28-issue-comment`,
`29-issue-closed`, `30-issues-closed-tab`, `31-new-pr`, `32-pulls-empty`,
`33-pr-detail`, `34-pr-open`, `35-pr-carol-view`, `36-pr-approved`,
`37-pr-checks-pending`, `38-pr-checks-success`, `39-pr-merged`,
`40-notifications-t16`, `41-notifications-carol`, `42-notifications-read`,
`43-protection-empty`, `44-protection-empty-pattern`, `45-protection-created`,
`46-protection-listed`, `47-protection-deleted`, `48-fork-created`, `49-fork-page`,
`50-upstream-forks`, `51-cross-repo-pr`, `52-merge-gated`, `53-cross-repo-approved`,
`54-merge-blocked-check`, `55-merge-ready`, `56-cross-repo-merged`,
`57-fork-outgoing-pr`, `58-gated-full`, `59-fork-pulls`, `60-binary-blob`,
`61-issue-many-comments`, `62-comment-after-31`, `63-pr3-requested-reviewer`.

**What worked well:** the whole git surface (HTTP + SSH, protected push), browsing
(tree/blob/commits/compare with escaping and 404s), issues (create/comment/close/reopen/pagination,
including the #159 redirect), PR create/approve/merge with checks, the new branch-protection
settings page, forks, cross-repo PRs and notifications were consistent between the rendered pages
and the JSON API, and the audit chain verified across the whole journey.
