# Pull requests, reviews, inline threads, suggestions, drafts and code owners — M4 lab acceptance report

Tester: t08. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454, Docker Compose).
Verdict: **FAIL** (one blocker makes every normal pull-request detail read fail).
Cases: 166 run, 155 pass, 4 fail, 7 notes (documented/by-design behaviour; nothing skipped silently).


## Create

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Create PR with valid title/body/branches | 201; head_id, base_id, state, draft, approvals, mergeability present | 201; all fields present | PASS |
| 2 | Whitespace-only title | 400 | 400 invalid input | PASS |
| 3 | Title length boundary (256 / 257 chars) | 201 / 400 | 201 / 400 | PASS |
| 4 | Body length boundary (65536 / 65537 chars) | 201 / 400 | 201 / 400 | PASS |
| 5 | Same source and target branch | 400 | 400 | PASS |
| 6 | Unknown source branch | 422 branch not found | 422 branch not found | PASS |
| 7 | Unknown target branch | 422 branch not found | 422 branch not found | PASS |
| 8 | Branch name naming refs/heads/... | 400 | 400 | PASS |
| 9 | Duplicate open PR for the same branch pair | 409 | 409 an open pull request already exists | PASS |
| 10 | draft=true on create | 201 with draft=true | 201, draft=true | PASS |
| 11 | Read grant (erin) creates a PR | campaign expectation: 403 | 201 (allowed) | NOTE: intended by ADR 0008, which puts pull_request.create in the read group for fork contributions |
| 12 | Write grant (carol) creates a PR | 201 | 201 | PASS |
| 13 | Unknown field in create body | 400 | 400 | PASS |
| 14 | Unknown repository | 404 | 404 | PASS |
| 15 | Title is not a string | 400 | 400 | PASS |

## Read, list and detail fields

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 16 | Read PR detail, live head (PR #11) | 200 with head/base/approvals/mergeability | 500 storage operation failed | FAIL: blocker D1 |
| 17 | Read PR detail, up-to-date head (PR #21) | 200, all documented fields | 200, all fields incl. mergeability and code-owner fields | PASS |
| 18 | Detail mergeability methods | merge, squash and rebase reported | all three: up_to_date | PASS |
| 19 | Unknown PR number | 404 | 404 pull request not found | PASS |
| 20 | Private repository, anonymous API read | 404 (campaign expectation) | 401 invalid or expired credentials | NOTE: global bearer-auth boundary; no repository data is disclosed |
| 21 | List item fields | id, number, title, body, state, draft, author, branches, timestamps | all present | PASS |
| 22 | state=closed filter | only closed | 6 closed | PASS |
| 23 | state=merged filter | only merged | 1 merged | PASS |
| 24 | Default/open filter | only open | 3 open | PASS |
| 25 | Invalid state value | 400 | 400 | PASS |
| 26 | List limit bounds (0 / 101) | 400 / 400 | 400 / 400 | PASS |
| 27 | Pagination limit/after | descending, no overlap | page 1 [10,9], next=9, page 2 [8,7] | PASS |
| 28 | List `after` is not an integer | 400 JSON error envelope | 400 plain text deserialization message (defect D2) | PASS: status correct, envelope inconsistent |
| 29 | List readable by erin, frank, dave, carol | 200 for each role | 200 for all four | PASS |
| 30 | Comments and reviews for an unknown PR | 404 / 404 | 404 / 404 | PASS |

## Drafts

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 31 | Merge blocked while draft | 409 pull request is a draft | 409 pull request is a draft | PASS |
| 32 | Request reviewer on a draft | 409 pull request is a draft | 409 | PASS |
| 33 | Mark draft ready | 200, draft=false | 200, draft=false | PASS |
| 34 | Ready state reflected in list | draft=false | draft=false | PASS |
| 35 | Mark ready is idempotent | 200, draft=false, no duplicate action | 200, draft=false | PASS |
| 36 | Convert back to draft | 200, draft=true | 200, draft=true | PASS |
| 37 | Merge blocked after convert-to-draft | 409 | 409 | PASS |
| 38 | Read-only reviewer toggles draft | 403 | 403 not permitted | PASS |
| 39 | Write non-author marks ready | 200 | 200 | PASS |
| 40 | PATCH title on a draft | 200 | 200 | PASS |
| 41 | Toggle draft on a closed PR | 409 | 409 pull request is not open | PASS |

## Reviews

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 42 | Add review request, then list it | 200, carol listed | 200, [carol] | PASS |
| 43 | Remove review request | 204 | 204 | PASS |
| 44 | Remove missing review request | 404 | 404 | PASS |
| 45 | Request unknown reviewer | 422 reviewer not found | 422 | PASS |
| 46 | Request non-collaborator (grace) | 422 reviewer cannot approve | 422 | PASS |
| 47 | Request the PR author as reviewer | 422 | 422 the author cannot review their own pull request | PASS |
| 48 | Duplicate review request | 200, one entry | 200, one carol entry | PASS |
| 49 | Remove review request by read grant (erin) | 403 | 403 | PASS |
| 50 | Carol approves | 201 approved | 201 approved | PASS |
| 51 | Self-approval forbidden | 403 | 403 not permitted | PASS |
| 52 | Read grant (erin) approves | 403 | 403 not permitted | PASS |
| 53 | Comment-only review without body | 400 | 400 invalid input | PASS |
| 54 | Comment-only review with body | 201 | 201 | PASS |
| 55 | Invalid verdict | 400 | 400 | PASS |
| 56 | Review body length boundary (65536 / 65537) | 201 / 400 | 201 / 400 | PASS |
| 57 | Review at a commit that is not the head | 409 | 409 commit is not the pull request head | PASS |
| 58 | Review list is append-only and ordered | 200, every verdict plus commit_id | 200, five verdicts in order | PASS |
| 59 | Review on a closed PR | 409 not open | 409 (rerun; first attempt used the author, correctly 403 self-approval) | PASS |
| 60 | Review on a merged PR | 409 | 409 | PASS |
| 61 | Latest verdict per reviewer is kept | 200, both verdicts appended | changes_requested then approved both present | PASS |
| 62 | Latest verdict wins at the merge gate | 201 | 201 | PASS |
| 63 | Merged PR reaches merged state | merged | merged | PASS |
| 64 | Approval counts only at the current head | 403 after pushing a new commit | 403 not permitted | PASS |
| 65 | Re-approval at the new head unlocks merge | 201 | 201 | PASS |
| 66 | dismiss_stale_approvals=false counts a stale approval | 201 | 201 | PASS |
| 67 | #136 detail approvals match the merge gate (dismiss=false) | approvals=1 and gate accepted | detail approvals=1 after head move; gate accepted 201 | PASS |
| 68 | Approvals at current head shown on a merged detail | approvals=1 | approvals=1 | PASS |

## Inline review comments

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 69 | Create a general comment | 201 | 201 | PASS |
| 70 | Create an anchored comment with path/line/side | 201 with commit_id, path, line, side | 201; all anchor fields present | PASS |
| 71 | Line 0 | 400 | 400 invalid input | PASS |
| 72 | Line past EOF | 400 line out of range | 400 | PASS |
| 73 | Unknown path | 400 path not found at commit | 400 | PASS |
| 74 | Invalid side | 400 | 400 | PASS |
| 75 | Old-side anchor | 201 | 201 | PASS |
| 76 | Path without commit anchor | 400 | 400 | PASS |
| 77 | Invalid commit id format | 400 | 400 | PASS |
| 78 | Anchor commit is not the head | 409 | 409 | PASS |
| 79 | Reply to a root | 201, in_reply_to=root | 201 | PASS |
| 80 | Reply to a reply | 422 replies attach to the thread root | 422 | PASS |
| 81 | Reply to an unknown comment | 400 | 400 invalid input | PASS |
| 82 | Resolve a thread | 200, resolved=true | 200 | PASS |
| 83 | Reply inherits root resolution | reply resolved=true | resolved=true | PASS |
| 84 | Reply to a resolved thread | allowed (document) | 201 | NOTE: replies remain open after resolution |
| 85 | Unresolve a thread | 200, resolved=false | 200 | PASS |
| 86 | Read grant (erin) resolves a thread | 403 | 403 | PASS |
| 87 | Resolve an unknown comment | 404 | 404 comment not found | PASS |
| 88 | Comment body boundary (whitespace / 65537) | 400 / 400 | 400 / 400 | PASS |
| 89 | Comment list pagination | 200, limit and next | 200, 2 items, next=2 | PASS |
| 90 | Comment on a closed PR | document | 201 (allowed) | NOTE: create_comment has no open-state check; GitHub also allows comments on closed PRs |
| 91 | Resolve a thread on a closed PR | 409 | 409 | PASS |
| 92 | Anchored comments become outdated after a push | all anchored outdated=true | 2/2 true | PASS |
| 93 | General comments are never outdated | outdated=false | false | PASS |

## Suggestions

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 94 | Create a suggestion comment | 201, suggestion="BETA\n" | 201, suggestion="BETA\n" | PASS |
| 95 | Apply suggestion as read grant (erin) | 403 | 403 not permitted | PASS |
| 96 | Apply suggestion | 200 with commit_id | 200 with new commit id | PASS |
| 97 | Applied suggestion changes branch content | line 2 becomes BETA | blob text 'alpha\nBETA\ngamma\n' | PASS |
| 98 | Re-apply an outdated suggestion | 409/422 refused | 409 commit is not the pull request head | PASS |
| 99 | Apply a comment with no suggestion | 422 | 422 comment is not anchored to a line | PASS |
| 100 | Apply an unanchored suggestion block | 422 | 422 comment is not anchored to a line | PASS |
| 101 | Old-side comment exposes no suggestion | suggestion=null | null | PASS |
| 102 | Apply an old-side suggestion | 422 | 422 a suggestion cannot apply to the base side | PASS |
| 103 | Apply after the head moved | 409 | 409 | PASS |
| 104 | Apply on a closed PR | 409 | 409 pull request is not open | PASS |
| 105 | Writer non-author applies a suggestion | 200 | 200 | PASS |

## Code owners

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 106 | CODEOWNERS added by push | file readable at target tip | * @carol on gated | PASS |
| 107 | Non-owner approval alone does not satisfy the gate | 403 | 403 not permitted | PASS |
| 108 | Matching owner approval satisfies the gate | 201 | 201 | PASS |
| 109 | Code-owner-only PR merges after owner approval | 201 | 201 | PASS |
| 110 | Author-as-sole-owner relaxation (#137) | 201 without a review | 201; CODEOWNERS "* @t08", author t08 | PASS |
| 111 | Invalid CODEOWNERS errors surfaced | 200 detail, non-empty code_owner_errors | 200; ["malformed owner token 'carol'"] | PASS |
| 112 | Malformed file with no parseable owners at the gate | document | require_code_owner_review passes (no owners) | NOTE: parse errors are surfaced in detail but do not make the gate fail closed |
| 113 | Create/PATCH response code-owner flag | false or computed, matching the merge gate | true with empty required_code_owners while the merge gate returns 403 | FAIL: defect D3 |

## Cross-repository pull requests

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 114 | Fork API copies refs and records parent | 201 + parent id + public visibility | 201 create; GET 200 with parent id; fork main = parent main | PASS |
| 115 | Create cross-repo PR | 201, head_id from fork tip | 201, head matches fork tip | PASS |
| 116 | Read live-source cross-repo PR detail | 200 | 500 storage operation failed | FAIL: blocker D1 |
| 117 | Apply suggestion on cross-repo PR | 422 not supported | 422 | PASS |
| 118 | Review at the fork head | 201 | 201 | PASS |
| 119 | Merge live cross-repo PR | 201 | 201 | PASS |
| 120 | Merged fork objects fetched into parent | contrib.txt present on parent main | present | PASS |
| 121 | Source repository is not a fork | 400 | 400 | PASS |
| 122 | Unknown source repository | 404 | 404 | PASS |
| 123 | Malformed source spec | 400 | 400 | PASS |
| 124 | Cross-repo PR with identical branch names | 201 | 201 | PASS |
| 125 | Delete fork via API | 204 | 204 | PASS |
| 126 | Deleted-fork PR detail (#134) | 200, closed, no 500 | 200, closed, closed_reason=source repository deleted | PASS |
| 127 | Deleted-fork PR head id | null | null | PASS |
| 128 | Merged cross-repo PR still reads after fork deletion | 200, merged | 200, merged | PASS |
| 129 | Reopen a system-closed deleted-fork PR | document | 200, reopened with head_id=null and closed_reason cleared | NOTE: allowed; leaves an open, unmergeable PR |

## Merge, PATCH and idempotency

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 130 | Read grant (erin) cannot merge | 403 | 403 not permitted | PASS |
| 131 | Merge with a stale expected head | 409 | 409 commit is not the pull request head | PASS |
| 132 | Merge with an invalid method | 400 | 400 | PASS |
| 133 | Merge an unknown PR | 404 | 404 merge intent not found | PASS |
| 134 | Merge a closed PR | 409 | 409 pull request is not open | PASS |
| 135 | Author merges own PR | 201 | 201 | PASS |
| 136 | Squash merge reaches merged state | merged | merged | PASS |
| 137 | PATCH title on a merged PR | 200 | 200 | PASS |
| 138 | Reopen a merged PR | 409 merged | 409 pull request is merged | PASS |
| 139 | Reopen a closed PR | 200, open | 200 | PASS |
| 140 | Close an open PR | 200, closed | 200 | PASS |
| 141 | PATCH whitespace title / oversized body / invalid state / empty body | 400 each | 400 each | PASS |
| 142 | Read grant cannot PATCH a PR | 403 | 403 | PASS |
| 143 | Write grant can PATCH a PR | 200 | 200 | PASS |
| 144 | Merge into main | 201 | 201 | PASS |
| 145 | Target branch advances after merge | merged file present on main | pr-create.txt present | PASS |
| 146 | Create PR idempotent replay | 201 twice, same number | 201/201, both #23 | PASS |
| 147 | Idempotency key reused with a different body | 422 documented KEY_REUSED | 422 | PASS |
| 148 | Comment idempotent replay | 201 twice, same comment id | same id | PASS |

## Web UI

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 149 | PR list page | 200 with rows and draft badge | 200 | PASS |
| 150 | PR detail page, up-to-date PR | 200 with merge box, review form | 200; merge box and review form present | PASS |
| 151 | PR detail page, live head | 200 | 503 Unavailable page | FAIL: blocker D1 |
| 152 | Draft badge on detail | Draft badge text | present | PASS |
| 153 | Merge box up-to-date guard | mentions already up to date | present | PASS |
| 154 | Inline comment anchor rendered | comment id anchor and app.txt line 2 | present | PASS |
| 155 | Resolve button rendered for the author | resolve form present | present | PASS |
| 156 | Unresolve control after resolve | unresolve form present | present | PASS |
| 157 | Draft badge removed when ready | no Draft badge | absent | PASS |
| 158 | Comment form with bad/missing CSRF | 403 / 403 | 403 / 403 | PASS |
| 159 | Comment form with valid CSRF | stored comment | stored (200 after redirect) | PASS |
| 160 | Review form with valid CSRF | stored review | stored | PASS |
| 161 | Resolve form POST | thread resolved | resolved=true | PASS |
| 162 | HTML escaping in PR list | no raw script tag | escaped, no raw | PASS |
| 163 | HTML escaping in PR detail | title and body escaped | escaped on a deleted-fork (200) detail page | PASS |
| 164 | Deleted-fork PR page closed state | closed rendered | closed rendered | PASS |
| 165 | Anonymous private-repo PR page | no repository data | sign-in page, no PR data | PASS |
| 166 | Anonymous public-repo PR list | 200 or sign-in | sign-in page (UI requires a session) | NOTE: public repos are not browsable anonymously in the web UI |


## Defects

The four FAIL rows above come from two root causes: three are D1, one is D3.

### D1 (blocker) — every normal pull-request detail read returns 500 (API) or 503 (UI): rebase mergeability uses a Git option the deployed Git does not support

- **Severity: blocker**
- **Repro** (as `t08`, bearer session token):
  ```bash
  # an ordinary open PR whose head is ahead of its target
  curl -s -H "Authorization: Bearer $TOK" \
    http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls/11
  # -> 500 {"error":"storage operation failed"}

  # the campaign fixture repo, proving it is not specific to lab08
  curl -s -H "Authorization: Bearer $TOK" \
    http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/pulls/1
  # -> 500 {"error":"storage operation failed"}

  # the browser page for the same PR
  # GET /organizations/lab08/repositories/t08-pulls/pulls/11 -> 503 "Unavailable"

  # a rebase merge is refused the same way, while a merge/squash intent succeeds
  curl -s -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
    -d '{"method":"rebase","expected_head_oid":"<head>"}' \
    http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls/2/merge
  # -> 500 {"error":"storage operation failed"}
  ```
- **Expected:** `200` with `head_id`, `base_id`, `approvals`, `changes_requested`, `statuses`, `mergeability` (merge/squash/rebase), `required_code_owners`, `code_owner_review_satisfied` and `code_owner_errors`. The rebase check should report a status (`clean`/`conflicts`/`unchecked`), not fail the whole request.
- **Actual:** the detail read fails because the rebase mergeability probe runs
  `git merge-tree --write-tree ... --merge-base=<oid>`, which Git 2.39.5 rejects:
  ```
  error: unknown option `merge-base=<oid>'
  usage: git merge-tree [--write-tree] [<options>] <branch1> <branch2>
  ```
  `mergeability::for_pull` always computes all three methods (including `rebase`)
  (`crates/githearth-server/src/mergeability.rs:207-218`), and `read_pull` calls it for
  every PR with resolvable tips (`crates/githearth-server/src/pulls.rs:889-901`), so
  the entire read returns 500. The browser maps the same failure to its 503
  "Unavailable" page.
- **Evidence:**
  - `docker exec githearth-app-1 git --version` -> `git version 2.39.5`;
    `Dockerfile:18` installs Debian bookworm's `git` in the runtime image.
  - Direct container command reproduces `unknown option 'merge-base=...'`.
  - App log on each failure:
    `{"level":"ERROR","fields":{"message":"repository read failed","category":"git","repository_id":"..."}}`.
  - Offending arg: `crates/githearth-git/src/merge.rs:285` (`--merge-base={base}`).
  - The version gate accepts 2.38 (`crates/githearth-git/src/layout.rs`, `parse_version`),
    and `docs/decisions/0004-merge-execution.md:61` says "Git >= 2.38" while claiming
    2.43 is in use; the deployed 2.39.5 satisfies the gate but not this command.
- **Scope: systematic.** Any PR whose head is not an ancestor of its target (i.e. any
  normal PR with changes) fails to read. I confirmed the same 500 on `acme/web` PR #1.
  PRs with an up-to-date head (PR #21, #24) and PRs whose source branch/fork is gone
  (deleted-fork PR #2 in `t08-xrepo`) return 200, which isolates the failure to the
  rebase branch of mergeability. After a PR merges, its head becomes an ancestor and
  its detail starts working. The failure also hides approvals, `required_code_owners`
  and `code_owner_errors` from clients and from the browser.

### D2 (minor) — invalid `after` cursor on the PR list returns a plain-text 400 instead of the JSON error envelope

- **Severity: minor**
- **Repro:**
  ```bash
  curl -s -H "Authorization: Bearer $TOK" \
    'http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls?after=abc'
  ```
- **Expected:** `400` with the usual JSON body, e.g. `{"error":"invalid input"}`.
- **Actual:** `400` with `text/plain`:
  `Failed to deserialize query string: after: invalid digit found in string`.
- **Evidence:** status 400; body above; all other validation failures on this route
  return JSON.
- **Scope:** systematic for query-deserialization rejections on this route (Axum's
  default rejection is not mapped to `ApiError`). Cosmetic, but clients that parse
  JSON errors will not handle it.

### D3 (minor) — create and PATCH pull-request responses report `code_owner_review_satisfied: true` without evaluating CODEOWNERS

- **Severity: minor**
- **Repro** (target branch has `CODEOWNERS` = `* @carol`, protection
  `require_code_owner_review: true`):
  ```bash
  curl -s -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
    -d '{"title":"x","source_branch":"pr-co-bad","target_branch":"gated"}' \
    http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls
  # 201: "required_code_owners":[], "code_owner_review_satisfied":true

  curl -s -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
    -d '{"method":"merge","expected_head_oid":"<head>"}' \
    http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls/<number>/merge
  # 403 {"error":"not permitted"} (code owner has not approved)
  ```
- **Expected:** the write response either computes the code-owner requirement or
  reports an unknown/neutral value; it must not claim the requirement is satisfied
  when the merge gate refuses the merge.
- **Actual:** `detail()` fills `required_code_owners: []`,
  `code_owner_review_satisfied: true` and `code_owner_errors: []`
  (`crates/githearth-server/src/pulls.rs:566-568`). Only `read_pull` overwrites them
  with the real review (`pulls.rs:917-919`), so create and PATCH responses (and
  `mergeability: []`) carry defaults that contradict the gate.
- **Evidence:** create 201 with `code_owner_review_satisfied:true`; merge on the same
  PR 403; PATCH 200 repeats the same defaults.
- **Scope:** every create and PATCH response; clients that trust the write response
  will believe code-owner review is complete when it is not.

## Notes / skipped

- **Read grants can create pull requests.** erin (`read`) got `201`, not the `403`
  the campaign list expected. This matches ADR 0008, which puts
  `pull_request.create` and `pull_request.comment` in the `read` group so a fork
  author who can read the parent can open a PR. It is recorded as a note, not a
  defect, but the campaign expectation and the decision disagree.
- **Private repo, anonymous API read -> 401, not 404.** The bearer-auth boundary
  rejects the request before repository lookup; no repository data is disclosed.
  The browser serves the sign-in page. Recorded as a note, not a defect.
- **Comments on closed PRs are accepted (`201`).** `create_comment` has no
  open-state check (`crates/githearth-server/src/pulls.rs:1320-1362`); GitHub also
  allows comments on closed PRs. Documented, not a defect.
- **Replies to resolved threads are accepted (`201`).** Resolution lives on the
  root and is inherited by replies; replying does not reopen the thread.
- **Reopening a system-closed deleted-fork PR is allowed** and clears
  `closed_reason` (`PATCH {"state":"open"}` -> 200, `head_id=null`). The code
  comment says reopening intentionally clears a system close reason, but the
  result is an open, permanently unmergeable PR whose source repository is gone.
  Flagged for product review; not counted as a failure of the #134 requirement
  (the deleted-fork PR reads 200/closed before any reopen).
- **Malformed CODEOWNERS with no parseable owners** (`* carol`) passes
  `require_code_owner_review` because there are no valid owners; the parse error is
  surfaced in `code_owner_errors` (["malformed owner token 'carol'"]) but does
  not make the gate fail closed. This matches the parser's documented
  skip-and-report behaviour; flagged as a security-review consideration.
- **Detail-side approval count immediately after a head move could not be read
  directly** because of D1: any non-ancestor head detail returns 500. The merge
  gate was used as the observable and refused the merge with 403 (stale approval),
  then accepted after re-approval. For the `dismiss_stale_approvals=false` rule
  (#136), the post-merge detail returned `approvals=1` with `head_id` at the new
  head, matching the gate that had accepted the merge.
- **Non-empty `required_code_owners` rendering**: gate behaviour was verified
  (non-owner approval 403, owner approval 201, author-as-sole-owner 201), but the
  detail list itself could not be rendered for a live head because of D1. The
  `code_owner_errors` path was verified on an up-to-date PR.
- **Author-as-sole-owner relaxation (#137) breadth:** `compute_review` inserts the
  PR author into the approver set for every owner entry
  (`crates/githearth-server/src/codeowners.rs:520-530`). With `* @t08` the author
  satisfies every changed file, and when a file has several owners the author
  also satisfies that file without any other owner approving (the gate is
  per-file, "any owner approved"). Verified with a no-review merge (201).
- **UI form POSTs return 200 in my client** because the HTTP client follows the
  server's 303 redirect; the state changes were verified through the API.
- **Test mistakes, re-run and corrected (not product defects):**
  - the first carol create used a branch pair erin's PR already owned -> 409; re-ran
    on a free pair -> 201;
  - the first review-list count expected 4 before later reviews existed; re-ran
    with the correct observation;
  - the first "review on closed PR" used the PR author (carol), which correctly
    got 403 self-approval before the open-state check; re-ran as dave -> 409;
  - the first suggestion-content check read the wrong response field (`content`
    instead of `text`); re-ran -> branch content `alpha\nBETA\ngamma\n`;
  - idempotency-key reuse returns the documented `422 KEY_REUSED`, not 409; re-ran
    with the documented expectation.
- **Not in scope / not exercised:** merge queue, auto-merge, status checks,
  review-thread notifications, cross-org fork targets, and concurrency races
  between close and review (covered by unit tests, not repeatable black-box here).

## Round 2 — fixed build (2026-11-14)

Tester: t08. Instance: http://127.0.0.1:8080, rebuilt `githearth-app` image
(container restarted 22:16:22 UTC during the rollout). Scope: re-run the 4
failing cases and the 7 NOTE cases from round 1, then a 15-case smoke.

Verdict: **PASS WITH DEFECTS**. Blocker **D1 is fixed**: every non-empty open PR
detail is 200 in the API and 200 in the UI, including `acme/web` PR #1. **D2 and
D3 remain** (both minor). One single non-reproducible 500 was seen on the first
read after container start (recorded as a note, not counted as a defect).

`docker exec githearth-app-1 git --version` -> `git version 2.47.3`, and
`git merge-tree -h` lists `--[no-]merge-base <tree-ish>`.

Cases: 26 run, 18 pass, 1 fail, 7 notes unchanged.

### Blocker evidence (D1)

- Direct probe inside the app container (read-only, scratch object dir):
  `git merge-tree --write-tree --merge-base=<merge-base> pr-review main` exits
  **0** and returns tree `51f4e89d2bfc7734f855cdb2bc73b641038c9c95`. Round 1
  failed with `error: unknown option 'merge-base=<oid>'` (exit 129).
- `t08-pulls` #11 head `ccd56ad` is **not** an ancestor of `main` `848c32d`
  (`git merge-base --is-ancestor` -> exit 1), i.e. the exact D1 trigger. Its
  detail is now 200 with `approvals:1` and merge/squash/rebase all `clean`.
- Sweep of **46** PR detail reads across `lab08/t08-pulls`, `lab08/t08-xrepo`
  and `acme/web`: **0 non-200**. The **16 non-empty open PRs** all return 200
  in the API and 200 in the browser, each with three mergeability statuses.
- `acme/web` #1: 200 API (head `c128866`, mergeability clean x3) and 200 UI.
- Rebase merge on a fresh PR (#40): **201** (round 1: 500).
- Stability: **1,400 consecutive reads** of live-head PRs over 134 s (spanning
  two 60 s reconcile intervals) all returned 200.

### Re-run of the 4 failing round-1 cases

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 16 | Read PR detail, live head (PR #11) | 200 with head/base/approvals/mergeability | 200; head `ccd56ad` not an ancestor of `848c32d`; approvals 1; merge/squash/rebase all `clean` | PASS: D1 fixed |
| 116 | Read live-source cross-repo PR detail | 200 | New fork `r2-fork` + `t08-xrepo` #5 (head `f964654`): 200, mergeability clean x3; UI 200 | PASS: D1 fixed |
| 151 | PR detail page, live head (PR #11) | 200 | 200; merge box and review form present | PASS: D1 fixed |
| 113 | Create/PATCH code-owner flag | computed or neutral, consistent with the gate | create #41: 201 `required_code_owners:[]`, `code_owner_review_satisfied:true`; PATCH 200 repeats it; detail says `[@carol approved:false]`, satisfied false; merge 403 | FAIL: D3 unchanged |

### Re-run of the 7 NOTE cases

| # | Case | Round-1 note | Round-2 observation | Verdict |
| --- | --- | --- | --- | --- |
| 11 | Read grant (erin) creates a PR | 201 by ADR 0008, not campaign's 403 | 201, PR #36 | NOTE unchanged |
| 20 | Private repo, anonymous API read | 401 auth boundary, no data | 401 `invalid or expired credentials` | NOTE unchanged |
| 84 | Reply to a resolved thread | allowed | 201 | NOTE unchanged |
| 90 | Comment on a closed PR | allowed, no open-state check | 201 on closed #10 | NOTE unchanged |
| 112 | Malformed CODEOWNERS with no valid owners | errors surfaced, gate passes | #37 detail `["malformed owner token 'carol'"]`, owners `[]`, satisfied true; merge 201 | NOTE unchanged |
| 129 | Reopen a system-closed deleted-fork PR | allowed, leaves headless open PR | 200 open, `head_id:null`, `closed_reason` cleared; merge 400 `invalid input` | NOTE unchanged |
| 166 | Anonymous public-repo UI PR list | sign-in page, no PR data | 303 -> `/login` 200 "Sign in", zero PR rows | NOTE unchanged |

### 15-case smoke

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| S1 | Create valid PR (#26) | 201 with detail fields | 201; state open, head/base/author/draft present | PASS |
| S2 | Read detail fields (#11) | 200 with approvals, changes_requested, statuses, mergeability, owners, errors | 200; approvals 1, CR 0, statuses [], three methods, owners [], errors [] | PASS |
| S3 | List, filters and pagination | descending, no overlap | open 18; limit 4 -> `[35,34,33,32]`, next 32; page 2 `[31,30,29,28]` | PASS |
| S4 | Draft lifecycle (#35) | draft; merge 409; ready; back to draft | draft true; merge 409 `pull request is a draft`; ready 200 false; back 200 true; UI list shows `draft (open)` | PASS |
| S5 | Self-approval forbidden (#27) | 403 | 403 `not permitted` | PASS |
| S6 | Latest verdict per reviewer (#27) | latter verdict wins | carol `changes_requested` 201 then `approved` 201; detail approvals 1, CR 0; merge 201; review list `[changes_requested, approved]` | PASS |
| S7 | Stale approvals #136, dismiss=false (#33) | stale approval still counts | carol approved at head1, pushed to `d6be125`; detail approvals **1**; merge **201**, state merged | PASS |
| S8 | Stale approvals, dismiss=true (#34) | stale approval dropped | pushed after approval; detail approvals 0; merge 403 | PASS |
| S9 | Inline threads (#29) | anchor, reply, resolve | anchored comment 201 (path/line/side/commit); reply 201 `in_reply_to` root; resolve 200 true; reply to resolved 201 (note); UI shows Unresolve form | PASS |
| S10 | Suggestion apply (#30) | comment 201, apply changes branch | suggestion `"BETA2\n"`; apply 200 commit `0c640cb`; `app.txt` -> `one\nBETA2\nthree\n` | PASS |
| S11 | Code owners gate and reporting (#31) | detail matches gate | open detail `[@carol approved:false]` satisfied false, merge 403; after carol approval `[@carol approved:true]` satisfied true, merge 201; UI #39 renders `Code owners @carol (required)` | PASS |
| S12 | #137 author-as-sole-owner (#32) | 201 without review | detail `[@t08 approved:true]` satisfied true; merge 201 | PASS |
| S13 | Cross-repo PR live and after fork delete (#134) | 200 live; 200 closed after delete | #5 200 live (UI 200); DELETE fork 204; detail 200 closed, `head_id:null`, `closed_reason:"source repository deleted"`; UI closed "(deleted)" | PASS |
| S14 | NEW: outstanding change request blocks merge (#28) | 409; merge form not mergeable | carol CR -> detail CR 1; merge **409 `the pull request has an outstanding change request`**; after approval the merge was accepted (201) and merged on retry once the target settled. UI #38 shows CR 1 and **no merge form**; a forced UI merge POST returns **409** with "This pull request cannot be merged right now." | PASS, with note (below) |
| S15 | UI smoke | list/detail/forms/guards | list 200, detail 200, draft badge, merge method select on #29, up-to-date guard disabled on #24, CSRF-less POST 403, `<script>` escaped; 16/16 non-empty open PR pages 200 | PASS |

### Additional checks

- **D2 (minor) still present.** `GET .../pulls?after=abc` -> **400**
  `content-type: text/plain; charset=utf-8`,
  `Failed to deserialize query string: after: invalid digit found in string`.
  Unchanged from round 1.
- **Code-owner-satisfied reporting.** Read paths are now correct and agree with
  the gate: #31 false -> 403, true -> 201; #39 false and UI "@carol (required)";
  #32 true -> 201. Write paths are still wrong: create #41 says `[]`/`true`,
  PATCH #41 says `[]`/`true`, while the detail says `[@carol false]` and the
  merge gate returns 403. This is D3, not a new issue.
- **Change-request UI note.** The detail page suppresses the merge form when a
  change request (or an unmet approval rule) blocks the merge, but prints no
  inline reason next to it. The reason only appears if a merge POST is forced
  (409 "This pull request cannot be merged right now."). Blocking is correct;
  the missing explanation is a minor UX gap.
- **One cold-start 500.** The first read of the run, `t08-pulls` #25 (closed) at
  22:17:22, returned 500 with
  `{"level":"ERROR","fields":{"message":"repository read failed","category":"git"}}`.
  It was the first request after the container restart; 15 immediate retries and
  every later sweep (including the 1,400-read stability loop) returned 200. Not
  reproducible; recorded for monitoring, not counted as a defect.
- **Merge is asynchronous.** Merge endpoints return a merge intent (`state:
  "queued"`, 201); the worker applies it within ~1 s. One intent (#28) went
  `stale` with `failure_reason:"target moved"` because other merges advanced
  `main` between approval and processing; a retry merged. Correct optimistic
  concurrency, not a defect.
- **Fixtures created.** `t08-pulls` PRs #26-#41 and branches `r2-*`; fork
  `r2-fork` created and deleted; `t08-xrepo` #5 left open/headless after the
  #129 reopen check. The repo source tree was not edited and no container was
  restarted by me.

### Defect status after round 2

- **D1 (blocker) — RESOLVED.** Every non-empty open PR detail is 200 in API and
  UI, including `acme/web` #1; rebase mergeability and rebase merge both work on
  Git 2.47.3.
- **D2 (minor) — REMAINS.** Plain-text 400 for an invalid query cursor.
- **D3 (minor) — REMAINS.** Create/PATCH responses report
  `code_owner_review_satisfied:true` with empty `required_code_owners` while the
  detail and the merge gate say the requirement is unmet.

**Updated verdict: PASS WITH DEFECTS.** The blocker is cleared and 3 of the 4
round-1 failures now pass; the remaining failure is the minor D3 reporting
defect, and D2 is unchanged.

