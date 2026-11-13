# Forks and cross-repository pull requests — M4 lab acceptance report

Tester: t11. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: **FAIL** (one blocker: every non-empty open pull request detail returns 500/503 on the shipped image).
Cases: 84 run (138 individual assertions), 67 pass, 16 fail, 1 skipped.

## Setup

- Owner of `lab11`; used `t11` plus `carol` (maintain), `dave` (maintain), `erin` (read), `frank` (triage), `grace` (member).
- Seeded public `lab11/t11-upstream`: branches `main`, `feature`, `docs` (+ a PR branch), annotated tag `v1.0.0`, 5 commits after merges; also `t11-empty`, `t11-sha256` (sha256 object format), `t11-d15-nonfork`.
- Created scratch orgs `t11-scratch` (t11 owner) and `t11-erin-org` (erin owner) for cross-org forks/PRs. All names carry the `t11-` prefix.
- Parents: `lab11/t11-upstream`, public empty `acme/site`, private readable `acme/web`; unreadable: `beta/app` and `lab12/t12-notify`.
- No application restart, migration or acme/beta write; only reads/clones/fork-into-lab11 of acme fixtures. acme/site was forked into lab11 only.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A1 | Fork public repo into lab11 under a new name | 201; parent_repository_id set; visibility public; status active | 201; parent 13cfc3ad…; public; active | PASS |
| A2 | Fork response field set | Repository fields incl parent_repository_id | 8 keys exactly, incl parent_repository_id | PASS |
| A3 | Fork with name omitted | 201; name from parent | 201; name t11-empty | PASS |
| A4 | Fork same parent twice under different names | Both 201, distinct ids | 201; second id d7ae4c7f… | PASS |
| A5/A6 | Fork name conflict, incl case-insensitive | 409 repository name already taken | 409 for t11-a1-fork and T11-A1-FORK | PASS |
| A7 | Fork private repo the actor can read (acme/web) | 201; visibility private | 201; private; parent acme/web id | PASS |
| A8 | Fork repo the actor cannot read (beta/app; lab12/t12-notify) | 404 without revealing existence | 404 not found for both | PASS |
| A9 | Fork nonexistent parent | 404 | 404 repository not found | PASS |
| A10 | Fork into nonexistent target organization | 404 organization not found | 404 organization not found | PASS |
| A11 | Fork into acme where t11 lacks repository.create | 403 not permitted | 404 organization not found | FAIL |
| A12 | Fork into lab11 where erin is read-only | 403 not permitted | 404 organization not found | FAIL |
| A37 | Fork into lab11 by frank (triage) | 403 not permitted | 404 organization not found | FAIL |
| A13 | Read-only erin forks acme/web into an org she owns | 201; private | 201; private | PASS |
| A14–A18 | Invalid fork payloads (name, .git, long description, unknown field, empty org) | 400 for each | 400 invalid input for each | PASS |
| A19 | Fork of an archived repo | Policy unspecified; record result | 201; fork is active and archived=false | PASS |
| A20 | Fork of a deleted parent | 404 | 204 delete; 404 fork | PASS |
| A21 | Fork of a fork | 201; parent is immediate fork | 201; parent = t11-a1-fork id | PASS |
| A22 | Fork into another organization owned by the actor | 201 in t11-scratch | 201 | PASS |
| A23–A25 | Idempotent replay, key reuse with different body, 5 concurrent same-name forks | Replay same id; 422 on reuse; one 201 rest 409 | 201/201 same id; 422; codes 201/409/409/409/409 | PASS |
| A26/A27 | Fork name length boundaries | 100 accepted; 101 rejected | 201; 400 | PASS |
| A28 | Fork inherits internal visibility | 201; internal | 201; internal | PASS |
| A29/A30 | Fork of empty public repo (acme/site) and empty own repo | 201; clone empty | 201; clone empty | PASS |
| A31/A32 | Fork unauthenticated; fork with repo:read token | 401; 403 read-only token | 401; 403 | PASS |
| A33 | Idempotent replay after deleting the fork | Stored 201 replayed; no resurrection | 201 cached; GET 404; no row recreated | PASS |
| A38/A39 | Fork sha256 repository and clone | 201; clone sha256 with history | 201; clone sha256; commit present | PASS |
| A34–A36/A40 | Maintainers (carol, dave) and member (grace) fork into lab11; carol forks private acme/web | 201; private for private parent | 201; private | PASS |
| B1 | Fork copies every branch and tag ref | ls-remote fork == parent | 7 refs, identical | PASS |
| B2/B3/B8 | Fork preserves branches, history, annotated tag, default branch | Branches/tags/history and HEAD match | main/feature/docs, v1.0.0 tag object, HEAD main, 5 commits | PASS |
| B4 | Push to the fork leaves the parent untouched | Parent lacks new branch | Parent refs unchanged | PASS |
| B5 | Delete parent, then clone the fork | Clone succeeds with pre-fork history | 204 delete; clone 200; history present | PASS |
| B6 | Deleted parent link retained on fork | parent_repository_id still set | 200 with parent id | PASS |
| B7 | Fork of empty repo clones empty | Clone succeeds, no refs | Empty clone | PASS |
| C1 | Fork API response carries parent link | parent_repository_id = parent id | 200 with parent id | PASS |
| C2 | Parent JSON shows children / fork count | fork_count or children field | No such field (8-key repository object only) | FAIL |
| C3 | List forks through the API | 200 with child forks | 405 (only POST /forks is routed) | FAIL |
| C4 | Repository page lists child forks / fork count | Fork list or count rendered | Only the Fork form card; no list/count | FAIL |
| C10 | Fork page links back to its parent | Forked-from link | No parent link on fork page | FAIL |
| C11 | Organization list distinguishes forks | Fork marker/count | Plain list, no fork marker | FAIL |
| C5/C6 | Fork form offers only namespaces the actor can create in | t11: lab11+t11-scratch; erin: t11-erin-org only | As expected | PASS |
| C7a/C7b | No private fork leakage on the public parent page | Private fork name absent for anonymous and t12 | Absent | PASS |
| C8/C8b/C8c | Public fork page anonymous; private fork anonymous; private fork non-member | 200; redirect to login/no leak; 404 | 200; 303 /login no name; 404 | PASS |
| C9/C9b | Private fork metadata and clone permissions | erin/dave 200; t12/anonymous 404 | As expected | PASS |
| D1 | Open PR from fork branch to upstream main | 201; source_repository_id names fork | 201; source_repository_id = fork id | PASS |
| D2 | Cross-repo PR resolves head from fork and base from upstream | head_id = fork sha; base_id = upstream main | Both match | PASS |
| D3 | Upstream PR list includes the cross-repo PR | Item with source_repository_id | Found | PASS |
| D4/D4b | Fork PR list/page includes outgoing PRs | Contains PR #3 | API items=[]; UI says no pull requests | FAIL |
| D6 | Review at fork head counts | 201; approval counted | 201; approvals=1 on merged PR #1 | PASS |
| D7 | Author cannot approve their own PR | 403 | 403 | PASS |
| D8/D9/D8b | Fork head moves; old head merge; stale approval | Head moves; old-head merge 409; stale approval blocks a 1-approval gate | Push moved head; 409 commit is not the head; 403 approval gate | PASS |
| D10/D12 | Merge by upstream owner; worker revalidation with moved fork head | PR #4 merges; PR #3's queued intent becomes stale | PR #4 merged; PR #3 intent stale, reason source moved | PASS |
| D11 | Merge brings fork commit into main | Fork commit ancestor of main | Ancestor true | PASS |
| D13/D14 | Push after merge; merged cross-repo PR detail | State stays merged; detail 200 | Merged; 200 | PASS |
| D15/D16/D24 | Non-fork source, fork-of-fork to grandparent, source = target | 400 not a fork | 400 for each | PASS |
| D17 | Missing fork source branch | 422 branch not found | 422 | PASS |
| D18/D18b | Same branch name both sides (empty diff); fork in another org | 201; allowed | 201 for docs→docs and t11-scratch fork | PASS |
| D19 | Private source fork the actor cannot read | 404 | 404 pull request not found | PASS |
| D20/D21/D22/P16 | Read user opens PR, cannot merge, can comment (via carol) | 201; 403; 201 | 201; 403; 201 | PASS |
| D27 | Second open PR for same branch pair | 409 | 409 already open | PASS |
| D28/D29 | Delete fork head branch after PR creation | PR stays open, head null; merge 422 | Head null; 422 branch not found | PASS |
| D30 | Empty cross-repo PR (head = target tip) | Allowed or refused per docs; merge outcome recorded | Create 201; merge intent failed:"nothing to merge"; PR stays open | PASS |
| D23 | Squash-merge a cross-repo PR | Merged; content on main | 201; merged; main 61b1676 contains d23.txt; detail 500 (D-01) | PASS |
| D26 | Open cross-repo PR detail stable across 20 reads | 20x 200 | 20x 500 | FAIL |
| D31 | Open same-repository PR detail (control) | 200 | API 500; UI 503 | FAIL |
| P12 | Maintainer dave merges a cross-repo PR | 201; merged; commit on main | 201; merged; ancestor true | PASS |
| D25 | Cross-object-format fork PR | Not reachable: forks inherit object format | Direct non-fork source 400; no cross-format fork exists | SKIP |
| E1 | Delete fork via API; PR detail after deletion | 204; PR 200 closed with reason | 204; 200 closed, source repository deleted | PASS |
| E2/E3/E4 | Closed PR head null, base resolves; list reason; absent from open list | head null; closed_reason set | As expected | PASS |
| E5 | Merge a fork-deleted closed PR | 409 not open | 409 | PASS |
| E6 | Browser PR page after fork deletion | 200, not 500/503 | 200; shows closed and Head (deleted) | PASS |
| E7/E17 | Comments and reviews survive fork deletion | 200 with rows retained | 200; 1 review and 1 comment retained | PASS |
| E15 | Delete fork with queued merge intent | PR closed; intent cancelled | PR closed; intent cancelled | PASS |
| E16/E18 | Delete fork with merged PRs | PRs stay merged; fork page 404 | PR #4/#10 merged; 404 | PASS |
| E11 | Recreate same fork name after deletion | 201 with new id | New id | PASS |
| E12 | Delete a fork that has its own fork | Child fork remains readable | 200; parent link retained | PASS |
| E13 | Delete a fork of an empty repo | 204 then 404 | 204; 404 | PASS |
| E8/E19/E9 | Delete twice, clone deleted fork, deleted fork page | 404 | 204 then 404; clone 404; page 404 | PASS |
| E14 | Deleting a fork does not change upstream refs | Refs identical | Identical | PASS |
| E20 | Reopen a PR system-closed by fork deletion | Refused; reason preserved | 200 open; closed_reason null; head null | FAIL |
| F1/F3/F5/F7/F4/F16 | Fork card; UI fork success same/cross org; conflict 409 page; bad CSRF 403; missing key 400 | As expected | All as expected | PASS |
| F6 | Erin browser-forks into lab11 without create | 403 permission page | 404 The repository no longer exists | FAIL |
| F6b | t11 browser-forks into acme without create | 403 permission page | 404 The repository no longer exists | FAIL |
| F8 | Browser page for an open cross-repo PR | 200 | 503 | FAIL |
| F9 | Browser page for a merged cross-repo PR | 200 and merged | 200; merged | PASS |
| F10 | Browser new-PR form can name a fork | Source repository field / fork picker | No source_repository field; fork page has no contribute action | FAIL |

## Defects

Six defects: one blocker, two major, three minor.

### D-01 (blocker) Every non-empty open pull request detail is 500 (API) / 503 (UI): the shipped Git 2.39 cannot run the rebase mergeability command

- Severity: blocker
- Repro:
  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
    -H 'Content-Type: application/json' \
    -d '{"username":"t11","password":"<test-password>"}' \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
  curl -si -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/pulls/3
  # HTTP/1.1 500 Internal Server Error
  # {"error":"storage operation failed"}
  ```
  The same happens for open PR #5 (same-repository control). In a signed-in browser,
  `GET /organizations/lab11/repositories/t11-upstream/pulls/3` returns 503.
  Root-cause check:
  ```bash
  docker exec githearth-app-1 git --version          # git version 2.39.5
  docker exec githearth-app-1 sh -c 'T=/var/lib/githearth/repositories/13/13cfc3adaee24092be97ed93f32e824f.git; \
    git --git-dir=$T merge-tree --write-tree --merge-base=$(git --git-dir=$T rev-parse HEAD) \
    $(git --git-dir=$T rev-parse HEAD) $(git --git-dir=$T rev-parse HEAD)'
  # error: unknown option `merge-base=71cf5adf...`
  ```
- Expected: 200 for an open PR, with live `head_id`/`base_id` and mergeability; browser page 200.
- Actual:
  - `GET .../pulls/3` and `.../pulls/5` return 500 `{"error":"storage operation failed"}`; 20 consecutive reads of #3 all 500.
  - Browser PR pages for open PRs return 503.
  - Server log: `repository read failed category=git repository_id=13cfc3ad-…`; 115 such errors in 30 minutes across several repositories.
  - Merged-by-merge PRs #1/#4 are 200 (the source is now an ancestor, so the rebase check short-circuits). Squash-merged PR #10 remains 500 permanently because its source oid is not an ancestor.
- Evidence / cause:
  - `pulls::read_pull` always computes mergeability for open tips (`crates/githearth-server/src/pulls.rs:889-900`).
  - It checks merge, squash and rebase (`crates/githearth-server/src/mergeability.rs:130-141`).
  - The rebase check runs `git merge-tree --write-tree --merge-base=<base> <previous> <commit>` (`crates/githearth-git/src/merge.rs:285`).
  - The Compose image installs Debian bookworm's git 2.39.5 (`Dockerfile:17-18`), which rejects `--merge-base` for `merge-tree`. ADR 0004 says the server checks Git ≥ 2.38 and documents this exact syntax (`docs/decisions/0004-merge-execution.md:61-67`), so the version guard passes on an image that cannot execute the command.
- Scope: systematic and image-wide; not fork-specific, but it blocks the fork PR page, PR detail API and any UI revalidation display. Every non-empty open PR is affected; an empty PR (head == target) short-circuits as `UpToDate`. Merge execution itself still works through the worker.

### D-02 (major) Forking into a namespace without `repository.create` returns masked 404 instead of 403 (#133 regression)

- Severity: major
- Repro:
  ```bash
  # t11 is an acme member but cannot create repositories there:
  curl -si -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"name":"web","visibility":"public"}' \
    http://127.0.0.1:8080/api/v1/organizations/acme/repositories
  # 403 {"error":"not permitted"}  (create is denied)

  curl -si -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"organization":"acme","name":"t11-a11-acme"}' \
    http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/forks
  # 404 {"error":"organization not found"}   expected 403
  ```
  Erin (read on lab11, create denied):
  ```bash
  curl -si -X POST -H "Authorization: Bearer $ERIN_TOKEN" -H 'Content-Type: application/json' \
    -d '{"organization":"lab11","name":"t11-a12-erin"}' \
    http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/forks
  # 404 {"error":"organization not found"}; POST /organizations/lab11/repositories -> 403
  ```
  Frank (triage) gets the same 404. In the browser, erin (and t11 into acme) gets a 404 page saying “The repository no longer exists.” instead of the 403 permission page.
- Expected: 403 `not permitted` when the actor can read the parent but lacks `repository.create` on the target namespace, as in the #133 fix.
- Actual: 404 `organization not found`, the pre-#133 masked denial.
- Evidence / cause: `crates/githearth-server/src/forks.rs:110-123` returns `org_not_found()` both when the target facts cannot be loaded and when `Action::RepositoryCreate` is denied. The browser maps the 404 to “The repository no longer exists.” (`ui.rs:889-913`).
- Scope: systematic for every actor who can read the parent but cannot create in the target; affects API and UI.

### D-03 (minor) No fork children list, fork count, or parent link in the API/UI

- Severity: minor
- Repro:
  ```bash
  curl -si -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/forks
  # 405 (only POST is routed)

  curl -s -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream
  # keys: id, organization_id, name, description, visibility, archived, status, parent_repository_id
  # no fork_count / forks / children

  curl -s -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-a1-fork
  # has parent_repository_id, but the browser fork page has no "forked from" link,
  # the parent page has only the <h2>Fork</h2> creation form, and the org list has no fork marker.
  ```
- Expected (campaign acceptance): fork list on the repo page/API shows children, a parent link and a fork count.
- Actual: only the child→parent `parent_repository_id` is exposed. There is no reverse listing, count or UI parent/child navigation.
- Evidence / cause: `forks.rs:25-30` registers only `POST .../forks`; `RepositoryJson` has no fork metadata (`repository_api.rs:53-63`); `ui.rs:724-801` renders only the fork form. No ADR or `docs/foundation.md` section documents a listing endpoint/UI, so this may be an intentional omission, but it fails the stated acceptance criterion.
- Scope: all repositories; no private-fork leakage was found (C7a/C7b pass).

### D-04 (minor) A fork's PR list/page omits PRs whose head lives in that fork

- Severity: minor
- Repro:
  ```bash
  curl -s -H "Authorization: Bearer $TOKEN" \
    'http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-a1-fork/pulls?state=all'
  # {"items":[],"next":null}

  curl -s -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/pulls
  # contains PR #3 with source_repository_id=dd7c6607-...
  ```
  The fork's browser Pull requests page says there are no pull requests.
- Expected (campaign acceptance): PR list appears in both the upstream and the fork.
- Actual: the fork list is always empty for outgoing PRs.
- Evidence / cause: `list_pulls` selects `WHERE repository_id = $1` (the target) at `crates/githearth-server/src/pulls.rs:843-849`; `source_repository_id` is never considered. The ADR defines PRs on the target, so this is a discoverability gap rather than a data loss.
- Scope: every cross-repo PR.

### D-05 (minor) A PR system-closed by fork deletion can be reopened, clearing the reason and leaving an open PR with no head

- Severity: minor
- Repro (PR #17 from `lab11/t11-e20-fork`):
  ```bash
  curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-e20-fork   # 204
  curl -s -H "Authorization: Bearer $TOKEN" \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/pulls/17
  # state=closed, closed_reason="source repository deleted", head_id=null

  curl -s -X PATCH -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"state":"open"}' \
    http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/pulls/17
  # 200; state=open, closed_reason=null, head_id=null
  ```
- Expected: refuse reopening while `source_repository_id` names a deleted fork (or preserve the system reason), so no open PR exists without a head.
- Actual: reopen succeeds, the reason is erased, and the PR is open with no head; a merge then fails 422 `branch not found`.
- Evidence / cause: `crates/githearth-server/src/pulls.rs:997-1004` clears `closed_reason` whenever the new state is `open` and never checks the source repository's status; only `merged` is terminal. The existing test `tests/forks.rs:986` covers clearing the reason when the fork still exists, not the deleted-fork case.
- Scope: any PR closed by fork deletion (`closed_reason = 'source repository deleted'`).

### D-06 (major) The browser cannot open a cross-repo PR; the fork flow dead-ends

- Severity: major
- Repro:
  ```bash
  curl -s -H "Cookie: githearth_session=..." \
    http://127.0.0.1:8080/organizations/lab11/repositories/t11-upstream/pulls/new
  # form contains source_branch and target_branch only; no source_repository / fork picker
  ```
  The fork page has no “Contribute”/“New pull request” link that selects the fork. The JSON API accepts `source_repository` and creates the PR (201, D1/D18b), so only the browser path is missing.
- Expected: a user who forks via the browser can propose a PR back from the fork (a source-repository field, picker, or prefilled link from the fork page).
- Actual: cross-repository PR creation is API-only; the new-PR form posts only `source_branch`/`target_branch`.
- Evidence / cause: `crates/githearth-server/src/pulls_ui.rs:389-390` (form fields) and `:494-495` (create payload) never send `source_repository`; `ui.rs:724-801` has no contribute action on the fork page.
- Scope: all fork contributions opened through the browser.

## Notes / skipped

- 138 individual assertions were executed; the table groups closely related boundary/negative checks into 84 reported cases.
- D25 skipped: a fork always inherits its parent's object format, so a real cross-object-format fork cannot exist. A direct non-fork source is already rejected with 400. sha256 forks and sha256 cross-repo PRs were tested separately (A38/A39/D26) and work.
- Campaign wording “read user … cannot open a PR against a repo where she lacks write” conflicts with ADR 0008, which puts `pull_request.create` and `pull_request.comment` in the read group. Erin opened PR #9 against the public upstream (201) and could not merge it (403); this matches the ADR, so it is recorded as PASS, not a defect.
- Forking an archived repository is allowed (A19) and produces an active, unarchived fork. ADR 0008 only checks `status = active`, not `archived`; this is an observation/policy question, not scored as a defect.
- Replaying an idempotent fork create after deleting the fork returns the stored 201 response (A33) but does not recreate the row (`GET` 404). This is normal idempotent-replay semantics, not a resurrection.
- Empty cross-repo PRs are accepted (201); the merge intent ends `failed` with “nothing to merge” and the PR stays open (D30). The docs do not forbid empty PRs.
- The browser PR page after fork deletion returns 200 and shows `Head (deleted)`; it does not render the `closed_reason` text. The acceptance requirement was “not 500/503”, so this passes.
- The early 500s on PR detail were not flake: 20 consecutive reads reproduced the same result. PR #1 became readable only after a merge-method merge made its source an ancestor; squash-merged PRs stay at 500.
- No application restart, migration, or acme/beta/other-lab write was performed. Only read/clone/fork-into-lab11 operations touched acme fixtures; `t11-scratch` and `t11-erin-org` are campaign-prefixed scratch orgs.

## Round 2 — fixed build (2026-11-14)

Tester: t11. Re-run on the fixed build at http://127.0.0.1:8080 (branch `fix/m4-lab`, 7e1ded3; checkout HEAD is still b820454). Container image built 2026-11-13T22:15Z on Debian 13.7 with Git 2.47.3. No restart, rebuild, migration or repository edit was performed.

Updated verdict: **PASS WITH DEFECTS** — D-01 (blocker) and D-02 (major) are fixed. D-06 (major, browser cross-repository PR creation) and D-03/D-04/D-05 (minor) are unchanged.
Cases: 37 re-run (27 targeted + 10-case smoke), 31 pass, 6 fail, 0 skipped. The six failures are the same four open defects.

### Re-run table

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R1 | D-01: every open PR detail, API, instance-wide | 200 with live head/base | 65/65 open PRs 200 (snapshot at 22:35; cross-repo included, e.g. `lab11/t11-upstream#3`, `lab08/t08-xrepo#2`) | PASS |
| R2 | D-01: stability, 20 reads each of `#3`, `#5`, `#10` | 20x 200 | 20x 200 each | PASS |
| R3 | D-01: other detail shapes — same-repo `#5`, sha256 `t11-sha256#1`, `acme/web#1`, squash-merged `#10` | 200 | 200 for each; `#10` no longer permanent 500 | PASS |
| R4 | D-01: browser pages for open PRs `#3`/`#5` | 200 | 200/200 | PASS |
| R5 | D-01: log scan for PR-detail failures | no PR-detail 500s | one 500 at 22:17:22 from another tester’s `lab08/t08-pulls` during active writes; not reproducible, all 38 PRs there re-read 200; no lab11 500s | PASS |
| R6 | D-02: fork `acme/web` into `acme` as t11 (read, no create) | 403 not permitted | 403 `{"error":"not permitted"}` | PASS |
| R7 | D-02: fork into `lab11` as erin (read, no create) | 403 | 403 not permitted | PASS |
| R8 | D-02: fork into `lab11` as frank (triage) | 403 | 403 not permitted | PASS |
| R9 | D-02: fork into `acme` as erin (read) | 403 | 403 not permitted | PASS |
| R10 | D-02 positive control: carol has maintain in acme | 201 | 201; probe fork deleted immediately via alice | PASS |
| R11 | D-02: unknown namespace `t11-no-such-org` | 404 | 404 organization not found | PASS |
| R12 | D-02: existing namespace without membership (`lab12`, `beta`) | 404 | 404 organization not found | PASS |
| R13 | D-02: unreadable parent (`beta/app`) | 404 | 404 not found | PASS |
| R14 | D-02: denied fork leaves no repository row | 404 GET of proposed names | 404 for all four denied names | PASS |
| R15 | D-02: browser fork `acme` as t11 | 403 permission page | 403 “Fork failed — You do not have permission to fork this repository.” | PASS |
| R16 | D-02: browser fork `lab11` as erin | 403 permission page | 403 same page | PASS |
| R17 | D-02: browser fork unknown namespace | 404 | 404 “The repository no longer exists.” | PASS |
| R18 | #134: system-closed deleted-fork PR `#13` | 200, closed reason preserved | 200, `closed`, `source repository deleted`, `head_id` null; 20x 200 | PASS |
| R19 | #134: fresh deleted-fork PR `#18` | 200 after fork deletion, null head | 200; `closed_reason="source repository deleted"`, head null; comments/reviews 200; browser 200 shows `Head (deleted)` | PASS |
| R20 | D-03: fork children list, fork count, parent link | `GET /forks` 200 with children; count field; parent link in UI | `GET .../forks` 405; repository JSON has only the same 8 keys, no `fork_count`/`children`; fork page has no parent/“forked from” link; parent page has only the Fork form; org list has no fork marker | FAIL (D-03) |
| R21 | D-04: fork PR list includes outgoing PRs | `#3` listed on `t11-a1-fork` | `{"items":[],"next":null}`; upstream still lists `#3` with `source_repository_id=dd7c6607…` | FAIL (D-04) |
| R22 | D-05: reopen a PR system-closed by fork deletion | refused, or reason preserved | `PATCH #18 {"state":"open"}` → 200, `state=open`, `closed_reason=null`, `head_id=null`; reproduced twice | FAIL (D-05) |
| R23 | D-06: browser new-PR form can name a fork | source-repository field or prefilled fork link | form posts only `source_branch`, `target_branch`, `title`, `body`, `draft`; no `source_repository`; fork page has no Contribute/New PR link; Compare is same-repo only | FAIL (D-06) |
| R24 | Fork description limit now counts characters | 1000 multibyte chars 201; 1001 400 | 201; 400 | PASS |
| R25 | Browser fork success (regression on the 403 change) | 303 to new repo | 303; repository 200 | PASS |
| R26 | C5 fork form offers only creatable namespaces | t11: lab11 + t11-scratch; erin: t11-erin-org | as expected | PASS |
| R27 | C7/C9 private fork visibility and no leak | private; anon 303; members 200 | private; anon 303; no private name on public pages | PASS |

Evidence for the two fixes:

```bash
docker exec githearth-app-1 sh -c 'cat /etc/debian_version; git --version'
# 13.7
# git version 2.47.3
docker exec githearth-app-1 sh -c 'T=/var/lib/githearth/repositories/13/13cfc3adaee24092be97ed93f32e824f.git; \
  git --git-dir=$T merge-tree --write-tree --merge-base=$(git --git-dir=$T rev-parse HEAD) \
  $(git --git-dir=$T rev-parse HEAD) $(git --git-dir=$T rev-parse HEAD)'
# d4e82728…, exit 0   (2.39 rejected --merge-base; 2.47 executes it)

curl -s -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:8080/api/v1/organizations/lab11/repositories/t11-upstream/pulls/3
# 200; head_id=1ed136ae… = t11-a1-fork refs/heads/b4-push;
# mergeability: merge clean, squash clean, rebase clean

curl -si -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"organization":"acme","name":"t11-r2-a11-acme"}' \
  http://127.0.0.1:8080/api/v1/organizations/acme/repositories/web/forks
# HTTP/1.1 403 Forbidden
# {"error":"not permitted"}
```

The #134 check used `#13` (already system-closed by the round-1 E15 fork deletion) and a fresh `t11-r2-del-fork` → PR `#18` → `DELETE` fork (204). Before the D-05 reopen probe, `GET .../pulls/18` returned 200 with `state=closed`, `closed_reason="source repository deleted"`, `head_id=null`.

### 10-case smoke (fresh fixtures)

| # | Case | Actual | Verdict |
| --- | --- | --- | --- |
| S1 | Fork `t11-upstream` → `lab11/t11-r2-smoke` | 201, parent `13cfc3ad…` | PASS |
| S2 | Clone fork, push `r2-smoke-branch` | push accepted; head `cad40a5…` | PASS |
| S3 | Open cross-repo PR (`lab11/t11-r2-smoke:r2-smoke-branch` → `t11-upstream:main`) | 201 PR `#19`, head fork sha, base upstream main | PASS |
| S4 | Open cross-repo PR detail API + browser | API 200 (`merge`/`squash`/`rebase` all clean); browser 200 | PASS |
| S5 | Merge by upstream owner | 201 intent, then `merged`; main `122b672` has `cad40a5` as ancestor | PASS |
| S6 | Fork’s PR list includes the outgoing PR | `{"items":[],"next":null}` | FAIL (D-04) |
| S7 | Permissions — erin→lab11 403, t11→acme 403, unknown 404, `beta/app` 404 | 403/403/404/404 | PASS |
| S8 | Delete fork; merged PR stays readable | 204; API 200 `merged`; browser 200; fork 404 | PASS |
| S9 | Fork after deletion — recreate same name | 201, new id `befeb9e5…`; no resurrection of the old row | PASS |
| S10 | Reopen the system-closed `#18` | 200 `open`, reason cleared, head null | FAIL (D-05) |

### Defect status after round 2

| Defect | Severity | Status |
| --- | --- | --- |
| D-01 every non-empty open PR detail 500/503 | blocker | **FIXED** — image ships Git 2.47.3; 65/65 open PRs 200, 20x stable, browser 200, squash-merged `#10` readable |
| D-02 fork without create returns masked 404 | major | **FIXED** — members with read/no-create get 403 API + browser; strangers/unknown get 404; no row created |
| D-03 no fork children list/count/parent link | minor | **OPEN** — unchanged: `GET /forks` 405, no JSON count, no UI parent/child navigation or org marker |
| D-04 fork PR list omits outgoing PRs | minor | **OPEN** — unchanged: fork list empty while upstream lists the PR |
| D-05 system-closed PR can be reopened, reason erased, head null | minor | **OPEN** — unchanged; reproduced twice on `#18` |
| D-06 browser cannot open a cross-repo PR | major | **OPEN** — unchanged; new-PR form has no source repository and the fork page still dead-ends |

### Notes

- The one PR-detail 500 in the server log (22:17:22, `lab08/t08-pulls`, repo `ce150430…`) happened while that tester was actively pushing; every one of its 38 PRs returned 200 on recheck and none occurred in lab11. It is recorded as non-reproducible, not as a remaining D-01 instance.
- `#134` is the issue number fixed by `fix/m4-lab` (commit 8b56bd4: “a closed cross-repository pull request stays readable after its source fork is deleted”); there is no PR #134 in this installation. The case maps to the deleted-fork readability checks above.
- Positive-control fork `acme/web` → `acme` by carol (201) was deleted via alice immediately; `acme` again lists only `web` and `site`. All other round-2 repositories were created under `lab11` and deleted; PR `#18` (closed) and `#19` (merged, merge commit `122b672`) remain in `t11-upstream` as test records.
- D-03 may be an intentional omission (no ADR documents a listing endpoint or UI), but it still fails the stated acceptance criterion. D-06, if browser parity is a release gate, still leaves the browser contribution path unusable even though the API path is complete.
