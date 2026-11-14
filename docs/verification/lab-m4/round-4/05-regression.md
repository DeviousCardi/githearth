# 05 — Pull requests, merges, protection and UI regression (M4 round 4)

- Date: 2026-11-14
- Instance: `main` `cd7946b` (PR #163 merged), HTTP <http://127.0.0.1:8080>, SSH 2222, PostgreSQL 54329
- Account: `t05` (owner of `lab05`); peer accounts `bob` (write), `carol` (maintain), `dave` (admin), `erin` (read), `frank` (triage), `grace` (no grant), `t06` (not a member of `lab05`)
- Method: JSON API with session bearer tokens, browser forms with a real session cookie + CSRF, and Git Smart HTTP pushes as each user. Merge cases poll the pull request until the merge worker finishes (2 s interval, 25–30 s cap).
- Harness: `work-r4/t05/suite.py` (109 cases). Raw results: `results-r4a.json` (first pass), `results-r4b.json` (corrected re-run), `results-r4d.json` (final pass).

## Verdict

**PASS.** 109/109 cases pass on the final pass. No reproducible defect was found in pull
requests, merges, branch/tag protection, the new branch-settings page, the reviewer UI or
the comment redirect. The merge-auth ordering change did not break any legitimate merge or
auto-merge path, and the reviewer UI left the JSON API behaviour and response shapes intact.

One **transient** `500 storage operation failed` was seen once on a merge in the first pass
(observation O1); it did not reproduce in a dedicated 8/8 reproduction run, an immediate
manual retry, or either of the two further full passes.

## Counts

| Pass | Cases | Pass | Fail |
|---|---|---|---|
| First pass (`r4a`) | 104 | 93 | 11 |
| Corrected re-run (`r4b`) | 104 | 104 | 0 |
| Final pass (`r4d`, +5 ordering/UI-idempotency cases) | 109 | 109 | 0 |

Of the 11 first-pass failures, 10 were wrong expectations in my harness (status-code
choices documented below), and 1 was the transient 500. Every failing case was re-run; the
final pass is a fresh instance namespace (`regr4d*`) with the same suite.

| Area | Cases | Pass |
|---|---|---|
| A | 12 | 12 |
| B | 15 | 15 |
| C | 13 | 13 |
| D | 7 | 7 |
| E | 9 | 9 |
| F | 17 | 17 |
| G | 17 | 17 |
| H | 9 | 9 |
| I | 5 | 5 |
| J | 5 | 5 |
| **Total** | **109** | **109** |

## Focus items

### 1. Merge authorization now runs before pull-request state (no regression)

`merge_intents::create_intent` authorizes `pull_request.read` before the `state != "open"`
check (`crates/githearth-server/src/merge_intents.rs:526`), and `auto_merge::enable_auto_merge`
does the same (`crates/githearth-server/src/auto_merge.rs:235`). `require_pull`
(`crates/githearth-server/src/pulls.rs:455`) still returns 403 for a principal that can read
the repository but may not perform the action, and 404 for one that cannot read it at all.

Confirmed both directions:

- Legitimate merges still succeed for every role: owner (H1, C1–C4), maintainer (H2), writer
  (H3), admin (H4), and a base-repository owner merging a fork PR (H7). Browser merge (G14),
  protected-branch merge after approval + check (F11), and merge after a change request is
  cleared (D5) all succeed.
- Auto-merge still works for the owner (E4) and for a plain writer (E8), including the
  status-triggered wake-up (`auto_merge::on_status`).
- The ordering property itself holds: a non-member merging or enabling auto-merge on a
  **merged** pull request gets 404, not 409 (J1, J3); a reader gets 409 only because the same
  reader can already `GET` the pull request and see `state=merged` (J2, J4). On an **open**
  pull request a reader gets 403 (H5, C8) and a non-member 404 (H6, C9), so no state leaks
  through a 409 to an unreadable principal.

### 2. Reviewer UI did not change API behaviour

The browser routes (`pulls_ui.rs:1833` request, `:1883` remove) re-enter the same
`pulls::add_review_request` / `remove_review_request` handlers as the JSON API, with the
session token as a Bearer credential.

- API behaviour is unchanged: add returns the requested-reviewer list (200), duplicate add
  keeps one row (B11, J5), unknown reviewer is 422 `reviewer not found` (B10, G10), read-only
  user is 403 (B12), remove is 204 (B13), unrequested remove is 404 (B15), unauthenticated
  list is 401 (B14).
- Response shape is unchanged after a UI request: items still carry `reviewer_id`,
  `username`, `requested_by`, `created_at` (I3).
- UI request/remove are reflected in the API list and redirect back to the pull page
  (G8, G9); a duplicate UI request still leaves exactly one row (J5).

## Defects

None reproducible.

## Observations (non-blocking, no action required for M4 sign-off)

**O1 — one transient 500 on a merge (first pass only).** During the first pass, merging
`regr4ad#3` immediately after a change-request → approval cycle returned
`500 {"error":"storage operation failed"}` in 111 ms; the server logged
`ERROR repository read failed category=git repository_id=1896c046… (regr4ad)` from
`browse::map_storage_error` (`crates/githearth-server/src/browse.rs:179`). The same request
succeeded on immediate retry (201, merged), and the identical sequence passed 8/8 targeted
retries plus every subsequent run; it was the only 5xx logged across all passes.
The log records only `category=git`, not the `StorageError` variant, so the cause cannot be
pinned down after the fact — logging the error kind (without paths) would make a recurrence
diagnosable. No data inconsistency was left behind (no intent row, ref unchanged).

**O2 — NUL handling differs by surface, by design.** A NUL in a browser path renders the
styled 404 (G16), while a NUL repository name on `/api/v1` is rejected as
`400 {"error":"invalid input"}` (I5) by `valid_path_param` / the JSON rejection rewriter.
This matches "NUL paths → 404" for browse URLs and "JSON errors for /api extractor
rejections"; noted only because the two surfaces differ.

**O3 — unknown reviewer is 422, not 404** (`pulls.rs:2234`). Matches GitHub and the rest of
the API's invalid-input mapping; my first-pass expectation of 404 was wrong.

**O4 — self-approval is 403** via the `no-self-approval` policy
(`crates/githearth-authz/policies/builtin.cedar:135`). Correct, but it means the reviewer UI
must be exercised as a non-author (G13 uses `bob`).

## Results


### A. Pull request lifecycle

| Case | Check | Result | Evidence |
|---|---|---|---|
| A1 | create pull request returns 201 with open, non-draft fields | PASS | #2 head=094d2cb1 |
| A2 | duplicate pull request is 409 | PASS | an open pull request already exists for these branches |
| A3 | unknown source branch is 422 | PASS | branch not found |
| A4 | same source and target branch is 400 | PASS | invalid input |
| A5 | create pull request without credentials is 401 | PASS | 401 |
| A6 | read pull request exposes live head and base ids | PASS | head=094d2cb1 base=03336084 |
| A7 | list pull requests returns newest first and honours state filter | PASS | 2 open |
| A8 | draft pull request is created as a draft | PASS | #3 draft=True |
| A9 | draft endpoint flips ready-for-review and back | PASS | false -> true |
| A10 | patch title and body | PASS | Renamed a2 |
| A11 | read an unknown pull request is 404 | PASS | 404 |
| A12 | pull request detail carries code-owner review fields | PASS | satisfied=True owners=0 |

### B. Reviews, comments, threads, suggestions, reviewers

| Case | Check | Result | Evidence |
|---|---|---|---|
| B1 | approving review is recorded at the head commit | PASS | approvals=1 |
| B2 | changes-requested review is recorded | PASS | changes_requested=1 |
| B3 | invalid review verdict is 400 | PASS | invalid input |
| B4 | commented review with an empty body is 400 | PASS | 400 |
| B5 | create and list a pull request comment | PASS | comment 0cc544d6 |
| B6 | empty comment body is 400 | PASS | 400 |
| B7 | resolve and unresolve a comment thread | PASS | resolved -> unresolved |
| B8 | inline suggestion applies as a commit on the head branch | PASS | head moved 9dd4a16->4a8d003 |
| B9 | request a reviewer and list the request | PASS | 1 requested |
| B10 | requesting an unknown reviewer is 422 | PASS | 422 |
| B11 | duplicate reviewer request does not duplicate the row | PASS | 1 |
| B12 | a read-only user cannot request a reviewer | PASS | 403 |
| B13 | remove a reviewer request | PASS | removed |
| B14 | reviewer list requires credentials | PASS | 401 |
| B15 | removing an unrequested reviewer is 404 | PASS | 404 |

### C. Merge methods and merge errors

| Case | Check | Result | Evidence |
|---|---|---|---|
| C1 | merge method: merge commit lands and records merged_by/at | PASS | #1 merged 2026-11-14T09:50:26Z |
| C2 | merge method: squash produces one commit on main | PASS | squash commit: PR m-squash |
| C3 | merge method: rebase lands without a merge commit | PASS | tip d9378b59 |
| C4 | merge method: ff_only fast-forwards when possible | PASS | fast-forwarded |
| C5 | merge method: ff_only refuses a diverged branch with 409 | PASS | target is not an ancestor of the source |
| C6 | merge with a stale expected head is 409 | PASS | commit is not the pull request head |
| C7 | merging a closed pull request is 409 | PASS | pull request is not open |
| C8 | a read-only user cannot merge (403, not 404) | PASS | 403 |
| C9 | a non-member sees 404 on merge (no repository disclosure) | PASS | 404 |
| C10 | merge of an unknown pull request is 404 | PASS | 404 |
| C11 | merge idempotency key replays the same intent; reuse with a new body is 422 | PASS | replayed de0c2baf |
| C12 | invalid merge method is 400 | PASS | 400 |
| C13 | merging a draft pull request is 409 | PASS | pull request is a draft |

### D. Conflicts and change-request blocking

| Case | Check | Result | Evidence |
|---|---|---|---|
| D1 | conflicting pull request reports conflicts in mergeability | PASS | mergeability=['conflicts', 'conflicts', 'conflicts'] |
| D2 | conflicting merge is refused 409 and recorded as conflicted | PASS | merge conflicts: conflict.txt |
| D3 | repeated conflicting merge reuses the recorded conflict | PASS | same intent |
| D4 | an outstanding change request blocks the merge with 409 | PASS | the pull request has an outstanding change request |
| D5 | a later approval clears the change request and the merge succeeds | PASS | merged |
| D6 | pull page explains that a change request blocks the merge | PASS | explanation rendered |
| D7 | blocked merge control is disabled, not silently missing | PASS | blocked, no merge form |

### E. Auto-merge and queue

| Case | Check | Result | Evidence |
|---|---|---|---|
| E1 | enable auto-merge on a pull request blocked by a required check | PASS | auto-merge merge |
| E2 | read auto-merge state as a reader | PASS | readable |
| E3 | cancel auto-merge returns the cancelled state and clears it | PASS | cancelled |
| E4 | auto-merge completes when the required check succeeds | PASS | merged at 2026-11-14T09:50:50Z |
| E5 | enabling auto-merge on a merged pull request is 409 | PASS | pull request is not open |
| E6 | a reader cannot enable auto-merge (403) | PASS | 403 |
| E7 | invalid auto-merge method is 400 | PASS | 400 |
| E8 | a writer can enable auto-merge and it merges (merge-auth ordering) | PASS | writer auto-merge merged |
| E9 | enabling auto-merge on a draft is 409 | PASS | pull request is a draft |

### F. Branch and tag protection

| Case | Check | Result | Evidence |
|---|---|---|---|
| F1 | create a branch protection rule | PASS | 62bd0600 |
| F2 | a duplicate branch rule is 409 | PASS | 409 |
| F3 | an invalid branch pattern is 400 | PASS | 400 |
| F4 | list branch rules includes the dismiss-stale flag | PASS | flag present |
| F5 | patch a branch rule | PASS | updated |
| F6 | delete a branch rule | PASS | deleted |
| F7 | direct push to a protected branch is refused | PASS | ref.push refused |
| F8 | force push to a protected branch is refused | PASS | force push refused |
| F9 | deleting a protected branch is refused | PASS | delete refused |
| F10 | required approvals gate the merge with 403 | PASS | 403 |
| F11 | after approval and a passing check the protected merge succeeds | PASS | merged under protection |
| F12 | create a tag protection rule | PASS | e6c09b1e |
| F13 | list tag protection rules | PASS | 1 rules |
| F14 | a write user cannot create a protected tag | PASS | tag refused |
| F15 | an admin can create a protected tag | PASS | tag v1.1 pushed |
| F16 | delete the tag protection rule | PASS | deleted |
| F17 | after the rule is gone a write user can create the tag | PASS | tag v2.0 pushed |

### G. Browser UI (settings, reviewers, comment redirect)

| Case | Check | Result | Evidence |
|---|---|---|---|
| G1 | branch settings page renders the rules table and create form | PASS | 200 |
| G2 | create a branch rule through the browser form | PASS | /organizations/lab05/repositories/regr4dg/settings/branches |
| G3 | delete a branch rule through the browser form | PASS | deleted |
| G4 | branch settings form without CSRF is refused | PASS | 403 |
| G5 | a reader sees the rules but no create form | PASS | read-only view |
| G6 | an invalid rule submitted from the browser keeps the form and shows 400 | PASS | 400 with preserved input |
| G7 | pull page renders the reviewer request control | PASS | form rendered |
| G8 | request a reviewer through the browser form | PASS | requested via UI |
| G9 | remove a reviewer through the browser form | PASS | removed via UI |
| G10 | an unknown reviewer through the UI renders an error page | PASS | 404 page |
| G11 | a comment submitted in the browser redirects to the pull page | PASS | /organizations/lab05/repositories/regr4dg/pulls/1 |
| G12 | the comment redirect points at the pull request itself | PASS | /organizations/lab05/repositories/regr4dg/pulls/1 |
| G13 | a review submitted in the browser redirects to the pull page | PASS | review recorded |
| G14 | merge through the browser redirects and the page shows merged-by/time | PASS | merged via UI |
| G15 | an invalid comment in the browser renders 400, not a 503 | PASS | 400 page |
| G16 | a NUL path in the browser renders the styled 404 | PASS | 404 page |
| G17 | the tag settings page renders and enforces CSRF | PASS | 200 + csrf enforced |

### H. Merge authorization ordering

| Case | Check | Result | Evidence |
|---|---|---|---|
| H1 | owner merge still works after the read-before-state change | PASS | merged |
| H2 | maintainer merge still works | PASS | merged |
| H3 | writer merge still works | PASS | merged |
| H4 | admin merge still works | PASS | merged |
| H5 | reader merge is 403, not a state leak through 409 | PASS | 403 |
| H6 | non-member merge is 404 | PASS | 404 |
| H7 | a fork pull request merges into the parent | PASS | fork PR #7 merged |
| H8 | re-merging a merged pull request is 409, not 404 | PASS | pull request is not open |
| H9 | draft merge by a writer is 409 (readable, not 404) | PASS | pull request is a draft |

### I. API consistency

| Case | Check | Result | Evidence |
|---|---|---|---|
| I1 | an unknown /api path returns a JSON 404 | PASS | 404 |
| I2 | a malformed path id returns the JSON envelope | PASS | json 400 |
| I3 | reviewer API shape is unchanged after the UI flow | PASS | keys=['created_at', 'requested_by', 'reviewer_id', 'username'] |
| I4 | API responses keep the nosniff header | PASS | nosniff |
| I5 | a NUL path in the API is a JSON 400, not a database 503 | PASS | 400 |

### J. Read-before-state and UI idempotency

| Case | Check | Result | Evidence |
|---|---|---|---|
| J1 | non-member merging a merged pull request gets 404, not a 409 state leak | PASS | 404 |
| J2 | a reader sees the same merged-state answer it can already read (409, never 403-leak) | PASS | 409, state already readable |
| J3 | non-member enabling auto-merge on a merged pull request gets 404 | PASS | 404 |
| J4 | a reader gets the same merged-state answer on auto-merge (409) | PASS | 409, state already readable |
| J5 | requesting the same reviewer twice through the UI keeps one row | PASS | 1 row |


## Notes

- Protection enforcement was verified end-to-end through Git Smart HTTP, not just the API:
  direct push (F7), force push (F8), branch delete (F9) and protected-tag create (F14) are
  refused with the expected reasons, while an admin tag push succeeds (F15) and the same push
  succeeds after the rule is deleted (F17).
- Branch-settings page: rule table renders every flag including dismiss-stale and merge-queue
  (G2), multi-line status checks are split into `ci/a, ci/b` (G2), create/delete redirect with
  303 (G2, G3), missing CSRF is 403 (G4), a reader sees no create form (G5), and a rejected
  rule returns 400 with the submitted value preserved (G6).
- Comment redirect is exactly `…/pulls/{number}` (G11, G12); invalid comment bodies render
  the 400 page rather than a 503 (G15).
- Merged-by/time is stored and rendered (`merged_at`/`merged_by` in C1, "Merged by t05 at …"
  in G14); the fork page shows "Forked from lab05/regr4dh" and the fork PR merges (H7).
- Merge idempotency replays the stored intent for the same key (C11) and returns 422 when the
  key is reused with a different body; a repeated conflicting merge reuses the recorded
  conflicted intent instead of duplicating it (D3).
- No source files in the GitHearth working tree were modified and nothing was posted to
  GitHub. Read-only SQL was used to confirm accounts, memberships and repository ids; no
  writes. Test repositories and branches were created only under `lab05`.
