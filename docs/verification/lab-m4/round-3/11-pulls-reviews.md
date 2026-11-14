# Pull requests and reviews — M4 closure round 3

Tester: t11. Date: 2026-11-14. Instance: main 800cad4 (PRs #161 and #162 merged).
Verdict: PASS WITH DEFECTS (one minor, pre-existing defect: plain-text query-string errors).
Cases: 123 run, 121 pass, 2 fail, 0 skipped.

Scope: regression sweep of pulls and reviews after the M4 closure changes —
create/read/list, drafts, self-approval refusal, latest verdict per reviewer,
approvals at head, stale approvals (#136), outstanding change requests blocking
merge (409 with the detail count agreeing), inline comments (side/outdated),
replies, resolve/unresolve, suggestions, review requests, code-owner gate,
cross-repository PRs, deleted-fork PRs, query-string errors, pagination, and the
UI pages. Two failures are the same defect (D2) and were re-run once each.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | Create PR valid title/body/branches (rerun) | 201 with head_id/base_id/state/draft/approvals | 201 number=10 state=open head=True approvals=0 | PASS |
| 2 | Create PR whitespace-only title | 400 invalid input | 400 {'error': 'invalid input'} | PASS |
| 3 | Create PR title 257 chars | 400 | 400 | PASS |
| 4 | Create PR title 256 chars | 201 | 201 number=3 title_len=256 | PASS |
| 5 | Create PR same source/target branch | 400 | 400 {'error': 'invalid input'} | PASS |
| 6 | Create PR unknown source branch | 422 branch not found | 422 {'error': 'branch not found'} | PASS |
| 7 | Create PR unknown field | 400 | 400 {'error': 'invalid input'} | PASS |
| 8 | Create duplicate open PR for same pair | 409 | 409 an open pull request already exists for these branches | PASS |
| 9 | Create PR with draft=true | 201 draft=true | 201 number=4 draft=True | PASS |
| 10 | List default/open filter | only open PRs | 200 n=10 all_open=True | PASS |
| 11 | List state=closed filter (no closed yet) | 200 empty | 200 n=0 | PASS |
| 12 | List invalid state value | 400 | 400 | PASS |
| 13 | List limit bounds 0 and 101 | 400 / 400 | 400 / 400 | PASS |
| 14 | List pagination limit/after | descending, no overlap, next present | [10, 9, 8] next=8 then [7, 6, 5] | PASS |
| 15 | List after=abc error envelope | 400 JSON error envelope | 400 text/plain; charset=utf-8 body=Failed to deserialize query string: after: invalid digit found in string | FAIL |
| 16 | Read PR detail fields | 200 with head/base/approvals/statuses/mergeability x3/owners/errors | 200 head=True approvals=1 cr=0 m={'merge': 'clean', 'squash': 'clean', 'rebase': 'clean'} keys_ok=True | PASS |
| 17 | Read unknown PR number | 404 | 404 {'error': 'not found'} | PASS |
| 18 | List readable by read-grant (erin) | 200 | 200 | PASS |
| 19 | Close PR (#10) | 200 state=closed | 200 state=closed | PASS |
| 20 | Read-grant (erin) creates PR | 201 (ADR 0008) / 403 campaign | 201 number=6 | PASS |
| 21 | List state=closed after close | only closed | 200 n=1 states={'closed'} | PASS |
| 22 | Merge a draft PR | 409 pull request is a draft | 409 {'error': 'pull request is a draft'} | PASS |
| 23 | Request reviewer on a draft | 409 pull request is a draft | 409 {'error': 'pull request is a draft'} | PASS |
| 24 | Mark draft ready | 200 draft=false | 200 draft=False | PASS |
| 25 | Merge a ready PR to merged state | 201 then merged | 201 intent=queued final=merged | PASS |
| 26 | Convert open PR to draft then ready | 200 true then 200 false | 200/True then 200/False | PASS |
| 27 | Add review request and list it | 200 carol listed | 200 then 200 names=['carol'] | PASS |
| 28 | Duplicate review request | 200 one carol entry | 200 names=['carol'] | PASS |
| 29 | Request unknown reviewer | 422 reviewer not found | 422 {'error': 'reviewer not found'} | PASS |
| 30 | Request the PR author as reviewer | 422 author cannot review own PR | 422 {'error': 'the author cannot review their own pull request'} | PASS |
| 31 | Request non-approver (grace, member) | 422 reviewer cannot approve | 422 {'error': 'reviewer cannot approve this pull request'} | PASS |
| 32 | Remove review request | 204 | 204 | PASS |
| 33 | Remove missing review request | 404 | 404 {'error': 'not found'} | PASS |
| 34 | Carol requests changes | 201 changes_requested | 201 verdict=changes_requested commit=True | PASS |
| 35 | Detail shows outstanding change request | changes_requested=1 approvals=0 | 200 cr=1 approvals=0 | PASS |
| 36 | Self-approval forbidden (author) | 403 not permitted | 403 {'error': 'not permitted'} | PASS |
| 37 | Read-grant (erin) approves | 403 not permitted | 403 {'error': 'not permitted'} | PASS |
| 38 | Comment-only review without body | 400 | 400 {'error': 'invalid input'} | PASS |
| 39 | Comment-only review with body | 201 | 201 verdict=commented | PASS |
| 40 | Invalid review verdict | 400 | 400 {'error': 'invalid input'} | PASS |
| 41 | Review at a commit that is not the head | 409 commit is not the pull request head | 409 {'error': 'commit is not the pull request head'} | PASS |
| 42 | Review list append-only with commit_id | all verdicts present in order | 200 n=3 verdicts=['approved', 'changes_requested', 'commented'] commit_ids_ok=True | PASS |
| 43 | Latest verdict per reviewer wins | carol approved; approvals=1 CR=0 | 201/approved detail approvals=1 cr=0 | PASS |
| 44 | Review on a closed PR | 409 pull request is not open | 409 {'error': 'pull request is not open'} | PASS |
| 45 | Review on a merged PR | 409 pull request is not open | 409 {'error': 'pull request is not open'} | PASS |
| 46 | Change request recorded on #2 | 201; detail changes_requested=1 | 201/changes_requested detail cr=1 | PASS |
| 47 | Outstanding change request blocks merge | 409 outstanding change request; detail count 1 | 409 the pull request has an outstanding change request detail_cr=1 | PASS |
| 48 | Approval clears change request and merge proceeds | approvals=1 CR=0; 201; merged | approve=201 approvals=1 cr=0 merge=201 state=merged | PASS |
| 49 | #136 stale approval counts when dismiss=false | head moves; detail approvals=1; merge 201 merged | approve=201 old=fda39667 new=e25ae6cb moved=True detail approvals=1 merge=201 final=merged | PASS |
| 50 | Stale approval dismissed when dismiss=true | head moves; detail approvals=0; merge 403 | approve=201 old=6658e061 new=82f27802 moved=True detail approvals=0 merge=403 {'error': 'not permitted'} | PASS |
| 51 | Create a general comment | 201 no anchor | 201 path=None commit=None | PASS |
| 52 | Create anchored comment (new side) | 201 path/line/side/commit_id set | 201 path=app.txt line=3 side=new commit=True | PASS |
| 53 | Create anchored comment (old side) | 201 side=old | 201 side=old line=2 | PASS |
| 54 | Anchored comment at line 0 | 400 | 400 {'error': 'invalid input'} | PASS |
| 55 | Anchored comment past EOF | 400 line out of range | 400 {'error': 'line out of range'} | PASS |
| 56 | Anchored comment unknown path | 400 path not found at commit | 400 {'error': 'path not found at commit'} | PASS |
| 57 | Anchored comment invalid side | 400 | 400 {'error': 'invalid input'} | PASS |
| 58 | Anchored path without commit | 400 | 400 {'error': 'invalid input'} | PASS |
| 59 | Anchored comment at non-head commit | 409 commit is not the pull request head | 409 {'error': 'commit is not the pull request head'} | PASS |
| 60 | Reply to a thread root | 201 in_reply_to=root | 201 in_reply_to=True | PASS |
| 61 | Reply to a reply | 422 replies attach to the thread root | 422 {'error': 'replies attach to the thread root'} | PASS |
| 62 | Reply to unknown comment | 400 | 400 {'error': 'invalid input'} | PASS |
| 63 | Resolve a thread | 200 resolved=true | 200 resolved=True | PASS |
| 64 | Reply inherits root resolution | reply resolved=true | n=1 resolved=True | PASS |
| 65 | Unresolve a thread | 200 resolved=false | 200 resolved=False | PASS |
| 66 | Read-grant (erin) resolves a thread | 403 | 403 {'error': 'not permitted'} | PASS |
| 67 | Resolve an unknown comment | 404 | 404 {'error': 'not found'} | PASS |
| 68 | Comment list pagination | item limit and next cursor | 200 n1=[1, 2] next=2 n2=[3, 4] | PASS |
| 69 | Anchored comments become outdated after a push | anchored outdated=true, general false | head_moved=True anchored=[True, True] general=[False, False] | PASS |
| 70 | Comment on a closed PR | 201 (documented) | 201 | PASS |
| 71 | Create a suggestion comment | 201 suggestion=BETA\n | 201 suggestion='BETA\n' | PASS |
| 72 | Apply suggestion as read grant (erin) | 403 | 403 {'error': 'not permitted'} | PASS |
| 73 | Apply a suggestion | 200 with new commit_id | 200 commit=9c9c17bf changed=True | PASS |
| 74 | Applied suggestion changes branch content | line 2 becomes BETA | 200 text='alpha\nBETA\ngamma\n' | PASS |
| 75 | Re-apply an outdated suggestion | 409/422 refused | 409 {'error': 'commit is not the pull request head'} | PASS |
| 76 | Apply a comment with no suggestion | 422 comment has no suggestion | 422 {'error': 'comment has no suggestion'} | PASS |
| 77 | Old-side suggestion hidden and refused | suggestion=null; apply 422 base side | sug=None apply=422 {'error': 'a suggestion cannot apply to the base side'} | PASS |
| 78 | Apply an unanchored suggestion block | 422 not anchored | 422 {'error': 'comment is not anchored to a line'} | PASS |
| 79 | Code-owner requirement in detail | required owners include @carol; not satisfied | owners=[{'name': '@carol', 'approved': False}] satisfied=False | PASS |
| 80 | Non-owner approval does not satisfy code-owner gate | detail still false; merge 403 | approve=201 satisfied=False merge=403 not permitted | PASS |
| 81 | Matching owner approval satisfies gate and merges | detail satisfied true; merge 201 merged | approve=201 satisfied=True merge=201 final=merged | PASS |
| 82 | Create response reports real code-owner facts (#157) | required owners [@carol]; satisfied false; no errors | 201 number=13 owners=[{'name': '@carol', 'approved': False}] satisfied=False errors=[] | PASS |
| 83 | PATCH response reports real code-owner facts (#157) | required owners [@carol]; satisfied false | 200 title=round3 co2 patched owners=[{'name': '@carol', 'approved': False}] satisfied=False | PASS |
| 84 | Fork listing exposes parent (#156) | parent id = r3-pulls | 200 parent=r3-pulls children=[] | PASS |
| 85 | Create cross-repository PR | 201 head from fork tip | 201 number=14 head=fc467c44bde911528ff155cdf90d01b4b10d304f source_repo=81fe9856-c5fb-4aed-b043-831ef306209d | PASS |
| 86 | Read live cross-repo PR detail | 200 with mergeability | 200 head=fc467c44bde911528ff155cdf90d01b4b10d304f m={'merge': 'clean', 'squash': 'clean', 'rebase': 'clean'} | PASS |
| 87 | Review a cross-repo PR at the fork head | 201 approved | 201 verdict=approved | PASS |
| 88 | Deleted-fork PR reads closed (#134) | delete 204; detail 200 closed head_id null closed_reason source repository deleted | del=204 detail=200 state=closed head=None reason=source repository deleted | PASS |
| 89 | Deleted-fork PR UI page | 200, closed rendered | 200 has_closed=True unavailable=False | PASS |
| 90 | Cross-repo PR unknown source repository | 404 | 404 {'error': 'not found'} | PASS |
| 91 | Cross-repo PR malformed source spec | 400 | 400 {'error': 'invalid input'} | PASS |
| 92 | Fork API pull list excludes outgoing PRs (#156) | empty list (PR targets r3-pulls) | 200 n=0 | PASS |
| 93 | Query-string limit=abc | 400 JSON envelope | 400 text/plain; charset=utf-8 Failed to deserialize query string: limit: invalid digit found in stri | FAIL |
| 94 | Unknown query parameter | 400 or ignored | 400 Failed to deserialize query string: bogus: unknown field `bogus`, expe | PASS |
| 95 | Review list pagination | limit 2, next, no overlap | 200 n1=[127, 135] next=135 n2=[136, 137] | PASS |
| 96 | PR list page | 200 with rows | 200 rows=0 has_title=True | PASS |
| 97 | PR detail page | 200 merge box and review form | 200 merge=True review=True | PASS |
| 98 | Draft badge on detail and list | Draft badge present | detail=200 badge=True list=200 list_badge=True | PASS |
| 99 | Inline comment anchor rendered | app.txt and comment anchor | 200 path=True anchor=True | PASS |
| 100 | Resolve control rendered | resolve form present | 200 resolve=True | PASS |
| 101 | Comment form POST without CSRF | 403 | 403 | PASS |
| 102 | Comment form POST with valid CSRF | stored comment | post=200 before=4 after=5 found=True | PASS |
| 103 | Review form POST with valid CSRF | stored review; satisfied true | post=200 satisfied=True | PASS |
| 104 | Code-owner requirement rendered | Code owners and @carol | 200 owners=True carol=True | PASS |
| 105 | HTML escaping in PR list | no raw <script>alert | 200 raw=False escaped=True | PASS |
| 106 | Unknown PR UI page | 404 | 404 | PASS |
| 107 | PR list page after deleted-fork PR | 200, deleted PR row | 200 closed=True | PASS |
| 108 | Read grant (erin) cannot merge | 403 not permitted | 403 {'error': 'not permitted'} | PASS |
| 109 | PR create idempotent replay | 201 twice, same number | 201/17 then 201/17 | PASS |
| 110 | PATCH invalid state value | 400 | 400 {'error': 'invalid input'} | PASS |
| 111 | Stale review remains in append-only list | review commit_id != current head | 200 n=1 stale=[True] | PASS |
| 112 | Merge with invalid method | 400 | 400 {'error': 'invalid input'} | PASS |
| 113 | Merge with a stale expected head | 409 | 409 {'error': 'commit is not the pull request head'} | PASS |
| 114 | List state=merged filter | only merged | 200 n=6 states={'merged'} | PASS |
| 115 | PATCH title on a merged PR | 200 title updated | 200 title=round3 draft renamed | PASS |
| 116 | Read grant cannot remove review request | 403 | 200/403 {'error': 'not permitted'} | PASS |
| 117 | Draft detail page hides merge form | no /merge form, Draft badge | 200 merge=False draft=True | PASS |
| 118 | Change request blocks merge despite another approval | approvals=1 CR=1; merge 409 | CR=201 approvals=1 cr=1 merge=409 the pull request has an outstanding change request | PASS |
| 119 | Reviewer approval clears own change request | CR=0 approvals=2 | approve=201 approvals=2 cr=0 | PASS |
| 120 | NUL byte in reviewer name (#152) | 400 invalid input | 400 {'error': 'invalid input'} | PASS |
| 121 | NUL byte in review body (#152) | 400 invalid input | 400 {'error': 'invalid input'} | PASS |
| 122 | NUL byte in comment body (#152) | 400 invalid input | 400 {'error': 'invalid input'} | PASS |
| 123 | Cross-tenant private repo pull list (#153) | 404 generic not found | 404 {'error': 'not found'} | PASS |

## Defects

### D2 (minor, carried over from rounds 1–2) — query-string deserialization errors bypass the JSON error envelope

- **Severity: minor**
- **Repro** (as `t11`, bearer session token):
  ```bash
  GET /api/v1/organizations/lab11/repositories/r3-pulls/pulls?after=abc
  GET /api/v1/organizations/lab11/repositories/r3-pulls/pulls?limit=abc
  GET /api/v1/organizations/lab11/repositories/r3-pulls/pulls/1/reviews?after=abc
  GET /api/v1/organizations/lab11/repositories/r3-pulls/pulls/1/comments?after=abc
  ```
- **Expected:** `400` with the API's JSON envelope, e.g.
  `{"error":"invalid input"}` (every in-handler validation failure on these
  routes does this).
- **Actual:** `400`, `content-type: text/plain; charset=utf-8`, body
  `Failed to deserialize query string: after: invalid digit found in string`
  (or `limit: invalid digit found in string`, or `bogus: unknown field ...`).
  Re-run once per failure: same status, content type and body both times.
- **Evidence:** `after=abc` -> 400 text/plain; `limit=abc` -> 400 text/plain;
  `state=open&after=zz` -> 400 text/plain; the comments and reviews list
  endpoints behave identically. Status codes are correct; only the body and
  content type are inconsistent. Clients that parse JSON errors cannot handle
  the response.
- **Root cause:** the affected handlers take `Query<ListQuery>` /
  `Query<SeqQuery>` extractors (`crates/githearth-server/src/pulls.rs:846`,
  `:1668`, `:1920`), so Axum rejects the request before the handler runs and
  `ApiError`'s JSON mapping is never applied. `router()` in
  `crates/githearth-server/src/lib.rs:246-270` registers no
  `QueryRejection`-to-`ApiError` mapping. The same extractor pattern exists on
  other list routes; this report only asserts the pull, comment and review
  lists.
- **Scope:** systematic for query deserialization on these list routes
  (`after`, `limit`, unknown parameter). Not a data or authorization defect.

No other product defect was found. **D3 from rounds 1–2 is fixed:** create and
PATCH responses now report the real code-owner requirement
(`required_code_owners:[{name:"@carol",approved:false}]`,
`code_owner_review_satisfied:false`) instead of the neutral defaults, matching
the detail and the merge gate (cases 83–84). The fix is the
`apply_codeowner_review` call on the write paths
(`crates/githearth-server/src/pulls.rs:812`, `:1223`, commit `ca659c8`).

## Notes

- **Fixtures created.** Repository `lab11/r3-pulls` (public) with branches
  `feat-*`, `draft`, `co-target`, `stale-target`, `strict-target`; PRs #1–#17;
  protection rules on `stale-target` (`required_approvals:1`,
  `dismiss_stale_approvals:false`), `strict-target` (`required_approvals:1`,
  dismissal on) and `co-target` (`require_code_owner_review:true`,
  `CODEOWNERS: * @carol`). Fork `lab11/r3-fork` was created for the cross-repo
  cases and deleted for the #134 case. No repository file was edited and no
  container was restarted.
- **#136 (stale approvals) verified with a real head move.** For
  `dismiss_stale_approvals:false`, carol approved at `fda39667`, a new commit
  moved the head to `e25ae6cb`, the detail still reported `approvals:1`, and
  the merge was accepted (201) and reached `merged`. With dismissal on
  (`strict-target`), the same flow reported `approvals:0` after the head moved
  to `82f27802` and the merge was refused with 403. Both detail counts agreed
  with the gate.
- **Change requests block merge even with approvals.** On #2 carol's
  `changes_requested` produced detail `changes_requested:1`; the merge returned
  **409** `the pull request has an outstanding change request`, matching the
  detail count. On #1, with carol already approved (`approvals:1`), dave's
  change request still blocked the merge with the same 409; his follow-up
  approval cleared it (`approvals:2`, `changes_requested:0`).
- **Merge is asynchronous.** Merge endpoints return a 201 intent
  (`state:"queued"`); the worker applied it within ~1 s and the detail then read
  `merged`. All "merged" expectations were checked on the final state.
- **#157 fixed.** Create/PATCH code-owner reporting agrees with the detail and
  the gate (cases 83–84); read-side gate behaviour (non-owner approval 403,
  owner approval 201, `@carol` rendered on the page) also passes.
- **#156 fixed.** `GET .../forks` reports the parent (`r3-pulls`) and the fork's
  own pull list is empty for an outgoing cross-repo PR, so numbers and
  pagination stay on the target repository's sequence.
- **#134 / deleted fork.** After `DELETE lab11/r3-fork` (204), the cross-repo PR
  reads 200/closed with `head_id:null` and
  `closed_reason:"source repository deleted"`; the UI page is 200 and renders
  the closed state (no "Unavailable").
- **#152 / #153.** NUL in a reviewer name, review body and comment body all
  return 400; a cross-tenant read of private `beta/app` returns the generic 404
  `{"error":"not found"}`.
- **Documented behaviour, not defects.** erin (`read`) can create a pull
  request (201), matching ADR 0008's fork-contribution intent; comments on a
  closed PR are accepted (201), as on GitHub.
- **Test mistakes, re-run and corrected (not product defects).**
  - The first strict-target stale-approval attempt re-pushed an already-pushed
    branch, so the head never moved and the approval was not stale; re-ran with
    a new commit (`82f27802`) -> detail 0, merge 403.
  - The first valid-create case hit a 409 because an interrupted earlier setup
    run had already opened that branch pair; re-ran the create on a fresh branch
    (`feat-closed`) -> 201.
  - `acme/web` is readable by t11 in this fixture, so the tenant-isolation check
    used private `beta/app` instead.
- **Not exercised here:** merge queue, auto-merge, required status checks (other
  domains), and concurrent close/review races (covered by unit tests, not
  reproducible black-box in this run).
