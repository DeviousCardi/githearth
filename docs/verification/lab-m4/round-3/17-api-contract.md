# API contract — M4 closure round 3

Tester: t17. Date: 2026-11-14. Instance: main 800cad4.
Verdict: PASS WITH DEFECTS
Cases: 141 run, 136 pass, 5 fail, 0 skipped. (Each failure re-run twice; identical results.)

Fixtures were created from scratch in `lab17` as `r3-run-<suffix>` (public,
5 branches, issues 1–5 and PRs 6–11, one same-repo PR per branch, PR 11 opened
from a fork), plus a private repo and two extra forks for negative/visibility
checks. "Target list" below means `GET /api/v1/organizations/lab17/repositories/r3-run-<suffix>`.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A1 | GET /organizations status+array | 200, JSON array | 200 list application/json | PASS |
| A2 | org item shape | id,slug,name,role | ['id', 'name', 'role', 'slug'] | PASS |
| A3 | GET repositories status+array | 200, JSON array | 200 list | PASS |
| A4 | repository item shape | 8 documented keys | ['archived', 'description', 'id', 'name', 'organization_id', 'parent_repository_id', 'status', 'v... | PASS |
| A5 | repo read 200 + same shape | 200, documented keys | 200 | PASS |
| A6 | issue list envelope | {items:[],next} | 200 keys=['items', 'next'] | PASS |
| A7 | issue item shape | documented keys | ['author_id', 'body', 'closed_at', 'created_at', 'id', 'number', 'state', 'title', 'updated_at'] | PASS |
| A8 | issue read 200 shape | 200 with number/title/state | 200 ['author_id', 'body', 'closed_at', 'created_at', 'id', 'number', 'state', 'title', 'updated_at'] | PASS |
| A9 | pull list envelope | {items:[],next} | 200 keys=['items', 'next'] | PASS |
| A10 | pull list item shape (no live head) | documented keys, no head_id/base_id | ['author_id', 'body', 'closed_at', 'closed_reason', 'created_at', 'draft', 'id', 'number', 'sourc... | PASS |
| A11 | pull read 200 detail shape | 200 with head_id/base_id/mergeability | 200 ['approvals', 'author_id', 'base_id', 'body', 'changes_requested', 'closed_at', 'closed_reaso... | PASS |
| A12 | comment list envelope+shape | {items,next}, documented keys | 200 item=['author_id', 'body', 'commit_id', 'created_at', 'id', 'in_reply_to', 'line', 'outdated'... | PASS |
| A13 | review list envelope+shape | {items,next}, documented keys | 200 | PASS |
| A14 | audit list envelope+shape | {items,next}, documented keys | 200 keys=['items', 'next'] | PASS |
| A15 | fork listing envelope | {parent,children,count} | 200 keys=['children', 'count', 'parent'] | PASS |
| A16 | protection list envelope | {generation,items} | 200 {'generation': 0, 'items': []} | PASS |
| A17 | notification list 200 array | 200, JSON array | 200 list | PASS |
| A18 | bob (write) can comment-review t17 PR | 201 | 201 {'id': '54189dd3-301b-4432-8aad-b8747264e348', 'seq': 234, 'reviewer_id': 'ebf6bceb-6238-49a4... | PASS |
| A19 | notification row shape after bob review | documented keys, read_at null | ['actor_username', 'created_at', 'id', 'kind', 'pull_request_id', 'read_at'] | PASS |
| A20 | mark read: 204, empty, no content-type | 204 empty, no body | 204 len=0 ct= | PASS |
| A21 | mark read is idempotent | 204 again | 204 | PASS |
| A22 | mark unknown notification | 404 JSON envelope | 404 {'error': 'notification not found'} | PASS |
| B1 | issues page1 limit=2 newest first | items [5,4], next=4 | nums=[5, 4] next=4 | PASS |
| B2 | issues page2 after=4 | items [3,2], next=2 | nums=[3, 2] next=2 | PASS |
| B3 | issues page3 tail | items [1], next null | nums=[1] next=None | PASS |
| B4 | issues empty page after lowest | items [], next null | {'items': [], 'next': None} | PASS |
| B5 | issues state=closed filter | [3,2,1] | nums=[3, 2, 1] next=None | PASS |
| B6-0 | issues limit=0 | 400 JSON envelope | 400 ct=application/json {'error': 'invalid input'} | PASS |
| B6-101 | issues limit=101 | 400 JSON envelope | 400 ct=application/json {'error': 'invalid input'} | PASS |
| B6--1 | issues limit=-1 | 400 JSON envelope | 400 ct=application/json {'error': 'invalid input'} | PASS |
| B6-100 | issues limit=100 | 200 JSON envelope | 200 ct=application/json {'items': [{'id': '0beffbad-ce7d-43d8-bec5-dcd03c552441', 'number': 5, 't... | PASS |
| B7 | pulls page1 | [11,10], next=10 | nums=[11, 10] next=10 | PASS |
| B8 | pulls page2 | [9,8], next=8 | nums=[9, 8] next=8 | PASS |
| B9 | pull cursor walk: unique, descending, complete | 6 distinct numbers 6-11, no repeats | seen=[11, 10, 9, 8, 7, 6] pages=3 | PASS |
| B10 | invalid pull state | 400 JSON envelope | 400 {'error': 'invalid input'} | PASS |
| B11 | comments page2 via after=seq | 2 items, next null, no overlap | p1=[1, 2] p2=[3, 4] | PASS |
| B12 | comments limit=0 | 400 JSON envelope | 400 {'error': 'invalid input'} | PASS |
| B12b | reviews page1 next is last seq | next == last item seq | next=232 seqs=[231, 232] | PASS |
| B12c | reviews page2 completes without overlap | 1 item, next null, no overlap | n2=1 next=None | PASS |
| B13 | audit page1/page2 seq cursor | next=last seq, page2 greater, no overlap | p1=[1, 2] next=2 p2=[3, 4] | PASS |
| B14 | audit limit bound 200 | 400 JSON envelope | 400 {'error': 'invalid input'} | PASS |
| B15 | repository id cursor pages | distinct, id ascending | p1=r3-api-fork2 p2=grace-fork | PASS |
| B16 | repository limit=0 | 400 JSON envelope | 400 {'error': 'invalid input'} | PASS |
| B17 | repository empty page after last id | 200 [] | 200 [] | PASS |
| B18 | repository malformed cursor | 400 JSON envelope | 400 ct=text/plain; charset=utf-8 Failed to deserialize query string: after: UUID parsing failed: ... | FAIL |
| B19 | unknown query field | 400 JSON envelope | 400 ct=text/plain; charset=utf-8 Failed to deserialize query string: bogus: unknown field `bogus`... | FAIL |
| B20 | malformed integer cursor | 400 JSON envelope | 400 ct=text/plain; charset=utf-8 Failed to deserialize query string: after: invalid digit found i... | FAIL |
| C1 | replay same key+body returns stored 201 | same status and body | 201/201 same=True | PASS |
| C2 | replay creates one row | one issue with that title | 1 | PASS |
| C3 | reuse key with different body | 422 JSON KEY_REUSED | 422 {'error': 'idempotency key reused with different request'} | PASS |
| C4 | same key+body, query string differs | replay 201 (hash is method+path+body) | 201/201 same=True | PASS |
| C5-0 | invalid key '' | 400 JSON invalid idempotency key | 400 {'error': 'invalid idempotency key'} | PASS |
| C5-1 | invalid key 'xxxxxxxxxx' | 400 JSON invalid idempotency key | 400 {'error': 'invalid idempotency key'} | PASS |
| C5-2 | invalid key 'has space' | 400 JSON invalid idempotency key | 400 {'error': 'invalid idempotency key'} | PASS |
| C5-3 | invalid key 'tab\tkey' | 400 JSON invalid idempotency key | 400 {'error': 'invalid idempotency key'} | PASS |
| C6 | same key, different actors are independent | 201 and 201 | 201/201 | PASS |
| C7 | failed request releases key for retry | 409 then 201 (no 422) | 409/201 {'id': 'a4948a9d-eeb0-4662-bddf-a520187c448d', 'organization_id': 'b0000000-0000 | PASS |
| C8 | 4 concurrent same key+body | one 201 at least, others 201/409, one id | codes=[201, 409, 409, 409] ids=1 | PASS |
| C9 | concurrent same-key rows | exactly 1 | 1 | PASS |
| C10 | concurrent same key, different bodies | one 201, others 409/422 | codes=[201, 409, 409, 409] | PASS |
| C11 | unkeyed duplicates are allowed | two 201, different numbers | 18/19 | PASS |
| C12 | fork replay same key+body | 201/201, same repo id | 201/201 | PASS |
| D1 | no credentials on API collection | 401 JSON envelope | 401 {'error': 'invalid or expired credentials'} | PASS |
| D2 | bad bearer token | 401 JSON envelope | 401 {'error': 'invalid or expired credentials'} | PASS |
| D3 | login wrong password | 401 JSON envelope | 401 {'error': 'invalid or expired credentials'} | PASS |
| D4 | malformed JSON body | 400 JSON envelope | 400 ct=application/json {"error":"invalid input"} | PASS |
| D5 | unknown body field | 400 JSON envelope (deny_unknown_fields) | 400 {'error': 'invalid input'} | PASS |
| D6 | missing required field | 400 JSON envelope | 400 {'error': 'invalid input'} | PASS |
| D7 | NUL in body text | 400 JSON invalid input, not 503 | 400 {'error': 'invalid input'} | PASS |
| D8 | NUL in path parameter | 400 JSON invalid input | 400 ct=application/json {"error":"invalid input"} | PASS |
| D9 | unknown org | 404 JSON generic body | 404 {'error': 'not found'} | PASS |
| D10 | unknown repo | 404 JSON 'not found' | 404 {'error': 'not found'} | PASS |
| D11 | unknown issue number | 404 JSON 'not found' | 404 {'error': 'not found'} | PASS |
| D12 | unknown pull number | 404 JSON 'not found' | 404 {'error': 'not found'} | PASS |
| D13 | foreign private repo is generic 404 | 404 'not found' on read+issues | 404/404 {'error': 'not found'} | PASS |
| D14 | read role cannot close an issue | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| D15 | read role cannot create repository | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| D16 | read role cannot approve | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| D17 | duplicate repository name | 409 JSON 'repository name already taken' | 409 {'error': 'repository name already taken'} | PASS |
| D18 | method not allowed on collection | 405 | 405 ct= body= | PASS |
| D19 | GET on POST-only login | 405 | 405 | PASS |
| D20 | unknown API route | 404 JSON for /api/v1 | 404 ct=text/html; charset=utf-8 | FAIL |
| E1 | cache-control no-store | no-store | no-store | PASS |
| E2 | x-content-type-options nosniff | nosniff | nosniff | PASS |
| E3 | referrer-policy same-origin | same-origin | same-origin | PASS |
| E4 | JSON content-type exact | application/json | application/json | PASS |
| E5 | health live 200 + no-store | 200 no-store | 200 no-store | PASS |
| E6 | HEAD health live | 200 | 200 | PASS |
| E7 | refs listing 200 JSON | 200 JSON | 200 application/json | PASS |
| E8 | public JSON API is session-only | 401 anonymously | 401 {'error': 'invalid or expired credentials'} | PASS |
| E9 | anonymous UI browse of public repo | 200 HTML | 200 text/html; charset=utf-8 | PASS |
| F1 | upstream parent is null | parent null | None | PASS |
| F2 | upstream children include the fork exactly once | fork id once, count matches | count=3 ids=3 | PASS |
| F3 | child row links to parent | parent_repository_id == upstream id | 1eb8d175-cb41-44fb-9a6c-44e3637ea81d | PASS |
| F4 | fork lists parent and no children | parent.id==upstream, count 0, children [] | parent=r3-run-7114fa count=0 | PASS |
| F5 | fork listing requires auth | 401 | 401 | PASS |
| F6 | fork into unknown org | 404 JSON | 404 {'error': 'not found'} | PASS |
| F7 | read role cannot fork (repository.create) | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| F8 | invalid fork name | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| F9 | unknown field in fork body | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| F10 | foreign private repo fork listing | 404 | 404 | PASS |
| G1 | target repo list has unique numbers | no duplicates | [11, 10, 9, 8, 7, 6] | PASS |
| G2 | cross-fork PR appears once in target list | 11 exactly once | [11, 10, 9, 8, 7, 6] | PASS |
| G3 | cross PR source_repository_id is the fork | fork id | be8dc1b6-6e0f-46f4-99e9-aaa12e1b8bc1 | PASS |
| G4 | fork list excludes outgoing rows | items [], next null | items=0 next=None | PASS |
| G5 | fork list stays one number sequence under paging | empty page, no bogus next | {'items': [], 'next': None} | PASS |
| G6 | fork UI shows outgoing PR link to target | 200, href .../r3-run-*/pulls/11 | 200 href=/organizations/lab17/repositories/r3-run-7114fa/pulls/11 | PASS |
| G7 | outgoing link resolves to the PR | 200 and title present | 200 | PASS |
| G8 | target UI list has no duplicate links/numbers | each number once | ['11', '10', '9', '8', '7', '6'] | PASS |
| G9 | target UI has no outgoing section | section absent | absent | PASS |
| H1 | API create rule | 201, fields echoed incl. checks list | 201 {'id': '0d2224ec-1589-4801-83d7-6cb1aaa82d23', 'repository_id': '1eb8d175-cb41-44fb-9a6c-44e3... | PASS |
| H2 | create bumps generation | generation +1 | 0->1 | PASS |
| H3 | UI page shows API-created rule | 200, pattern + checks in HTML | 200 | PASS |
| H4 | UI page carries CSRF + idempotency fields | hidden csrf & key | True | PASS |
| H5 | UI create redirects | 303 | 303 loc=/organizations/lab17/repositories/r3-run-7114fa/settings/branches | PASS |
| H6 | UI-created rule matches API fields | approvals 3, checks [ci/a,ci/b], deletion+signed true | {'id': 'efa49bc3-0523-491c-b516-9dcf028ba837', 'repository_id': '1eb8d175-cb41-44fb-9a6c-44e3637e... | PASS |
| H7 | UI form idempotency replay | 303 and still one release/* rule | 303 count=1 | PASS |
| H8 | duplicate rule API | 409 JSON | 409 {'error': 'protection rule already exists'} | PASS |
| H9 | duplicate rule UI | 409 page with duplicate message (not 503) | 409 | PASS |
| H10 | API invalid approvals | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| H11 | UI invalid approvals | 400 page, preserved input | 400 | PASS |
| H12 | UI invalid pattern | 400 page | 400 | PASS |
| H13 | UI wrong CSRF | 403 | 403 | PASS |
| H14 | read role cannot create rule | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| H15 | UI hides create form from read role | 200, no add-rule form | 200 form=False | PASS |
| H16 | API patch rule | 200 approvals 4 | 200 {'id': '0d2224ec-1589-4801-83d7-6cb1aaa82d23', 'repository_id': '1eb8d175-cb41-44fb-9a6c-44e3... | PASS |
| H17 | patch unknown field | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| H18 | UI delete rule | 303 | 303 | PASS |
| H19 | UI delete visible to API | release/* gone | gone | PASS |
| H20 | API delete returns 204 empty | 204 | 204 len=0 | PASS |
| H21 | API delete unknown rule | 404 JSON | 404 {'error': 'not found'} | PASS |
| H22 | malformed rule id | 400 JSON | 400 None | FAIL |
| I1 | read absent intent | 404 JSON | 404 {'error': 'not found'} | PASS |
| I2 | create invalid oid | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| I3 | create invalid method | 400 JSON | 400 {'error': 'invalid input'} | PASS |
| I4 | read role cannot queue merge | 403 JSON | 403 {'error': 'not permitted'} | PASS |
| I5 | create merge intent | 201 with intent shape | 201 keys=['actor_id', 'candidate_oid', 'checks_approvals_digest', 'created_at', 'expected_target_... | PASS |
| I5b | merge intent replay same key | 201, same operation_id | 201 | PASS |
| I5c | merge intent key, different body | 409/422, not a second intent | 422 {'error': 'idempotency key reused with different request'} | PASS |
| I6 | read the created intent | 200 same operation_id | 200 state=queued | PASS |
| I7 | cancel intent | 200 cancelled, 404 if already merged, 409 if executing | 200 {'operation_id': 'cb3992bf-2567-43df-b76a-3afc91bafc0f', 'repository_id': '1eb8d175-cb41-44fb... | PASS |

## Defects

### D1 — Query/path extractor rejections bypass the JSON error envelope
- Severity: minor
- Repro (authenticated as t17):
  - `GET /api/v1/organizations/lab17/repositories?after=not-a-uuid`
  - `GET /api/v1/organizations/lab17/repositories/r3-run-<s>/issues?bogus=1`
  - `GET /api/v1/organizations/lab17/repositories/r3-run-<s>/issues?after=abc`
  - `DELETE /api/v1/organizations/lab17/repositories/r3-run-<s>/protection/not-a-uuid`
- Expected: `400 application/json` with the normal envelope, e.g.
  `{"error":"invalid input"}`, the same shape every handler-level 400 returns.
- Actual: `400 text/plain; charset=utf-8` with axum/serde text:
  ```
  Failed to deserialize query string: after: invalid digit found in string
  Failed to deserialize query string: bogus: unknown field `bogus`, expected one of `state`, `limit`, `after`
  Failed to deserialize query string: after: UUID parsing failed: invalid character: found `n` at 0
  Invalid URL: Cannot parse `id` with value `not-a-uuid`: UUID parsing failed: ...
  ```
  The status code is right; the body and content type are not.
- Evidence: cases B18, B19, B20, H22. Reproduced on three consecutive fresh runs.
- Root cause: handlers extract `Query<...>` and typed `Path<(..., Uuid)>` directly
  and therefore use axum's default rejection instead of the project envelope:
  `issues.rs:299`, `pulls.rs:845`, `audit.rs:249`, `repository_api.rs:414` and
  `:822`, `protection.rs:492` and `:653`. Only `ApiError` renders JSON
  (`lib.rs:418-423`, constructors at `lib.rs:451-453`). No custom
  `rejection`/`FromRequest` mapping exists.
- Impact: a JSON client cannot parse all 400s uniformly. The leaked text names
  parameter names and types only.

### D2 — Unknown `/api/v1` paths return the HTML UI 404
- Severity: minor
- Repro: `GET /api/v1/does-not-exist` with a valid bearer token.
- Expected: `404 application/json` `{"error":"not found"}`, consistent with the
  generic tenant 404 (`repository_api.rs`, `issues.rs:35-37`).
- Actual: `404 text/html; charset=utf-8` with the full styled web page
  (`<!doctype html>...`).
- Evidence: case D20. Reproduced on three consecutive fresh runs.
- Root cause: the UI router, including its fallback, is merged into the JSON
  router (`lib.rs:270`), and `ui::router()` installs `.fallback(fallback_page)`
  (`ui.rs:1003`); `fallback_page` always returns `Html`
  (`ui.rs:242-256`).
- Impact: unknown API endpoints look like a web page to programmatic clients;
  only cosmetic, no wrong status.

## Notes

- No 5xx responses and no 2xx-with-error bodies were observed in any case.
- Collection envelopes intentionally differ, and clients must know them per
  collection: repositories and notifications return a bare JSON array with no
  `next`; issues/pulls/comments/reviews/audit return `{items,next}`; forks
  return `{parent,children,count}`; protection returns `{generation,items}`.
  Repository cursors are `id`-based (`after` = last id of the page, no `next`
  field); comments/reviews/audit use their `seq`; issues/pulls use `number`.
- Issues and pull requests share one per-repository number sequence: with
  issues 1–5, the first PR is 6. The target-repo PR list carried numbers 6–11
  with no duplicates; the cross-repository PR 11 appeared once and its
  `source_repository_id` was the fork.
- Read role can create issues and pull requests by design: `issue.create` and
  `pull_request.create` are in the schema `read` group
  (`githearth-authz/policies/githearth.cedarschema:147,153`). The 403 cases
  therefore use issue close, repository create, review approval and protection
  update, which the read/member roles do not grant.
- Idempotency: keys are scoped per `(actor_id, key)` and the hash is
  `method + path + body` (`idempotency.rs:47-53`), so a query string is not
  part of it (C4 replays). Replays return the stored status and body; a
  different body yields 422 `idempotency key reused with different request`;
  concurrent losers get 409 `idempotency key conflict`; a failed handler
  releases the key (fork 409 retry as 201); unkeyed duplicates are allowed.
- Notifications are session-only (an access token gets 403), return at most 50
  rows, support only `unread`, and have no cursor or `limit`
  (`notifications.rs:298`); passing `limit` is an extractor 400 (D1).
- Fork listing: upstream returns `parent:null` and its readable children with
  `parent_repository_id` set; the fork returns its parent and no children;
  the endpoint requires authentication (401 anonymously), a stranger on a
  private repo gets 404, and a read-role member cannot fork (403).
- The branch-protection page and the JSON API share the same
  `protection::create_rule/delete_rule` functions, so create, duplicate (409
  page vs 409 JSON), invalid approvals/pattern (400 page vs 400 JSON), patch,
  delete, CSRF (403) and permission filtering (create form hidden for a read
  role) all agreed; a replayed UI form with the same idempotency key created
  one rule. Only the malformed path UUID differed (D1).
- Merge intents: create returns 201 with the documented shape, a same-key
  replay returns the same `operation_id`, a different body under that key is
  rejected, read-role queueing is 403, and cancel returns 200 or 404 once the
  worker has already merged. `DELETE …/merge` ignores `Idempotency-Key`
  (`cancel_intent` passes `None`, `merge_intents.rs:897`), so a repeated cancel
  is a 404 rather than a replay; noted, not filed.
- Test mistakes corrected while running (not product defects): the first page
  of the default open issue list only holds the two open issues (closed ones
  need `state=all`); a query string is intentionally not part of the
  idempotency hash; a fixed idempotency key collided with an earlier run's
  stored hash; `bad..name` is a valid repository name (only `.` and `..` are
  not); and read/member roles do grant issue/PR creation and repository
  creation respectively.
