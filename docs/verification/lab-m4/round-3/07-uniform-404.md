# Uniform tenant 404 / existence oracle (#153) — M4 closure round 3

Tester: t07. Date: 2026-11-14. Instance: main `800cad4`.
Verdict: **PASS WITH DEFECTS**
Cases: 307 run (295 API/UI + 12 Git HTTP), 302 pass, 5 fail, 0 skipped.

Issue #153's original three-body reproduction is fixed: every repository/issue/PR/
protection/fork/audit lookup I tested returns status 404 with the byte-identical
21-byte body `{"error":"not found"}` for existing-but-unreadable, missing, and
foreign targets, and with identical headers. Two endpoints in the merge-intent
and auto-merge family still answer before authorizing and leak private PR
existence/state (one defect, 5 failing probes).

## Results

States compared for every API family: `beta/app` and `lab08/t08-pulls`
(existing, unreadable), `lab07/no-such-r3` and `acme/no-such-r3` (missing repo
in an existing org), `no-such-org-r3/web` (missing org). "Foreign" existing
resources (beta issue #1, lab08 issue #42, lab08 PR #8/3/35) were also probed.

### Tenant-failure matrix (all expected 404, body `{"error":"not found"}`, 21 B)

| # | Case | Sub-cases | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | Repository read, PATCH, DELETE, archive, unarchive | 5 states × 5 routes = 25 | uniform 404 | 404 / 21 B / same headers; no creation or deletion occurred | PASS |
| 2 | Repository access list, grant, revoke | 5 × 3 = 15 | uniform 404 | 404 / 21 B; grant/revoke did not touch fixtures | PASS |
| 3 | Issues list, comments list, comment create | 5 × 3 = 15 | uniform 404 | 404 / 21 B | PASS |
| 4 | Existing foreign issues: beta/app#1, lab08/t08-pulls#42 (GET + PATCH) | 4 | 404 identical to a missing issue | 404 / 21 B for all four | PASS |
| 5 | Pulls list, comments, comment create/patch, reviews, review create, review requests list/add/remove | 5 × 9 = 45 | uniform 404 | 404 / 21 B | PASS |
| 6 | Existing foreign PR lab08/t08-pulls#8: GET, PATCH, comments, reviews, merge GET, merge DELETE | 6 | 404 identical to a missing PR | 404 / 21 B for all six | PASS |
| 7 | Merge intent: create on a **missing** PR, read, cancel | 5 × 3 = 15 | uniform 404 | 404 / 21 B | PASS |
| 8 | Auto-merge: enable on a **missing** PR, read, cancel | 5 × 3 = 15 | uniform 404 | 404 / 21 B | PASS |
| 9 | Branch protection list/create/patch/delete (random rule id) | 5 × 4 = 20 | uniform 404 | 404 / 21 B | PASS |
| 10 | Tag protection list/create/delete (random rule id) | 5 × 3 = 15 | uniform 404 | 404 / 21 B | PASS |
| 11 | Forks list and fork create | 5 × 2 = 10 | uniform 404 | 404 / 21 B; no fork created | PASS |
| 12 | Commit statuses read/post | 5 × 2 = 10 | uniform 404 | 404 / 21 B | PASS |
| 13 | Browse refs and tree | 5 × 2 = 10 | uniform 404 | 404 / 21 B | PASS |
| 14 | Audit and audit/verify | 3 orgs each = 6 | uniform 404 | 404 / 21 B | PASS |
| 15 | Org repository list and repository create | 3 orgs each = 6 | uniform 404 | 404 / 21 B; no repo created | PASS |
| 16 | Org read by UUID (own, readable acme, beta, lab08, random UUID) | 5 | 200/200/404/404/404 | exactly that; the three 404s are byte-identical | PASS |
| 17 | Legit 400/409/422 responses unchanged | 10 | see below | unchanged | PASS |
| 18 | Read-but-cannot-do stays 403, plus readable 200 controls | 13 + 11 = 24 | 403 for denials, 200 for readable controls | 403 / 25 B for all 13; 200 for all 11 controls | PASS |
| 19 | Anonymous API requests | 12 | uniform 401 | 401 / 42 B, one header set | PASS |
| 20 | Anonymous UI, plus 12 Git HTTP probes below | 7 | uniform sign-in/401 | see below | PASS |
| 21 | Cross-org / cross-repo IDOR sweep | 11 | uniform 404 | 404 / 21 B; fixture rows intact afterwards | PASS |
| 22 | Merge/auto-merge state oracle on unreadable PRs | 9 | uniform 404 | **5 × 409 with distinct messages** | **FAIL** |

Detail for the original #153 repro (now fixed):

```
GET /api/v1/organizations/beta/repositories/app            -> 404 {"error":"not found"}
GET /api/v1/organizations/beta/repositories/no-such-r3     -> 404 {"error":"not found"}
GET /api/v1/organizations/no-such-org-r3/repositories/app  -> 404 {"error":"not found"}
```

All three have `content-type: application/json`, `content-length: 21`,
`cache-control: no-store`, `x-content-type-options: nosniff`,
`referrer-policy: same-origin`; no other headers vary. The two old messages
(`organization not found`, `repository not found`) no longer appear on any of
the 222 tenant-failure probes.

### 403 remains for "can read but cannot do"

t07 is an `acme` member (read) and can read `acme/web` (private) and `acme/site`
(public) but has no write/admin grant.

| Case | Actual |
| --- | --- |
| PATCH / DELETE / archive / unarchive acme/web, access list/grant, issue PATCH, branch/tag protection create | `403 {"error":"not permitted"}` (8 cases) |
| POST repository create in acme | 403 |
| GET acme/audit and acme/audit/verify (member, not owner/admin) | 403 |
| POST merge and POST auto_merge on readable acme/web PR #1 | 403 |
| Readable GETs (repo, issues, PR #1, protection, tags, forks, refs, audit of own org, org repo list) | 200 (11 cases) |

The 403 body is the fixed 25-byte `{"error":"not permitted"}`; no names or ids.

### Legitimate 400 / 409 / 422 (unchanged)

| Case | Status / body |
| --- | --- |
| Invalid repo name | 400 `{"error":"invalid input"}` |
| Empty issue PATCH; invalid issue state | 400 `invalid input` (both) |
| Invalid grant role | 400 `invalid input` |
| Invalid merge method | 400 `invalid input` |
| Duplicate branch protection rule | 409 `protection rule already exists` |
| Duplicate tag protection rule | 409 `tag protection rule already exists` |
| PR with a missing base branch | 422 `branch not found` |
| Fork to an existing name | 409 `repository name already taken` |
| Unparseable UUID in a typed path segment | 400 plain-text extractor rejection |

### Anonymous

- 12 API GETs (repo/issue/PR/protection/forks/audit/refs plus org read by
  readable, unreadable and random UUID): all `401 {"error":"invalid or expired credentials"}`,
  42 B, identical headers, no `WWW-Authenticate` on the JSON API.
- UI: private repo, missing repo, foreign org repo, missing org, and missing org
  page all `303 Location: /login` with an empty body and identical headers;
  public `lab07/t07-public` and `lab01/t01-public` render 200 (intended).
- Git HTTP: anonymous `info/refs` for acme/web (private), beta/app,
  lab08/t08-pulls, acme/no-such-r3 and no-such-org-r3/web all return
  `401` with `WWW-Authenticate: Basic realm="githearth"` and the same 42-byte
  JSON; public repos return 200. With t07 credentials, beta/app,
  lab08/t08-pulls, acme/no-such-r3 and no-such-org-r3/web all return
  `404 {"error":"not found"}` (21 B), while t07's own repo returns 200.

### Cross-org / cross-repo IDOR sweep

All 404 `{"error":"not found"}` and no fixture was modified:

| Attempt | Result |
| --- | --- |
| lab08 protection-rule id PATCH/DELETE through t07's repo | 404/404 |
| t07's own branch-rule id PATCH/DELETE through a different t07 repo | 404/404 |
| t07's own tag-rule id DELETE through a different t07 repo | 404 |
| t07's own grant id DELETE through a different t07 repo / through beta/app | 404/404; own grant still listed afterwards |
| lab08 PR #8 and issue #42 numbers in t07's own repo | 404/404 |
| lab08 comment id on t07's own PR #2 | 404 |
| Unknown reviewer name on t07's own PR | 404 |

## Defects

### Merge and auto-merge answer before authorizing, leaking private PR existence/state

- Severity: **minor** (same class as #153: existence/state oracle, no content or
  names leak)
- Repro (t07 is not a member of `lab08`; `lab08/t08-pulls` is private):

  ```bash
  TOKEN=$(curl -s -X POST http://127.0.0.1:8080/api/v1/login \
    -H 'Content-Type: application/json' \
    -d '{"username":"t07","password":"<test-password>"}' \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

  # all against the unreadable foreign repo lab08/t08-pulls
  curl -s -o /dev/null -w '%{http_code}\n' -X POST -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"method":"merge","expected_head_oid":"0000000000000000000000000000000000000000"}' \
    http://127.0.0.1:8080/api/v1/organizations/lab08/repositories/t08-pulls/pulls/8/merge

  curl ... /pulls/999/merge   # nonexistent PR
  ```
- Expected: all four indistinguishable, `404 {"error":"not found"}`.
- Actual:

  | Request (t07 → lab08/t08-pulls) | Status / body |
  | --- | --- |
  | POST merge PR #8 (open, wrong head) | `409 {"error":"commit is not the pull request head"}` |
  | POST merge PR #8 with the real head | `404 {"error":"not found"}` |
  | POST merge PR #3 (merged) | `409 {"error":"pull request is not open"}` |
  | POST merge PR #35 (draft) | `409 {"error":"pull request is a draft"}` |
  | POST merge PR #999 (missing) | `404 {"error":"not found"}` |
  | POST auto_merge PR #3 (merged) | `409 {"error":"pull request is not open"}` |
  | POST auto_merge PR #35 (draft) | `409 {"error":"pull request is a draft"}` |
  | POST auto_merge PR #8 (open) / #999 | `404 / 404` |

  So an outsider can enumerate private PR numbers and learn whether each is
  open, closed/merged or draft, and (with the wrong-head probe) whether a
  supplied commit is the current head. Titles, bodies, authors and code are not
  exposed.
- Root cause: both handlers load the repository and PR and run state checks
  before calling the authorization helper:
  - `crates/githearth-server/src/merge_intents.rs:513-545` does `load_current`,
    `require_readable_repo` (status only), `find_pull`, `state`/`draft` checks
    and the `head_moved` check; `require_merge` is only reached at
    `merge_intents.rs:595`.
  - `crates/githearth-server/src/auto_merge.rs:224-245` does the same
    state/draft checks before `require_enable_permission` at
    `auto_merge.rs:249`.
  - `repository_api::load_repo` intentionally succeeds for a non-member (the
    denial is expected at `require`/`require_pull`), so `require_readable_repo`
    is not an authorization check.
- Suggested fix: after `find_pull`, call
  `pulls::require_pull(state, &loaded, &pull, Action::PullRequestRead)` (or the
  merge action) before the state/draft/head/branch probes, mirroring
  `merge_intents::read`/`cancel` and `auto_merge::read`/`cancel`. Then all
  denial paths return the shared `not_found()`, and a readable caller still
  gets 403 from the existing `read_ok` branch.
- Re-run: the five failing rows were re-run once (and the merged/draft rows a
  third time); identical 409/409/409/409/409 every time. The pass rows
  (correct head, missing PR) stayed 404.

## Notes

- Issue #153's tested scope is fixed in this build: the old three-body repro now
  yields one 21-byte body, and the repository routes cover read/update/delete/
  archive/unarchive/access (8 handlers) with identical status, body and headers.
  The existing tests assert this (`tests/repository_api.rs:450`,
  `not_found_bodies_do_not_reveal_existence`); there is no equivalent test for
  the merge-intent/auto-merge write paths, which is why this gap survived.
- **Timing (300 interleaved samples, keep-alive, localhost, medians):**
  unreadable repo `0.934 ms` vs missing repo in a readable org `0.901 ms` vs
  missing org `0.452 ms`; unreadable issue `1.044 ms` vs missing issue
  `0.961 ms` vs missing org `0.420 ms`; org read by UUID, unreadable `0.831 ms`
  vs missing `0.520 ms`. Unreadable vs missing *repository* is within noise
  (difference 0.03–0.08 ms; the distributions overlap around the median). A
  missing organisation is consistently ~0.4–0.5 ms faster because the facts
  snapshot is skipped. The audit endpoint was noisier in a separate 300-sample
  run (3.2 ms unreadable vs 1.7 ms missing org), with the same direction. This
  is a residual sub-millisecond timing signal for *organisation* existence
  only; it leaks no repository name or content and is not reliably observable
  over a network, so I record it as an observation rather than a defect.
- Test mistakes, corrected: `acme/web` is *readable* by t07 because t07 is an
  `acme` member (private repositories are readable inside the org), so it was
  moved from the unreadable set to the 403/200 set; `/api/v1/organizations/{id}`
  takes a UUID, not a slug (slug requests are a 400 extractor error, uniform
  for every value); 32-zero (unhyphenated) ids were rejected by the UUID
  extractor, so typed path probes use a valid nil UUID.
- Fixtures created for this round and left in place: commits on `main` and
  `feature` and PR #2 in `lab07/t07-second`, branch rule `main`, tag rule
  `v1.0.0`, one repository grant, plus one issue in `beta/app` (#1, as bob) and
  one in `lab08/t08-pulls` (#42, as t08) to test existing-but-unreadable issue
  reads. No repository was deleted, archived or modified by the failing
  probes; the own rule/tag/grant rows were verified intact after the IDOR sweep.
- No SQL text, internal UUIDs, repository names, issue/PR titles, file paths or
  user names appeared in any denial body or header across the 295 API/UI probes.
