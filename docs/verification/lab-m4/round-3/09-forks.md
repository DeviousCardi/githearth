# Forks and cross-repository pull requests — M4 closure round 3

Tester: t09. Date: 2026-11-14. Instance: main 800cad4 (`GET /health/live` and
`/ready` 200). Verdict: **PASS**.
Cases: 83 run, 83 pass, 0 fail, 0 skipped (≈150 individual assertions).

Scope: issue #156 (fork listing parent/children/count, fork outgoing pull
requests, system-close reopen guard), #133 fork namespace 403 regression,
fork metadata/visibility/deletion, and #134 deleted-fork pull readability.

## Setup

- Owner of `lab09`; also used `erin` (read), `frank` (triage), `grace`
  (member), `carol`/`dave` (maintain), `alice` (acme owner, outsider), and
  `t10` (owner of `lab10`) for cross-org privacy checks.
- New fixtures: `lab09/t09r3-up` (public; `main`, `feature`, annotated tag
  `v1.0.0`), `lab09/t09r3-priv` (private), `lab09/t09r3-internal` (internal),
  `lab09/t09r3-empty` (empty public), `lab09/t09r3-sha` (sha256), plus scratch
  orgs `t09r3-org` (t09) and `t09r3-erin-org` (erin). Forks:
  `lab09/t09r3-fork`, `t09r3-org/t09r3-fork-org`, `lab09/t09r3-pfork`,
  `t09r3-erin-org/t09r3-pfork-erin`; reopen fixtures `t09r3-ro`…`ro10`.
- Cross-org privacy fixtures: `erin` forked private `lab09/t09r3-priv` into
  `t09r3-erin-org`; `t10` forked private `t10r3-org/t10r3-secret2` into
  `lab10/t10r3-secret2-fork`. `carol` can read `lab10` but not `t10r3-org`.
- Only reads/clones of other testers' data; no restart, migration, or
  repository edit. All created repositories carry the `t09r3-` prefix and the
  noisy probes were deleted afterwards; `t09r3-up`/`fork` and the two #134 PRs
  (`ro2` closed, `ro4` merged) are left as evidence.

## Results

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| 1 | `GET .../t09r3-up/forks` as owner | 200; `parent` null; readable children; count | 200; `parent:null`, children `[t09r3-fork, t09r3-fork-org]`, `count:2` | PASS |
| 2 | `GET .../t09r3-fork/forks` back-link | 200; parent object; own children only | 200; parent `t09r3-up` (full 8-key object); `count:0` at check time | PASS |
| 3 | Listing of the middle repo of a fork-of-fork | Parent = fork, child = grandchild | 200; parent `t09r3-fork`, child `t09r3-fork-of2`, `count:1` | PASS |
| 4 | Listing an empty repository | 200; `parent:null`, `children:[]`, `count:0` | 200; exactly that | PASS |
| 5 | Cross-org readable child | Child in `t09r3-org` listed for t09 | Child `t09r3-fork-org` listed, `count` included it | PASS |
| 6 | Unreadable private child is hidden | Carol (read on `t09r3-priv`, not on erin's org) sees only the lab09 fork and count 1; erin's fork name absent | 200; `count:1`, `children:[t09r3-pfork]`; raw body contains no `t09r3-pfork-erin` | PASS |
| 7 | Count is per-viewer | Erin sees 2 children; carol 1; alice (no access to private parent) 404 | 2 / 1 / 404; counts equal `children.len()` in each | PASS |
| 8 | Parent link hidden when parent is unreadable | Carol sees `parent:null` on `lab10/t10r3-secret2-fork`; t10 sees the parent | 200 `parent:null` for carol; 200 `parent:"t10r3-secret2"` for t10; no parent name in carol's body | PASS |
| 9 | Deleted child omitted from listing | Fork count and array drop the deleted child | Before 3 → delete 204 → 200 `count:2`, deleted name absent | PASS |
| 10 | Deleted parent | Child still 200; `parent:null`; `parent_repository_id` retained in repository JSON | 200 `{"parent":null,"count":0}`; repo JSON still carries the old parent UUID, `status:active` | PASS |
| 11 | Listing a deleted/nonexistent/unreadable repo | 404, same generic body | 404 for `t09r3-delparent`, `no-such-r3`, `beta/app`, and a private repo t09 cannot read | PASS |
| 12 | Listing auth matrix | Anonymous 401; `repo:read` token 200; token for unreadable repo 404; extra query ignored | 401; 200 with correct count; 404; `?foo=1` still 200 | PASS |
| 13 | Stability of listing | 20 consecutive reads 200 | 20/20 200 (private parent listing and outgoing page) | PASS |
| 14 | Public parent as a non-member | Alice sees public children in other orgs | 200, public children listed; `acme/web` count differs correctly per viewer: alice 0, carol 5, erin 7 | PASS |
| 15 | Archived parent | Listing still 200; fork of archived parent succeeds (policy unspecified) | 200; fork 201, `archived:false`, `status:active` | PASS |
| 16 | Fork pulls page shows outgoing PRs | Dedicated section; each linked to the target PR | 200; `<h2>Pull requests opened from this fork</h2>` with links `/organizations/lab09/repositories/t09r3-up/pulls/{1,2,3}` | PASS |
| 17 | Outgoing ordering and state labels | Newest first; current state shown | `#4 (closed), #3 open, #2 open, #1 merged`, newest first; merged/closed labels correct | PASS |
| 18 | JSON list is incoming-only | Fork with only outgoing PRs returns `items:[]` | `{"items":[],"next":null}` | PASS |
| 19 | JSON list with one incoming PR plus outgoing PRs | Only the incoming PR, its own number sequence | `{"nums":[1],"next":null}` while the page shows four outgoing PRs | PASS |
| 20 | JSON state filters do not mix in outgoing | `open/closed/merged/all` operate on target-only PRs | Fork: open `[1]`, closed `[]`, merged `[]`, all `[1]` | PASS |
| 21 | Same-repository PR is not outgoing | Appears in incoming table, not in the section | Fork PR #1 (`source_repository_id` = fork) in incoming table; absent from outgoing links | PASS |
| 22 | Unreadable target hidden in the section | Carol (reads fork in lab10, not target org) sees no section/title; direct PR 404 | 200 page with 0 occurrences of the heading and of `hidden-target contribution`; direct PR 404; JSON list empty | PASS |
| 23 | Deleted target hidden | Page 200, no section, no 500 | At `t10r3-secret-fork`: 200, heading absent, no target link; PR detail 404; JSON list empty | PASS |
| 24 | Cross-org target link | Link points at the target org/repo | `t10` page links `/organizations/t10r3-org/repositories/t10r3-secret/pulls/1` | PASS |
| 25 | No section on non-fork/empty repos | No outgoing card | `t09r3-up/pulls` and `t09r3-empty/pulls`: 200, heading count 0 | PASS |
| 26 | Public fork viewed by an outsider | Alice can read fork and public target, so she sees the section | 200; section present; links to the four target PRs; target PRs 200 | PASS |
| 27 | Title escaping | HTML in an outgoing title is escaped | Raw `<script>` count 0; `&lt;b&gt;bold&lt;/b&gt; &amp; …` rendered | PASS |
| 28 | Incoming pagination is unaffected | `limit=1`, `state=all` walks 3→2→1 with `next`; no duplicate/skip | `[3] next 3`, `[2] next 2`, `[1] next null`; outgoing PRs never appear | PASS |
| 29 | Pagination bounds | `limit=0/101` and bad state → 400; `after` non-integer → 400 | 400 for each; `after=999` → page 1 | PASS |
| 30 | Fork JSON pagination with incoming data | Single page, `next:null`, target numbers not injected | `{"nums":[1],"next":null}` | PASS |
| 31 | Normal close and reopen | 200 each; `closed_reason` cleared on reopen | close `closed`; reopen `state:open, closed_reason:null` | PASS |
| 32 | Reopen by read-only user | 403; no state change | Erin: 403 `not permitted`; state stayed closed | PASS |
| 33 | Reopen by plain member | 403 | Grace: 403 `not permitted` | PASS |
| 34 | Invalid reopen inputs | `state:"merged"` 400; anonymous 401; missing PR 404 | 400 / 401 / 404 | PASS |
| 35 | Reopen after the source branch is deleted | 409; state stays closed; head null | 409 `source repository or branch no longer exists`; GET `closed`, `closed_reason:null`, `head_id:null` | PASS |
| 36 | Reopen after the branch is restored | 200 and open again | 200 `{"state":"open","closed_reason":null}` | PASS |
| 37 | Delete a fork with an open cross-repo PR | 204; PR system-closed with reason and null head | 204; 200 `closed`, `closed_reason:"source repository deleted"`, `head_id:null` | PASS |
| 38 | Reopen that system-closed PR | 409; reason preserved | 409 `source repository or branch no longer exists`; GET still `closed`, same reason, head null (repeated twice) | PASS |
| 39 | Title-only and close-no-op edits on a system-closed PR | 200, reason not erased by a non-open transition | Title edit 200 with reason intact; `{"state":"closed"}` 200 with reason intact | PASS |
| 40 | Merge a system-closed PR | 409 not open (not 500/400) | 409 `pull request is not open` with `expected_head_oid`; a 400 without the required oid was a test mistake and re-ran correctly | PASS |
| 41 | Reopen a merged PR | 409 merged | 409 `pull request is merged` | PASS |
| 42 | Recreate the fork under the same name, then reopen | Still 409 (old fork id is deleted; no resurrection) | New fork id `cc3f9f18…` 201 with branch restored; reopen still 409; reason/head preserved | PASS |
| 43 | Reopen by triage/maintain/non-member/read token | 409 for roles allowed to close; 403 otherwise | Frank 409; Dave 409; Alice 403; `repo:read` token 403 `read-only token` | PASS |
| 44 | Stability of the deleted-fork PR | 20 consecutive reads 200 | 20/20 200 | PASS |
| 45 | Fork `acme/web` into `acme` as t09 (member, no create) | 403 not permitted | 403 `{"error":"not permitted"}` | PASS |
| 46 | Fork into `lab09` as erin (read, no create) | 403 | 403 not permitted | PASS |
| 47 | Fork into `lab09` as frank (triage, no create) | 403 | 403 not permitted | PASS |
| 48 | Fork into `lab09` as grace (member) | Member role has create in this policy; control normal create also 201 | Fork 201, and `POST .../repositories` as grace 201; no inconsistency | PASS |
| 49 | Unknown target organization | 404 generic | 404 `{"error":"not found"}` | PASS |
| 50 | Existing org without membership (`beta`, `t10r3-org`) | 404, existence not confirmed | 404 for both | PASS |
| 51 | Stranger (`alice`) into `lab09` | 404, not 403 | 404 | PASS |
| 52 | Unreadable parent | 404; no fork row created | `beta/app` 404; all denied names GET 404 afterwards | PASS |
| 53 | Browser fork, allowed | 303 to the new repository | 303 `/organizations/t09r3-org/repositories/t09r3-browser`; repo has parent link | PASS |
| 54 | Browser fork, read-only actor | 403 permission page | 403, `Fork failed … permission` | PASS |
| 55 | Browser fork, unknown namespace | 404 page | 404 | PASS |
| 56 | Browser fork, bad CSRF | 403 | 403 | PASS |
| 57 | Browser fork form namespaces | Only namespaces with create | Erin's form offers her two orgs, not `lab09` | PASS |
| 58 | Fork with read-only token | 403 `read-only token` | 403 | PASS |
| 59 | Fork response metadata | 8 keys incl `parent_repository_id`, `archived:false`, `status:active` | Exactly the 8 repository keys; parent id set; description empty; active/unarchived | PASS |
| 60 | Visibility inheritance | public→public, private→private, internal→internal | All three inherited exactly | PASS |
| 61 | Private parent forced private | Fork of `acme/web` is private | 201, `visibility:"private"`, parent `acme/web` | PASS |
| 62 | Description/archived metadata | Description not inherited (implementation choice); fork not archived | `description:""` even when parent had one; `archived:false` | PASS |
| 63 | Ref and HEAD copy | Fresh fork refs equal parent; HEAD follows parent default | `git ls-remote` identical (5 lines incl peeled tag); `HEAD → refs/heads/main` on both | PASS |
| 64 | Object format | sha256 parent yields sha256 fork with history | Fork clone `--show-object-format` = `sha256`, HEAD = parent commit | PASS |
| 65 | Omitted name and case-insensitive conflict | Name defaults to parent; same-org conflict 409; upper-case conflict 409 | Omitted name → `t09r3-up` 201 in another org; same org 409; `T09R3-META` 409 | PASS |
| 66 | Invalid fork payloads | 400 for each | Empty/whitespace org, slash/space/`.git` name, 101-char name, 1001-char description, unknown field, malformed JSON, empty body → all 400 | PASS |
| 67 | Length and NUL boundaries | 100-char name and 1000 multibyte description accepted; 101/1001 and real NUL rejected | 201 / 201; 400 / 400; NUL in description, organization and name 400 (an earlier 201 used an escaped literal, a test mistake, re-ran correctly) | PASS |
| 68 | Delete a fork | 204; GET 404; clone 404; second delete 404 | 204; 404; `repository not found` on clone; 404 | PASS |
| 69 | Delete a child | Parent listing count drops; parent refs unchanged | Count 9→8; upstream `ls-remote` hash identical before/after | PASS |
| 70 | Delete a fork that has its own fork | Grandchild remains readable; its parent link disappears | 200 child; listing `parent:null`; parent's own listing count 0 | PASS |
| 71 | Idempotent replay | Same key+body replays 201 with the same id | 201/201, identical id | PASS |
| 72 | Idempotency-key reuse with a different body | 422 | 422 `idempotency key reused with different request` | PASS |
| 73 | Concurrent same-name forks | One 201, rest 409 | `[201, 409, 409, 409, 409]` | PASS |
| 74 | Replay after deleting the fork | 201 cached response, same id, no resurrection | 201 same id; GET 404 | PASS |
| 75 | Delete authorization | Non-owner/read token 403; owner 204 | Erin 403, alice 403, `repo:read` 403; owner 204 | PASS |
| 76 | Fork of a deleted parent | 404 | 404 | PASS |
| 77 | Fork an empty public repo | 201; empty clone | `acme/site` fork 201, clone empty (0 commits) | PASS |
| 78 | #134: delete fork, then read its PR | 200 closed, reason, null head | 200 `{"state":"closed","closed_reason":"source repository deleted","head_id":null}` | PASS |
| 79 | #134: list after deletion | Closed PR listed with reason | `state=closed` returns the PR with `closed_reason:"source repository deleted"` | PASS |
| 80 | #134: comments and browser | Comments 200; page 200 showing `Head (deleted)` | Comments 200 with retained body; UI 200 with `Head (deleted)`; comment visible | PASS |
| 81 | #134: merged PR after deletion | Stays readable/merged; reopen refused | 200 `merged`; UI 200; reopen 409 `pull request is merged` | PASS |
| 82 | #134: outsider read | Alice sees the closed reason on a public parent | 200 closed/reason/head null; UI 200 `Head (deleted)`; comments 200 | PASS |
| 83 | #134: stability | 20 reads 200 | 20/20 200 | PASS |

## Defects

None found in the #156/#133/#134 scope.

Two transient `503 {"error":"git requests busy"}` responses occurred while
other testers were running heavy Git work (one PR detail read, one `git push`);
both succeeded on immediate retry (200/OK) and the server log shows no 5xx from
the fork routes. Recorded as environment contention, not a defect.

## Notes

- **Fork listing is API-only.** `GET .../forks` (forks.rs:25-30, 43-110) is not
  linked from the repository page: `ui.rs:852-860` still renders only the fork
  *form* card, with no parent/“forked from” link and no child list or count.
  This matches the shipped fix commit (“expose fork listing …”), whose tests
  cover the API only, but a browser user still cannot discover the fork graph.
  Not counted as a defect for this round; flagging as a residual UX gap.
- **Outgoing section limits.** `outgoing_pulls` fetches at most 200 rows and the
  UI asks for 20 (`pulls_ui.rs:356`), with no “more” link or pagination, and the
  section ignores the `state=` tab. Acceptable for the accepted requirement
  (separate, linked section); observations only.
- Reopen refusal is not atomic with Git refs: `branch_tip` is checked before the
  row update (pulls.rs:1150-1160), so a branch deleted in the narrow window
  would still reopen with a null head. No practical repro; noted as a residual
  TOCTOU risk, not observed.
- A system-close reason survives title-only edits and close-no-ops, which is
  correct: the `CASE` only clears it when the new state is `open`
  (pulls.rs:1164-1168).
- After fork deletion, a merged/closed PR’s `approvals` drops to 0 and its
  `head_id` is null; the PR, comments and reason remain readable. The browser
  shows `Head (deleted)` but does not print the `closed_reason` text.
- Fork description is empty rather than copied from the parent; ADR 0008 only
  requires visibility inheritance, so this is recorded as behaviour, not a bug.
- `member` (grace) can create repositories/fork into `lab09` while `read`
  (erin) and `triage` (frank) get 403; normal repository creation behaves the
  same way, so the 403 gate matches the system’s role ladder.
- No restart, migration, or edit of the repository was performed. Read-only SQL
  was used only to identify fixtures/roles. Test-created probes were deleted;
  `lab09/t09r3-up`, `t09r3-fork`, `t09r3-priv`, `t09r3-pfork`, `t09r3-ro2`
  (closed PR #1) and `t09r3-ro4` (merged PR #1) remain as evidence.
