# Browser organization/repository creation and pull-request polish — M4 lab acceptance report (round 4)

Tester: t01. Date: 2026-11-14. Instance: <http://127.0.0.1:8080>
(main `cd7946b`, PR #163 merged, Docker Compose quick start).

Verdict: **FAIL** — two UI-affordance defects in the new browser surfaces. All
core create/validate/authorize paths and every regression area pass; neither
defect is a privilege escalation or data-integrity problem, but both present
controls that cannot succeed.

Cases: **137 run, 133 pass, 4 fail, 0 skipped**. Each failure was re-run once
and failed again; no test passed only on retry and no unrelated case was flaky.

Scope: browser organization and repository creation (form structure, validation,
CSRF, idempotency, duplicate 409, reserved `new`, NUL paths, read-only link
visibility), reviewer request/removal on the pull page (list, form, CSRF,
unknown/non-collaborator/self reviewer, drafts, closed and merged PRs),
the fork parent note, merged-by/time, blocked-merge explanations, branch
settings (dismiss-stale column and status-check textarea), and a regression
smoke of the organization, repository, pull and issue pages.

## Setup

- Accounts: `t01` (owner of `lab01`), `carol` (maintain), `dave` (admin),
  `erin` (read), `frank` (triage), `grace` (member, no repository access).
  All use `<test-password>`.
- Browser sessions only for the flows under test: every GET/POST below used a
  `githearth_session` cookie and the page's CSRF token. The JSON API was used
  only to seed fixtures (branches, pull requests, reviews, a protection rule)
  and to cross-check reviewer/merge state.
- Fixtures (all new, `r4b-*`): `lab01/r4b-core-8340fd` (private; `main`
  protected with require-PR, 1 approval, dismiss-stale; branches `feature`,
  `draft-feature`, `blocked-feature`; PRs open #1, draft #2, approval-blocked #3),
  `lab01/r4b-merge-8340fd` (private,
  unprotected; six PRs covering merged (twice), changes-requested, approved,
  closed and conflicting states), `lab01/r4b-fork-8340fd` (private fork of
  `r4b-p-8340fd/parent`, a private org erin cannot read), plus new organizations
  created through the UI. `lab01/t01-public` was used for the anonymous smoke.
- Git over HTTP (username + password) seeded commits and branches; all repository
  writes stayed inside the `r4b-*` fixtures. No application or database restart.
  Read-only SQL was used only to confirm that a replayed branch-rule create
  produced one row.

## Result summary by area

| Area | Cases | Pass | Fail |
| --- | --- | --- | --- |
| Organization creation | 28 | 28 | 0 |
| Repository creation | 31 | 28 | 3 (one defect) |
| Reviewer request/remove | 24 | 23 | 1 (one defect) |
| Fork parent note | 5 | 5 | 0 |
| Merged PR | 6 | 6 | 0 |
| Blocked merge | 11 | 11 | 0 |
| Branch settings | 12 | 12 | 0 |
| Regression smoke | 20 | 20 | 0 |
| **Total** | **137** | **133** | **4** |

## Results

### Org

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| O01 | New organization page signed out | 303 to /login | 303 /login | PASS |
| O02 | New repository page signed out | 303 to /login | 303 /login | PASS |
| O03 | Organization form structure | slug/name labels, maxlength, help text, CSRF | all form controls and help text present | PASS |
| O04 | Organization CSRF token matches session token | same 32-hex token as page nav | token ae031807... matches nav | PASS |
| O05 | Create organization with valid slug | 303 to /organizations/&lt;slug&gt; | 303 /organizations/r4b-ui-org-0224c9 | PASS |
| O06 | Created organization page | 200, name, slug, New repository link | org page renders name, slug and create link | PASS |
| O07 | Created organization appears on home | home lists the org | org listed on home | PASS |
| O08 | Duplicate organization slug | 409, form re-rendered with error | 409, error and preserved slug | PASS |
| O09 | Organization slug uppercase rejected | 400 form page | 400 with form and error | PASS |
| O10 | Organization slug with space rejected | 400 | 400 | PASS |
| O11 | Reserved slug new rejected | 400 | 400 with form | PASS |
| O12 | Organization slug too long | 400 for 65 chars | 400 | PASS |
| O13 | Organization slug empty | 400 | 400 | PASS |
| O14 | Organization slug double hyphen | 400 | 400 | PASS |
| O15 | Organization slug leading hyphen | 400 | 400 | PASS |
| O16 | Organization slug trailing hyphen | 400 | 400 | PASS |
| O17 | Missing CSRF on organization create | 403 Forbidden page | 403 Invalid CSRF token | PASS |
| O18 | Wrong CSRF on organization create | 403 | 403 | PASS |
| O19 | Signed-out POST to organization create | 303 to /login | 303 /login | PASS |
| O20 | Organization name HTML is escaped | escaped, no raw &lt;script&gt; | script escaped in org page | PASS |
| O21 | Unknown organization page | 404 styled | 404 Not found | PASS |
| O22 | NUL in organization path | 404 | 404 | PASS |
| O23 | Non-member cannot read private organization | 404 for erin on new org | 404, no name leak | PASS |
| O24 | Read-only member sees no New repository link | lab01 page for erin lacks link | no create link | PASS |
| O25 | Organization trailing slash | 404 | 404 | PASS |
| O26 | CSRF token from another session is rejected | 403 | 403 for foreign CSRF token | PASS |
| O27 | Organization name empty | 400 | 400 | PASS |
| O28 | Organization name length boundary | 200 chars accepted, 201 rejected | 303 / 400 | PASS |

### Repo

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R01 | Repository form structure | name/description/visibility, idempotency, CSRF | all controls present, private default | PASS |
| R02 | Create repository with valid name | 303 to repository page | 303 repository page | PASS |
| R03 | Created repository page | 200 with name, visibility, description | repo page renders metadata | PASS |
| R04 | Created repository listed on organization page | org page links the repo | repo listed | PASS |
| R05 | Duplicate repository name | 409 form page with error | 409, error, preserved name | PASS |
| R06 | Repository name conflict is case-insensitive | 409 | 409 for case variant | PASS |
| R07 | Repository name with space rejected | 400 | 400 with form | PASS |
| R08 | Reserved repository name new rejected | 400 | 400 with form | PASS |
| R09 | Repository name empty | 400 | 400 | PASS |
| R10 | Repository name dot rejected | 400 | 400 | PASS |
| R11 | Repository name dotdot rejected | 400 | 400 | PASS |
| R12 | Repository name .git suffix rejected | 400 | 400 | PASS |
| R13 | Repository name 100 chars accepted | 303 | 303 | PASS |
| R14 | Repository name 101 chars rejected | 400 | 400 | PASS |
| R15 | Description 1000 chars accepted | 303 | 303 | PASS |
| R16 | Description 1001 chars rejected | 400 | 400 | PASS |
| R17 | Invalid visibility rejected | 400 | 400 | PASS |
| R18 | Missing CSRF on repository create | 403 | 403 | PASS |
| R19 | Wrong CSRF on repository create | 403 | 403 | PASS |
| R20 | Malformed idempotency key | 400 reload-the-page page | 400 reload message | PASS |
| R21 | Idempotent replay of repository create | both 303 to same location | replay 303 same location | PASS |
| R22 | Idempotency key reused with different body | 422 form page | 422 with form | PASS |
| R23 | NUL in repository path | 404 | 404 | PASS |
| R24 | NUL in new-repository path | 404 | 404 | PASS |
| R25 | New-repository page for unknown organization | 404 | 200, form rendered; POST returns 404 | **FAIL** |
| R26 | Read-only member opening new-repository form | 403 | 200, form rendered; POST returns 403 | **FAIL** |
| R27 | Read-only member submitting repository create | 403 form page | 403 | PASS |
| R28 | Repository create for unknown organization | 404 form page | 404 with form | PASS |
| R29 | Description HTML is escaped | no raw onerror attribute | description escaped | PASS |
| R30 | Non-member opening new-repository form | 404 | 200, form rendered for a private org | **FAIL** |
| R31 | CSRF token from another session on repository create | 403 | 403 for foreign CSRF token | PASS |

### Reviewer

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| V01 | Open PR shows reviewers section | Reviewers heading, seeded dave, request form | reviewers section with dave and form | PASS |
| V02 | Reviewer request form fields | label, required input, CSRF, idempotency | form controls present | PASS |
| V03 | Request reviewer carol through the page | 303 back to PR, carol listed | 303, carol on page and in API | PASS |
| V04 | Reviewer list is alphabetical | carol before dave | carol then dave | PASS |
| V05 | Remove reviewer carol through the page | 303, carol gone from page and API | 303, carol removed | PASS |
| V06 | Unknown reviewer username | 422 Cannot request that reviewer | 422 reviewer not found page | PASS |
| V07 | Non-collaborator reviewer | 422 reviewer cannot approve | 422 cannot approve | PASS |
| V08 | Author cannot request themselves | 422 author cannot review own PR | 422 self-review rejected | PASS |
| V09 | Missing CSRF on reviewer request | 403 | 403 | PASS |
| V10 | Wrong CSRF on reviewer removal | 403 | 403 | PASS |
| V11 | Remove a reviewer who is not requested | 404 Cannot remove that reviewer | 404 styled page | PASS |
| V12 | Draft PR hides reviewer form | no Reviewers heading or request form | no form on draft | PASS |
| V13 | Draft PR has no remove controls | no Remove reviewer button | no remove buttons | PASS |
| V14 | Read-only user is shown reviewer controls | form and Remove hidden | 200 with request form and Remove button; POST 403 | **FAIL** |
| V15 | Read-only user requesting a reviewer | 403 not permitted | 403 not permitted | PASS |
| V16 | Read-only user removing a reviewer | 403 not permitted | 403 not permitted | PASS |
| V17 | Duplicate reviewer request is idempotent | 303 and one dave entry | 303, single dave entry | PASS |
| V18 | Reviewer username HTML is not reflected | 422, no raw script in page | 422, username not reflected | PASS |
| V19 | Merged PR shows no reviewer request form | no form, no remove buttons | no reviewer controls on merged PR | PASS |
| V20 | Remove button names the reviewer | aria-label includes username | aria-label present | PASS |
| V21 | Closed PR hides reviewer controls | no form or remove button | no controls on closed PR | PASS |
| V22 | Reviewer request on a closed PR | 409 not open page | 409 pull request is not open | PASS |
| V23 | Maintainer can request a reviewer | 303 and reviewer listed | 303, carol listed | PASS |
| V24 | Foreign CSRF token on reviewer request | 403 | 403 for foreign CSRF token | PASS |

### Fork

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| F01 | Fork page names a readable parent | Forked from link for t01 | Forked from link present | PASS |
| F02 | Fork parent link text | org/repo label | org/repo link text | PASS |
| F03 | Unreadable parent is omitted | erin sees fork but no Forked from | no parent link or parent org name | PASS |
| F04 | Anonymous visitor on private fork | 303 to /login, no parent leak | 303 /login | PASS |
| F05 | Non-fork repository has no parent note | no Forked from on core | no parent note | PASS |

### Merge

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| M01 | Merged PR badge | Merged badge | Merged badge | PASS |
| M02 | Merged PR names merger and time | Merged by t01 at &lt;timestamp&gt; | Merged by t01 at 2026-11-14T09:39:34Z | PASS |
| M03 | Merged PR has no merge form | no merge action form | no merge form | PASS |
| M04 | Read-only user can read merged PR | 200 for erin | 200 with merged note | PASS |
| M05 | Merged-by shows a username, not an id | Merged by &lt;username&gt; at | Merged by t01 | PASS |
| M06 | Merged-by names a different merger | Merged by carol at | Merged by carol | PASS |

### Blocked

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| B01 | Blocked PR explains why to a merger | explanation, no merge form | Merging is blocked: the protection requirements (approvals, checks or code owners) are not met. | PASS |
| B02 | Blocked PR has no enabled merge button | no Merge pull request button | no merge button | PASS |
| B03 | Admin also sees the block explanation | dave sees explanation | explanation shown to admin | PASS |
| B04 | Read-only user sees no block explanation | erin: no explanation, no form | silent for read-only | PASS |
| B05 | Triage user sees no block explanation | frank: no explanation | silent for triage | PASS |
| B06 | Draft PR shows no block explanation | draft: no explanation, no form | silent for draft | PASS |
| B07 | Blocked merge API refuses | 409/403 from the merge endpoint | 403 not permitted | PASS |
| B08 | Change request explains the block | A change request blocks the merge. | change-request explanation | PASS |
| B09 | Unblocked PR shows the merge form | approved PR has merge form | merge form present | PASS |
| B10 | Maintainer also sees the block explanation | carol sees explanation | explanation shown to maintainer | PASS |
| B11 | Conflicting PR disables merge with a reason | conflict reason, disabled button | conflict reason and disabled button | PASS |

### Protection

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| P01 | Branch settings lists the rule | table row for main | main rule listed | PASS |
| P02 | Dismiss stale is on for main | row shows yes under Dismiss stale | main dismiss-stale=yes | PASS |
| P03 | Add-rule form has dismiss checkbox and textarea | textarea + checked dismiss box | textarea and checked dismiss checkbox | PASS |
| P04 | Add a rule with dismiss stale unchecked | 303, new row shows no | 303, rel-b6e660/* dismiss=no | PASS |
| P05 | Delete a rule from the settings page | 303 and row gone | 303, rule removed | PASS |
| P06 | Missing CSRF on add rule | 403 | 403 | PASS |
| P07 | Read-only user sees no rule controls | table but no Add a rule/Delete | read-only table only | PASS |
| P08 | Read-only user adding a rule | 403 | 403 | PASS |
| P09 | Invalid approvals value | 400 with error | 400 with error | PASS |
| P10 | NUL in branch settings path | 404 | 404 | PASS |
| P11 | Idempotent replay of add rule | both 303, one row | 303 twice, one row | PASS |
| P12 | Status-check textarea parses comma and newline | table shows three checks | checks parsed into ci/test, lint, unit | PASS |

### Regression

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| G01 | Organization page regression | 200, repos and create link | org page OK | PASS |
| G02 | Repository page regression | 200 with browse/issues/pulls/settings links | repo page links OK | PASS |
| G03 | Repository tree page | 200 and lists README.md | tree lists files | PASS |
| G04 | Blob page | 200 with file contents | blob renders content | PASS |
| G05 | Commit history page | 200 with commits | commits page OK | PASS |
| G06 | Create issue through the UI | 303 to the new issue | 303 /organizations/lab01/repositories/r4b-core-8340fd/issues/6 | PASS |
| G07 | Issue page renders title and body | 200 with title and body | issue page OK | PASS |
| G08 | Comment on an issue | 303 and comment appears | 303, comment shown | PASS |
| G09 | Pull request list page | 200 listing open feature | pulls list OK | PASS |
| G10 | Pull request page | 200 with description and mergeability | PR page OK | PASS |
| G11 | Issue list page | 200 listing the issue | issues list OK | PASS |
| G12 | Home page | 200 with organizations | home OK | PASS |
| G13 | Anonymous public repository page | 200 without session | public page 200 anonymous | PASS |
| G14 | Unknown repository page | 404 styled | 404 | PASS |
| G15 | Tag protection settings page | 200 | tags settings OK | PASS |
| G16 | Notifications page | 200 | notifications 200 | PASS |
| G17 | Sign out | 303 to /login and session cleared | 303 /login, session gone | PASS |
| G18 | Login page renders | 200 with username/password and CSRF | login form OK | PASS |
| G19 | NUL in pull request path | 404 | 404 | PASS |
| G20 | NUL in issue path | 404 | 404 | PASS |

## Defects

### D1 — Reviewer request/remove controls are rendered for users who cannot use them

Severity: **medium** (misleading affordance; no privilege bypass — the POST is
still refused).

The pull page shows the "Request a review from" form and a `Remove` button on
every reviewer to any viewer with `pull_request.comment`, which includes the
read and triage roles. Requesting or removing a reviewer also requires the
ability to approve (or to be the author), so for a non-author read/triage user
the rendered controls always fail with 403.

Repro (read grant, non-author):

```console
# sign in as erin, then open PR #1 in lab01/r4b-core-8340fd
$ curl -s -b erin.jar http://127.0.0.1:8080/organizations/lab01/repositories/r4b-core-8340fd/pulls/1 \
    | grep -c 'Request a review from'
1
$ curl -s -b erin.jar http://127.0.0.1:8080/organizations/lab01/repositories/r4b-core-8340fd/pulls/1 \
    | grep -c 'Remove reviewer dave'
1
# submitting either form with the page token and session cookie:
HTTP/1.1 403 Forbidden
<h1>Cannot request that reviewer</h1><p class="error" role="alert">not permitted</p>
```

Triage (`frank`) behaves the same. The organization page already hides the
"New repository" link from these roles (case O24), so this is inconsistent with
the project's own pattern.

Root cause: `crates/githearth-server/src/pulls_ui.rs:852` (remove button) and
`:876` (request form) gate on `abilities.may_comment`. The write path calls
`require_review_permission` (`crates/githearth-server/src/pulls.rs:2177-2195`),
which requires `PullRequestApprove` unless the actor is the author. A read/triage
non-author has comment but not approve. Gate the UI on "author or may approve"
instead.

### D2 — New-repository form is rendered without checking the organization exists or that the viewer may create there

Severity: **low–medium** (dead-end form; authorization is still enforced on
submit, so no repository can be created).

`/organizations/{slug}/repositories/new` returns 200 with a full form for:

1. a slug that does not exist;
2. a private organization the viewer is not a member of;
3. an organization where the viewer has read-only access.

Submitting the form then returns 404 (unknown org) or 403 (no permission), so
the page offers an action that cannot succeed. The organization page correctly
hides the "New repository" link (O24), so only direct navigation reaches this
state.

Repro:

```console
# erin (read) on a repository she can read
$ curl -s -o /dev/null -w '%{http_code}\n' -b erin.jar \
    http://127.0.0.1:8080/organizations/lab01/repositories/new
200                       # page contains "Create repository"
# POSTing a valid name with that page's CSRF token -> 403 not permitted

# t01 on an organization that does not exist
$ curl -s -o /dev/null -w '%{http_code}\n' -b t01.jar \
    http://127.0.0.1:8080/organizations/no-such-org-zzz/repositories/new
200                       # page contains the bogus slug in the breadcrumb
# POST -> 404 not found

# erin on a private organization she is not a member of
$ curl -s -o /dev/null -w '%{http_code}\n' -b erin.jar \
    http://127.0.0.1:8080/organizations/r4b-p-8340fd/repositories/new
200
```

Root cause: `crates/githearth-server/src/ui.rs:835-855` (`new_repository_page`)
only authenticates the session and validates the path parameter; it never looks
up the organization or authorizes `RepositoryCreate`. Compare the organization
page, which resolves facts before rendering the link
(`crates/githearth-server/src/ui.rs:625-640`). Resolve the organization, return
the styled 404 when it is missing or unreadable, and render the form only when
`repository.create` is allowed (403/404 otherwise).

## Notes

- **Everything on the round-4 checklist that was exercised through the browser
  works**: create org/repo through forms with CSRF, idempotency and validation;
  duplicate org/repo returns a 409 form page with the values preserved; `new`
  is rejected as both a slug and a repository name; NUL paths are styled 404s;
  reviewer add/remove works for owner, maintainer and author, with unknown,
  non-collaborator and self-reviewer rejected as styled 422 pages; drafts and
  closed/merged PRs hide the reviewer form; the fork note appears only when the
  parent is readable; merged PRs show "Merged by <user> at <time>"; blocked
  merges explain why to owner/admin/maintainer and stay silent for read, triage
  and drafts; branch settings show the dismiss-stale column and a status-check
  textarea, and parse comma/newline-separated checks.
- R25, R26 and R30 are three manifestations of D2, counted separately because
  they exercise different access paths (unknown org, read-only member,
  non-member of a private org).
- Idempotent replay was checked both at the form level and against PostgreSQL:
  replaying the same repository-create and branch-rule keys returned 303 twice
  and left exactly one row; reusing a key with a different body returned 422
  with the form re-rendered.
- Foreign-session CSRF tokens are rejected (403) on org create, repo create and
  reviewer request; missing and wrong tokens are also 403.
- Reviewer error pages are 422/404 with the styled page and a "Back to the pull
  request" link; reviewer usernames are not reflected into the error page, so
  the `<script>` probe produced only "reviewer not found".
- One non-reproducible observation: a merged PR page returned 404 once
  immediately after the merge completed, then 200 on every later read. A
  deliberate race test (merge, then 40 page reads at 50 ms intervals) returned
  40 × 200, so it is recorded as an observation, not a filed defect.
- Exact-path routing returns 404 for trailing slashes
  (`/organizations/new/`); this is the existing route shape, not a new regression.
- No source, migration or fixture outside the `r4b-*`/`lab01` test data was
  modified, and no GitHub issue was posted.

## Regression smoke

All 20 smoke cases passed: organization page, repository page (browse/history/
issues/pulls/settings links), tree, blob, commit history, create issue through
the UI, issue detail and comment, pull list and pull detail, issue list, home,
anonymous public repository, unknown repository 404, NUL PR/issue paths, tag
settings, notifications, sign-out and the login page.
