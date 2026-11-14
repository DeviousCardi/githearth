# 04 — New-user journey (round 4 final verification)

Date: 2026-11-14. Instance: `main` `cd7946b`, HTTP <http://127.0.0.1:8080>,
SSH `127.0.0.1:2222`. Account `t04` / `<test-password>`.

## Verdict

**PASS.** The whole new-user journey works end to end through the browser:
sign-in → organization and repository creation → HTTP/SSH push and clone →
browse → issue + comment → pull request → reviewer request from the PR page →
approval as `carol` → check → merge with merged-by info and notification →
branch protection from the settings page → fork → cross-repo PR from the form →
merge → audit verification. The UI and the API agree on every object I
compared. Two disclosure gaps remain (clone URL and cross-repo source
repository), and one functional gap blocks collaboration in a *newly created*
organization. None of them is a regression in the round-4 changes.

## Counts

| | |
|---|---|
| Checks run | 118 |
| Pass | 115 |
| Fail | 2 (D1, D2 — both UI disclosure, non-blocking) |
| Functional gap | 1 (G1 — no way to add an org member) |
| Failures re-run | all; the rest were test-expectation errors and passed on re-run |
| Journey steps still needing the API | 2 write paths + audit read (listed below) |

## Setup

`lab04` already existed with `carol` as a member, so the collaborative journey
ran in `lab04/journey-r4` (private, created from the UI). A second organization
`lab04-r4` was created from the UI to exercise the new organization-creation
path; it is single-member because of G1.

Repository state after the journey: 1 issue (#1), 5 pull requests (#2–#6),
1 fork (`lab04/journey-r4-fork`), 1 branch-protection rule on `main`,
audit chain 403 rows, verify `ok: true`.

## Results

### Authentication, organizations, repositories

| # | Case | Result |
|---|---|---|
| 1 | Anonymous `/login` renders (200) | PASS |
| 2 | Login form has CSRF, username, password | PASS |
| 3 | Wrong password rejected (`?error=1`) | PASS |
| 4 | `t04` signs in | PASS |
| 5 | Home lists the account's organizations | PASS |
| 6 | `/organizations/new` renders | PASS |
| 7 | Reserved slug `new` rejected (stays on form) | PASS |
| 8 | Invalid slug `Bad Slug!` rejected | PASS |
| 9 | Create organization from the UI (`lab04-r4`) | PASS |
| 10 | Organization page shows name and "New repository" | PASS |
| 11 | `/organizations/{slug}/repositories/new` renders | PASS |
| 12 | Reserved repository name `new` rejected | PASS |
| 13 | Invalid repository name rejected | PASS |
| 14 | Create private repository from the UI | PASS |
| 15 | Repository page exposes HTTP **and** SSH clone URLs | **FAIL — D1** |
| 16 | Empty repository shows push/clone instructions | PASS |
| 17 | `/settings` renders tokens + SSH keys | PASS |
| 18 | Add SSH key from the settings form | PASS |
| 19 | Create repository in an existing org (`lab04`) from the UI | PASS |

### Git transport and browsing

| # | Case | Result |
|---|---|---|
| 20 | Push `main` over HTTP with account password (Basic auth) | PASS |
| 21 | Clone over HTTP | PASS |
| 22 | Clone over SSH (`ssh://git@127.0.0.1:2222/lab04/journey-r4.git`) | PASS |
| 23 | Push a branch over SSH | PASS |
| 24 | Push `feature` over HTTP | PASS |
| 25 | Populated repository page shows Browse/Commits/Compare | PASS |
| 26 | Tree root lists `README.md` and `src/` | PASS |
| 27 | Nested tree lists `src/app.txt` | PASS |
| 28 | Blob page renders file content | PASS |
| 29 | Commits page lists commits | PASS |
| 30 | Commit page shows SHA, author, message and diff | PASS |
| 31 | `compare/main...feature` shows 1 ahead and the diff | PASS |
| 32 | `/compare?base=main&head=feature` redirects to canonical path | PASS |
| 33 | Bare `/compare` returns 404 (redirect-only endpoint) | PASS (note) |

### Issues

| # | Case | Result |
|---|---|---|
| 34 | Issues list renders empty state | PASS |
| 35 | New-issue form renders | PASS |
| 36 | Create issue from the UI | PASS |
| 37 | Comment on issue from the UI | PASS |
| 38 | Edit form renders with current values | PASS |
| 39 | Edit saves title and body | PASS |
| 40 | Close issue from the UI | PASS |
| 41 | Reopen issue from the UI | PASS |
| 42 | Issues list shows the issue | PASS |
| 43 | Whitespace-only title rejected with a message | PASS |

### Pull request, reviewer, check, merge

| # | Case | Result |
|---|---|---|
| 44 | Pull list renders empty state | PASS |
| 45 | New-PR form renders with source repository selector | PASS |
| 46 | Create PR `feature` → `main` from the UI | PASS |
| 47 | PR page shows state, approvals, files changed | PASS |
| 48 | Reviewer request form present | PASS |
| 49 | Request `carol` as reviewer from the PR page | PASS |
| 50 | Remove reviewer from the PR page | PASS |
| 51 | `carol` approves from the UI | PASS |
| 52 | PR page shows the approval | PASS |
| 53 | API pull read agrees with the UI (open, branches) | PASS |
| 54 | Post `ci/build` success on the head commit (API) | PASS |
| 55 | PR page shows the check as success | PASS |
| 56 | Merge form available to the maintainer | PASS |
| 57 | Merge succeeds (merge worker, ~2 s) | PASS |
| 58 | Merged page shows "Merged by t04 at …" | PASS |
| 59 | API pull shows `state=merged` and `merged_by` | PASS |
| 60 | Notifications page renders | PASS |
| 61 | `carol` receives review-request notifications | PASS |
| 62 | Mark a notification read | PASS |
| 63 | Self-approval on own PR refused | PASS |
| 64 | Reviewer without repository access refused with a message | PASS |
| 65 | Unknown reviewer refused with a message | PASS |
| 66 | Duplicate reviewer request is idempotent (one row) | PASS |
| 67 | Merge by a stranger returns 404 (state not leaked) | PASS |
| 68 | PR read by a stranger returns 404 | PASS |

### Branch protection

| # | Case | Result |
|---|---|---|
| 69 | Settings page opens from the repository page | PASS |
| 70 | Blank pattern rejected | PASS |
| 71 | Reserved status context (`githearth/legacy-require-checks`) rejected | PASS |
| 72 | Duplicate status contexts rejected | PASS |
| 73 | Create rule (main, 1 approval, ci/build, require PR) | PASS |
| 74 | Protection API agrees with the UI row | PASS |
| 75 | Delete rule from the settings page | PASS |
| 76 | Second PR targets protected `main` | PASS |
| 77 | Blocked merge explained: "Merging is blocked: the protection requirements …" | PASS |
| 78 | Merge form withheld while blocked | PASS |
| 79 | `carol` approves the protected PR | PASS |
| 80 | Required check posted; merge form appears | PASS |
| 81 | Protected PR merges once requirements are met | PASS |
| 82 | Merged-by shown after the worker runs | PASS |

### Fork and cross-repository PR

| # | Case | Result |
|---|---|---|
| 83 | Fork from the repository page UI (`lab04/journey-r4-fork`) | PASS |
| 84 | Fork page shows "Forked from lab04/journey-r4" | PASS |
| 85 | Fork parent link opens the parent repository | PASS |
| 86 | Fork API exposes `parent_repository_id` | PASS |
| 87 | Push a branch to the fork over HTTP | PASS |
| 88 | New-PR form on the parent lists the fork as a source | PASS |
| 89 | Open cross-repo PR from the form | PASS |
| 90 | Cross-repo PR page names the source repository | **FAIL — D2** |
| 91 | `carol` approves the cross-repo PR | PASS |
| 92 | Required check posted for the cross-repo PR | PASS |
| 93 | Cross-repo PR merges | PASS |

### Notifications, audit, API consistency and boundaries

| # | Case | Result |
|---|---|---|
| 94 | `carol` opens her own PR from the UI | PASS |
| 95 | `t04` approves `carol`'s PR | PASS |
| 96 | `t04` merges `carol`'s PR | PASS |
| 97 | `carol` (author) receives a merged notification | PASS |
| 98 | `carol`'s PR shows "Merged by t04" | PASS |
| 99 | Unknown `/api` path returns JSON 404 | PASS |
| 100 | Malformed path id returns JSON 400 | PASS |
| 101 | Reserved org slug rejected by the API as JSON 400 | PASS |
| 102 | Reserved repo name rejected by the API as JSON 400 | PASS |
| 103 | NUL in path returns 404 | PASS |
| 104 | UI and API issue lists agree | PASS |
| 105 | UI and API pull lists agree | PASS |
| 106 | Create read-only token from the settings UI | PASS |
| 107 | Read-only token can read | PASS |
| 108 | Read-only token cannot write (403 `read-only token`) | PASS |
| 109 | Revoke token from the settings UI | PASS |
| 110 | Tag protection settings page renders | PASS |
| 111 | Duplicate fork name refused ("repository name already taken") | PASS |
| 112 | Stranger cannot read a new org's repository (404) | PASS |
| 113 | New org offers no member management | **GAP — G1** |
| 114 | Audit list readable (`?limit=200&after=300`) | PASS |
| 115 | Audit verify: `{"rows":403,"ok":true,"first_bad_seq":null}` | PASS |
| 116 | Audit includes merged, forked, protection, grant and check events | PASS |
| 117 | Author sees the unread review notification | PASS |
| 118 | Mark all read clears the unread count | PASS |

## Defects

### D1 — Repository page never shows the SSH clone URL and drops the clone URL once the repository has commits

- **Severity:** low–medium (discoverability; no data or authorization impact).
- **Repro:**
  1. Create a repository from the UI. The page says
     `This repository is empty. Clone it with: git clone http://127.0.0.1:8080/lab04/journey-r4.git`.
  2. Push any commit, reload the repository page.
  3. No `git clone` line is rendered at all, and `ssh://` never appears on any
     repository page even though SSH is enabled.
- **Evidence:** populated page text contains neither `git clone` nor `ssh://`.
- **Root cause:** the only clone URL render is the empty-state branch,
  `crates/githearth-server/src/ui.rs:1026` (helper at `ui.rs:662`); there is no
  `ssh://` string anywhere in the server sources.
- **Impact:** a new user loses the only UI-provided clone command after the
  first push, and cannot discover the SSH remote from the product (it only
  appears in `README.md` / `docs/foundation.md`).

### D2 — Cross-repository pull request page does not name the source repository

- **Severity:** low–medium (ambiguity for fork PRs).
- **Repro:**
  1. Fork `lab04/journey-r4` to `lab04/journey-r4-fork`.
  2. Push branch `from-fork` to the fork.
  3. On the parent, open a PR with source repository `lab04/journey-r4-fork`,
     branch `from-fork` → `main`.
  4. The PR header reads `Merged · from-fork → main · by t04 …`; the fork is
     never named.
- **Evidence:** page text does not contain `journey-r4-fork`; the API object
  does expose `source_repository_id`.
- **Root cause:** the header renders only the two branch names,
  `crates/githearth-server/src/pulls_ui.rs:1317`.
- **Impact:** when a branch name exists in both the base repository and a fork,
  the page cannot tell the reviewer which repository the commits come from.

## Functional gap

### G1 — A newly created organization cannot gain a second member (no UI, no API)

- **Repro:** create `lab04-r4` from `/organizations/new`; open
  `/organizations/lab04-r4`. There is no member list, invite form or members
  route. The only way to grant a repository role is
  `POST /api/v1/organizations/{org}/repositories/{name}/access`, and that call
  is refused with `{"error":"invalid grant"}` because the grant trigger requires
  the subject to already be a member
  (`crates/githearth-server/migrations/0004_repository_grants.sql:38-45`).
- **Impact:** a brand-new organization is permanently single-member, so the
  reviewer/approval/merge flow cannot be exercised there. That is why the
  collaborative journey ran in the pre-existing `lab04`, where `carol` is
  already a member.
- **Not a round-4 regression** (membership management does not exist in this
  build), but it is the largest remaining obstacle to a complete new-user
  journey.

## Steps that still require the API

All creation, browsing, review, merge, protection, fork and cross-repo PR steps
were done in the browser. Only these needed the API:

1. **Grant a repository role** (no UI exists):
   `POST http://127.0.0.1:8080/api/v1/organizations/lab04/repositories/journey-r4/access`
   body `{"subject_kind":"account","subject_id":"<carol-uuid>","role":"read"}`
   (idempotency key required).
2. **Post a commit status / satisfy a required check** (no UI exists):
   `POST http://127.0.0.1:8080/api/v1/organizations/lab04/repositories/journey-r4/commits/<sha>/statuses`
   body `{"context":"ci/build","state":"success","description":"…"}`.
3. **Read the audit log** (no UI exists):
   `GET http://127.0.0.1:8080/api/v1/organizations/lab04/audit?limit=200&after=300`
   and `GET http://127.0.0.1:8080/api/v1/organizations/lab04/audit/verify`.
   Both are read-only; the journey itself did not need them.

Organization membership has **no** endpoint at all (G1), so it is not listed as
an API step.

## UI / API agreement

Verified equal after the journey:

- Pull #2/#3/#5/#6: UI `Merged` and "Merged by t04 at …" ↔ API
  `state=merged`, `merged_by`, `merged_at`.
- Protection rule: UI row `main | no | no | yes | 1 | ci/build` ↔ API
  `pattern=main`, `required_approvals=1`, `required_status_checks=["ci/build"]`,
  `require_pull_request=true`.
- Fork: UI "Forked from lab04/journey-r4" ↔ API `parent_repository_id`.
- Issues and pulls: UI lists ↔ API `items` (1 issue, 5 PRs).
- Audit: 403 rows, `ok=true`, and every journey event kind present.

## Notes / expectation corrections (not defects)

- `new` as a slug or repository name is rejected, but both the UI and API answer
  the generic `invalid input` rather than naming the reservation. Consistent,
  just terse.
- Pull request numbers share the issue counter, so the first PR in a repository
  with one issue is `#2` (matches GitHub semantics).
- Merges are asynchronous: the UI redirects while the intent shows `Merge:
  Queued`, and the worker completes in ~1–3 s. A page reload shows the final
  state; the API returns `201` for the merge request, not the merge.
- `dismiss_stale_approvals` defaults to `true` for a new protection rule in both
  the API and the UI (asserted by `tests/protection.rs`), so the pre-checked box
  is intended.
- Bare `/compare` is a redirect-only endpoint and 404s without `base`/`head`;
  no UI link points at it.
- Merged notifications go only to the author, and not when the author merges
  their own PR; reviewers are not notified of a merge. Verified with a
  `carol`-authored PR merged by `t04`.
- Duplicate reviewer requests are idempotent (200, single row), not a 409.
- Screenshots for each stage are in `/tmp/ghm4r4/shots/`.
