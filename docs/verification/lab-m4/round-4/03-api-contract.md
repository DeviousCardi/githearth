# 03 — API contract consistency (round 4 final verification)

- Date: 2026-11-14
- Instance: `main` `cd7946b` (PR #163 merged), Compose quick start, HTTP `127.0.0.1:8080`
- Account: `t03` / `<test-password>`; organization `lab03` (owner)
- Secondary accounts used read-only: `bob` (granted `write` on one fixture repo), `t04` (stranger), `admin` (instance admin, not an org member)
- Scope: `merged_at`/`merged_by`, code-owner facts on every PR surface, error envelopes, unknown `/api` 404, pagination/idempotency, org/repo UI parity, `new` reserved, fork listing/outgoing PR shape, and a collection-wide contract smoke.

## Verdict

**PASS — no API-contract defects found.** 154 distinct cases, all pass on the final run. 14 cases failed on their first execution; every one was a harness/fixture error or an assertion written against the wrong documented behaviour, and each was corrected and re-run once (details in "Re-run log"). No product change is required for this domain.

## Counts

| Area | Cases | Pass | Fail |
| --- | ---: | ---: | ---: |
| A. `merged_at` / `merged_by` | 15 | 15 | 0 |
| B. Code-owner facts (create/PATCH/GET/draft/headless/fail-closed/approval) | 17 | 17 | 0 |
| C. Error envelopes (400/401/403/404/409/422, extractors, unknown `/api`, NUL) | 33 | 33 | 0 |
| D. Pagination / idempotency | 15 | 15 | 0 |
| E. Org/repo UI ↔ API parity | 17 | 17 | 0 |
| F. `new` reserved | 5 | 5 | 0 |
| G. Fork listing / outgoing PR shape | 14 | 14 | 0 |
| H. Collection contract smoke | 30 | 30 | 0 |
| X. Extra consistency probes | 8 | 8 | 0 |
| **Total** | **154** | **154** | **0** |

First executions: 139 pass / 14 fail across the 153 cases run at that point; one extra case (C33) was added while verifying C32 and passes. After correction and one re-run each: 154/154. See "Re-run log".

## Results

### A. `merged_at` / `merged_by` (15/15)

Every PR JSON (`create`, `GET`, list items, PATCH, draft response) carries both keys; they are `null` while open or closed-unmerged, and set after a merge.

| Case | Result | Evidence |
| --- | --- | --- |
| A1 open PR has both keys as `null` | pass | `merged_at:null, merged_by:null` |
| A2 merged PR has an ISO-8601 `Z` timestamp | pass | PR #1 `merged 2026-11-14T09:37:40Z` |
| A3 `merged_by` is the merging account | pass | PR #1 `merged_by=df56…` = `t03` |
| A4 `merged_by` is the actor, not the author | pass | PR #8 author `t03`, `merged_by=ebf6…` = `bob` (bob held a `write` grant) |
| A5 merged list item repeats the same values | pass | list item #8 matches detail |
| A6 closed-unmerged PR keeps both `null` | pass | `state=closed`, both `null` |
| A7 `merged_at` within `[created_at, now]` | pass | `09:37:32` ≤ `09:37:40` ≤ `09:41:46` |
| A8 stable across re-reads | pass | same values |
| A9 squash merge records both | pass | `t03r4-squash-c8b976` PR #1 `merged 09:45:38Z`, `merged_by=t03` |
| A10 reopening a merged PR is refused | pass | `409 {"error":"pull request is merged"}` |
| A11 re-merging a merged PR | pass | `409 {"error":"pull request is not open"}` |
| A12 pull page shows merged-by and merged-at | pass | “Merged by bob at 2026-11-14T09:40:22Z” |
| A13 page merged-by username is the actor | pass | “Merged by t03” |
| A14 default open list excludes merged, keeps the fields | pass | 10 open items at the final read, all with both keys |
| A15 every merged list item has both set | pass | 3 merged items (#1, #8, #16) |

### B. Code-owner facts (17/17)

`create`, `PATCH`, `GET`, `/draft`, headless PRs, unreadable/malformed `CODEOWNERS`, and an approval all agree or fail closed.

| Case | Result | Evidence |
| --- | --- | --- |
| B1 GET for a `src/` change | pass | `[{"name":"@alice","approved":false}]`, `satisfied=false`, `errors=[]` |
| B2 PATCH body edit returns the same facts | pass | identical triple |
| B3 create body equals a follow-up GET (draft) | pass | identical triple |
| B4 `/draft` response equals GET | pass | identical triple on draft→ready→draft |
| B5 headless PR (deleted head) | pass | `head_id=null`, `[], false, []` |
| B6 headless PR PATCH | pass | same |
| B7 unreadable `CODEOWNERS` (binary) GET | pass | `errors:["CODEOWNERS is not a text file"]`, `satisfied=false` |
| B8 same on PATCH | pass | identical error |
| B9 same on create | pass | `201` with `errors:["CODEOWNERS is not a text file"]` on a fresh branch |
| B10 repo without `CODEOWNERS` | pass | create and GET both `[], true, []` |
| B11 bob approves | pass | `201`, review at the head |
| B12 GET reflects the approval | pass | `[{"name":"@bob","approved":true}]`, `satisfied=true` |
| B13 PATCH agrees after approval | pass | same |
| B14 malformed owner token errors agree create/GET | pass | `["malformed owner token \`badtoken\`"]` |
| B15 same on PATCH | pass | identical |
| B16 deleted *base* branch | pass | `base_id=null`, `[], false, []`, no errors |
| B17 same on PATCH | pass | same |

Root behaviour: `pulls.rs:865-872` (create uses `review_loaded` on the same transaction), `pulls.rs:1104-1112` (GET), `pulls.rs:1466-1476` (PATCH), `pulls.rs:1283` (draft); fail-closed in `codeowners.rs:583` and unreadable handling at `codeowners.rs:322-360`.

### C. Error envelopes (33/33)

Every 400/401/403/404/409/422 sampled returns `application/json` with `{"error":"…"}`, including extractor rejections and unknown `/api` paths.

| Status | Cases | Evidence |
| --- | --- | --- |
| 400 | C1 malformed JSON, C2 bad query, C3 bad path id, C4 unknown field, C18/C29/C30 bad pagination params, C20 NUL in an API path, X5 NUL in a title, D9 bad idempotency key, F1–F5 reserved names | all `{"error":"invalid input"}` (or the specific message) |
| 401 | C5 no credentials, C6 bad token | `{"error":"invalid or expired credentials"}` |
| 403 | C7 acme member cannot create repos, C8 read-only token write, C32/C33 archived repo new content | `{"error":"not permitted"}` / `{"error":"read-only token"}` |
| 404 | C9 unknown repo, C10 unknown PR, C15/C22/C23/X2 unknown or nested `/api` (GET/POST/HEAD), C16 bare `/api`, C24 unknown org UUID, C25 non-member repo, C26/C27 stranger merge/auto-merge | `{"error":"not found"}` |
| 409 | C11 duplicate open PR, C12 repo name taken, A10/A11 merge state | specific messages |
| 422 | C13 idempotency key reused, C14 merge missing branch, C28 status context whitespace | specific messages |

Extractors: C2/C3/C29/C30 and C19 show the `text/plain` → JSON rewrite; C19 additionally verifies the rewritten 400 carries `x-frame-options: DENY`. Unknown `/api` paths are JSON for GET, POST and HEAD; non-`/api` paths get the styled HTML 404 (C17), and NUL in a browser path is a styled 404, not a 503 (C21). Root: `lib.rs:396-426` (`json_rejections`), `ui.rs:246-267` (`fallback_page`).

### D. Pagination / idempotency (15/15)

| Case | Result | Evidence |
| --- | --- | --- |
| D1 orgs `limit=1`/`after` | pass | `[acme] -> [lab03]` |
| D2 org limit 0/101 → 400 | pass | both 400 |
| D3 repos `limit=2`/`after` | pass | disjoint pairs, id-ordered |
| D4 pulls `limit=2`/`after` | pass | `[9,8] -> [4,3]`, `next` present |
| D5 issues `limit=2`/`after` | pass | `[7,6] -> [5]` |
| D6 repo create idempotent replay | pass | same id, 201/201 |
| D7 PR create idempotent replay | pass | same number, 201/201 |
| D8 merge idempotent replay | pass | same `operation_id`, stored 201 (`queued`) |
| D9 malformed key | pass | 400 JSON |
| D10 pull page shape | pass | `{items,next}` |
| D11–D15 bounds (`repos 101`, `issues 0`, bad state, access limit) | pass | all 400 JSON / honoured |

### E. Org/repo UI ↔ API parity (17/17)

| Case | Result | Evidence |
| --- | --- | --- |
| E1 `/organizations/new` form | pass | `slug`, `name`, `csrf_token` |
| E2 UI org create | pass | `303 → /organizations/<slug>`; API shows same `name`/`role=owner` |
| E3 reserved `new` | pass | 400 page |
| E4 duplicate slug | pass | 409 page |
| E5 repo form | pass | `name`, `description`, `visibility`, `idempotency_key` |
| E6 UI repo create | pass | 303; API read matches `description`/`visibility` |
| E7 reserved repo `new` | pass | 400 |
| E8 duplicate repo | pass | 409 |
| E9 invalid repo name | pass | 400 |
| E10 unauth page | pass | `303 → /login` (UI convention; the JSON API uses 401) |
| E11 bad CSRF | pass | 403 |
| E12 API also reserves `new` | pass | 400 |
| E13 invalid slug creates nothing | pass | 400 and absent from the API list |
| E14 invalid visibility creates nothing | pass | 400; API read 404 |
| E15 repo form replay | pass | 303/303, one row |
| E16 64-char slug cap | pass | API 400, UI 400 |
| E17 100-char repo name accepted | pass | 201 |

### F. `new` reserved (5/5)

Org slug `new` (API + UI), repo name `new` (API + UI), fork name `new`, `new.git`, `new-`, `New`, and PATCH rename→`new` are all 400 and leave state unchanged. `lib.rs:895` and `repository_api.rs:44-46`.

### G. Fork listing / outgoing PR shape (14/14)

| Case | Result | Evidence |
| --- | --- | --- |
| G1 parent listing | pass | `parent:null`, one child with `parent_repository_id` |
| G2 fork listing | pass | `parent` = contract, `children:[]`, `count:0` |
| G3 repo JSON | pass | `parent_repository_id` set, visibility inherited |
| G4 duplicate fork name | pass | 409 |
| G5 unknown repo | pass | 404 |
| G6 cross-repo PR | pass | `source_repository_id` = fork, `head_id` resolved |
| G7 outgoing row | pass | heading + `number/title/state/target` present |
| G8 outgoing link | pass | links to the parent PR |
| G9 unknown target org | pass | 404 |
| G10 fork `new` | pass | 400 |
| G11 fork of a fork | pass | parent = the intermediate fork |
| G12 count matches children | pass | `count=1` |
| G13 row fields | pass | title/state/target/number all present |
| G14 fork page parent link | pass | “Forked from …” |

`outgoing_pulls` filters each target by `RepositoryRead` (`pulls.rs:979-1050`), so no title/target leaks to a reader of the fork who cannot read the parent.

### H. Collection contract smoke (30/30)

`/health/live`, `/health/ready`, `/api/v1/me`, orgs (list/read), repos (list/read/access), forks, pulls (list/read/comments/reviews/requested-reviewers), issues (list/read/comments), branch protection, tag protection, statuses, refs, tree, blob (`?path=`), commits, commit, compare, audit, audit verify, notifications, tokens, SSH keys — all returned their documented 200 with the expected body type. (`/health/*` are empty 200s by design.)

## Defects

**None.** No case produced a wrong status, a wrong envelope, a missing field, a wrong value, or a divergence between create/PATCH/GET/draft/headless/UI/API.

## Observations (not defects)

1. **405 has no JSON envelope.** `POST /api/v1/me` → `405` with no content type or body (hyper default). 405 is outside the 400/401/403/404/409/422 set this round covers, so this is a note only.
2. **`GET /api/v1/organizations/{id}` takes a UUID.** A slug is a 400 extractor reject; a valid unknown UUID is a 404 (C24). Clients must not pass slugs to that route; the slug route is the nested `/organizations/{slug}/…` family.
3. **Archived repositories answer 403 `not permitted`, not 409 `repository is not active`.** New PRs and edits are refused by authorization first (`archived-no-new-content`, ADR 0002); 409 is for `creating`/`failed` rows. This is consistent with the ADR and with the repo-mutation tests.
4. **Idempotent replay returns the recorded 201 body.** Create PR #16 with key K, merge it, then replay K: the response still says `state:"open", merged_at:null` while `GET`/list show `merged` and the merge time. That is the standard "same key, same response" contract, but clients must re-GET to see merge state. Verified deliberately; not a defect.

## Re-run log (14 first-run failures, all harness/expectation)

| Case | First result | Cause | Correction / re-run |
| --- | --- | --- | --- |
| A6, B3 | 409 duplicate open PR | fixture branch already had an open PR | fresh branches; pass |
| C19 | missing `x-frame-options` | the request was a handler 400, not an extractor reject | i64-overflow path; pass with `DENY` |
| C20 | 401 | no auth on the request | auth added; 400 JSON; UI NUL 404 added as C21; pass |
| D8 | state `queued` ≠ `merged` | replay returns the stored 201 body | assert stored replay + same `operation_id`; pass |
| E2, E6 | 200, not 303 | `requests` followed the redirect | `allow_redirects=False`; pass |
| E10 | 200, not 401 | UI redirects to `/login` | assert 303 `/login`; pass |
| G7 | branch name absent | outgoing row shows number/title/state/target, not the branch | assert those; pass |
| H1, H2 | no JSON body | `/health/*` are empty 200s | allow empty for health; pass |
| H22 | 404 | blob path is a `?path=` query | corrected URL; pass |
| C24 | 400 | used a slug on the UUID route | random UUID → 404; pass |
| C32 | 403, not 409 | ADR 0002 archived-no-new-content | assert 403; C33 adds the PATCH variant; pass |

A full re-run of suite 1 (106 cases) and suite 2 (40 cases) after the corrections was green, plus a targeted re-run of C24 and C32 (with the C33 PATCH variant) before folding them back into suite 2. Suite 3 (8 cases) was green first time.

## Notes

- Fixtures are new repos in `lab03` prefixed `t03r4-` (`contract`, `target`, `failclosed`, `fork`, `fork2-*`, `co2-*`, `malco-*`, `arch-*`, `basegone-*`, `squash-*`, `idem-*`, UI-created `r4ui-*`). No repository was edited and nothing was posted to GitHub.
- Git fixtures were pushed over SSH (`127.0.0.1:2222`) with a key registered to `t03` via `POST /api/v1/user/ssh-keys`; merge execution was observed by polling `GET` until the state left `open` (the worker is asynchronous).
- The only SQL used was read-only (`docker compose exec -T postgres psql`) to look up account ids and the grant trigger; no writes.
- Test artifacts: `h.py`, `setup2.py`, `run.py` (106 cases), `run3.py` (40), `run4.py` (8), `results-r4*.json`.
