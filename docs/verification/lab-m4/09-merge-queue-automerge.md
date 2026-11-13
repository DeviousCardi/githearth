# Merge methods, merge intents, merge queue and auto-merge — M4 lab acceptance report

Tester: t09. Date: 2026-11-14. Instance: http://127.0.0.1:8080 (main b820454).
Verdict: **FAIL** (one blocker; the rest of the merge surface largely works).
Cases: 80 run, 67 pass, 12 fail, 1 skipped.

Fixture: `lab09/t09-merge`, seeded as t09 with a linear `main`, 15 feature branches, a
`develop` target, two conflicting branches, and 150-commit clean/conflicting branches.
Roles exercised: t09 owner, dave maintain (admin grant), carol write (maintain grant),
erin read (triage grant), frank triage (member grant). No acme/beta setting was changed;
one read-only `GET` on acme PR #1 was used to confirm scope.

## Results

See the table below. Section keys: A merge methods, B authorization/state, C conflicts,
D merge queue, E auto-merge, F large-branch rebase, G concurrency/revalidation,
H worker/journal, I UI.

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| A1 | merge commit (two parents) | 201; merged; main has 2 parents [old main, head]; DB merged_by t09 | 201; merged; 2 parents; second==head; journal ref_update rows=1; pull_requests.merged_by=3b78a700-92ce-4c60-8153-dab0bbd0d11d | PASS |
| A1b | idempotent replay of a merge request (same Idempotency-Key) | 201 replay with the same operation_id | POST 201; same_operation=True | PASS |
| A2 | squash (single parent, content, message, author) | 201; merged; one parent = old main; content b.txt; title+body+Merged-by; author t09 | POST 201; intent=merged; parents=1; content='b'; msg='A2 squash merge\n\nsquash body line\n\nMerged-by: t09'; author=t09 <t09@users.noreply.githearth.invalid> | PASS |
| A3 | rebase (linear history, no merge commit) | 201; merged; linear replay, no merge commit | POST 500 {"error":"storage operation failed"} (re-run); GET pull also 500 storage operation failed; no ref move — git merge-tree --merge-base unsupported on git 2.39.5 | FAIL |
| A4 | ff_only allowed (fast-forward) | 201; intent merged; main ref == branch head (no new commit) | POST 201; intent=merged; main==branch_head=True; result=983cb4efbd025d9a33a860e546e725f32134fee3 | PASS |
| A5 | ff_only when not fast-forwardable | 409/422 at the API; no ref move | re-run POST 201; intent=stale (target is not an ancestor of the source); main unchanged=True — deterministic; ff_only is deliberately excluded from the synchronous mergeability check | FAIL |
| A6 | invalid merge method | 400 for unknown/case-wrong method | fast-forward=400 {"error":"invalid input"}; Merge=400 {"error":"invalid input"} | PASS |
| A7 | merge into a non-default branch | 201; merged; develop moves, main unchanged; two parents | 201; merged; origin/develop has 2 parents, second==feature-e head; main unchanged | PASS |
| A8 | Merge a PR whose head == base (empty branch) | either a synchronous 409/422 or a terminal async refusal; no ref move | POST 201; intent failed 'nothing to merge'; main unchanged=True | PASS |
| A9 | merge an already-merged PR (idempotency) | 409 not open / 409 pull request is merged; no new ref move | merge POST 409 {"error":"pull request is not open"}; PATCH state=closed 409 {"error":"pull request is merged"}; main unchanged=True | PASS |
| A10 | invalid expected_head_oid | 400 for short or uppercase oid | short=400; uppercase=400 | PASS |
| A11 | merge unknown PR number | 404 | 404 {"error":"merge intent not found"} | PASS |
| A12 | merge with a moved head (stale expected_head_oid) | 409 commit is not the pull request head; no ref move | POST 409 {"error":"commit is not the pull request head"}; main unchanged=True | PASS |
| A13 | merge body validation | 400 for unknown fields and missing expected_head_oid | unknown_field=400; missing_oid=400 | PASS |
| A14 | merge without authentication | 401 | 401 {"error":"invalid or expired credentials"} | PASS |
| A15 | squash collapses a multi-commit branch | 201; merged; exactly one first-parent commit; both files present | POST 201; intent=merged; first-parent commits added=1; parents=1; files=one,two | PASS |
| B1 | merge by owner | 201; merged | POST 201; intent=merged | PASS |
| B2 | merge by maintain | 201; merged; merged_by dave | POST 201; intent=merged; merged_by=565f9032-4e0d-40b2-a2b6-e4293bdd8d3a | PASS |
| B3 | merge by write | 201; merged | POST 201; intent=merged | PASS |
| B4 | merge by read | 403 not permitted; no ref move | POST 403 {"error":"not permitted"}; main unchanged=True | PASS |
| B5 | merge by triage/base member | 403 not permitted | POST 403 {"error":"not permitted"} | PASS |
| B6 | merge by member-only account | 403 | skipped: grace has an admin grant in lab09 (fixture differs from the brief's role table) — effective grant admin; not a member-only probe | SKIP |
| B7 | merge a draft PR | 409 pull request is a draft; after ready, merge works | draft merge=409 {"error":"pull request is a draft"}; ready=200; after ready merge=201 intent=merged | PASS |
| B8 | merge a closed PR | 409 pull request is not open; no ref move | close=200; merge=409 {"error":"pull request is not open"}; main unchanged=True | PASS |
| B9 | merge with a failing required check | refused 403/409; after success, merge works | merge with status failure=403 (refused); status success=201; merge=201 intent=merged | PASS |
| B10 | required approvals not met / met | 403 before approval; 201 merged after approval | merge before=403 {"error":"not permitted"}; review=201; merge after=201 intent=merged | PASS |
| B12 | merge with an outstanding change request | 409 refused; no ref move | review=201; merge=201; intent=merged; main moved=True — no change-request gate exists in merge_allowed/builtin.cedar (pulls.rs reports changes_requested but merge ignores it) | FAIL |
| B13 | approval stale after a new commit | 403 no approvals at the new head | review=201; push new head=e53e2d13ae7992fe284a870444b3caeb5db2d9eb; merge=403 {"error":"not permitted"} | PASS |
| B14 | merge a PR whose source branch was deleted | 422 branch not found; no ref move | delete=0; POST 422 {"error":"branch not found"}; main unchanged=True | PASS |
| B15 | cancel a queued merge as a read user | 403 not permitted; owner cancel works | POST 201; erin DELETE=403 {"error":"not permitted"}; t09 DELETE=200 {"operation_id":"1b1c9392-78e5-4d9c-a35d-d265b25035a3","repo | PASS |
| B16 | required check success on a different commit | 403 refused; success at the head then merges | merge with status only on main=403 {"error":"not permitted"}; after success at head=201 intent=merged | PASS |
| B17 | merge with a pending required check | 403/409 refused; success then merges | pending merge=403; after success=201 intent=merged | PASS |
| B18 | merge with an approval and an outstanding change request | 409 refused; no ref move | approve=201; changes_requested=201; merge=201; intent=merged; main moved=True — change requests are not part of the merge facts | FAIL |
| C1 | conflicting branches (same file), merge commit | 409/422; intent conflicted naming shared.txt; no ref move; mergeability conflicts | POST 409; intent=conflicted reason=merge conflicts: shared.txt; main unchanged=True; cache=conflicts:shared.txt | PASS |
| C2 | force-push resolve a conflict then merge | 201; merged; main moves | force head=6af81633356990ac562bf3d261ed11222f8bb3c1; POST 201; intent=merged; main moved=True | PASS |
| C3 | conflicting branches, squash | 409/422; intent conflicted; no ref move | POST 409; intent=conflicted reason=merge conflicts: shared.txt; main unchanged=True | PASS |
| C4 | conflicting branches, rebase | 409/422 conflicted | POST 500 {"error":"storage operation failed"}; no conflicted intent created — blocked by the rebase merge-tree defect (A3) | FAIL |
| C5 | conflict still detected independently of cached mergeability | 409 conflicted for the same head via another method | POST merge method=merge on same PR 409; intent=conflicted; main unchanged=True | PASS |
| D1 | enqueue a PR behind a merge queue | 201; intent in_queue=true, non-terminal | q1 POST 201; in_queue=True; state=queued | PASS |
| D2 | queue position for concurrent entries | three live entries have positions 1,2,3 (FIFO by creation) | live positions: 22\|1; 23\|2; 24\|3 | PASS |
| D3 | worker processes queued entries in order | all three merged, in creation order, content on main | states=['merged','merged','merged']; journal_sequence 115<117<119; content q1,q2,q3 | PASS |
| D4 | re-targeting when the target moves (queue) | later entries retarget their expected_target_oid and still merge | q1 result=3ce0fd3912bc47e5ae48cb117f4c828046d65b43; q2 expected=3ce0fd3912bc47e5ae48cb117f4c828046d65b43; q3 expected=b9b06a8980478ed30e8df4afa76d4e2c43cc12d9 | PASS |
| D5 | cancel a queued entry via API | DELETE 200 state cancelled; no merge | POST 201; DELETE 200 state=cancelled; final=cancelled; queue-4.txt on main=False | PASS |
| D6 | cancel a queued entry by closing the PR | close 200; intent cancelled; no merge | POST 201; close 200; intent=cancelled; q5.txt on main=False | PASS |
| D7 | queued merge blocked by a stale approval | intent stale after the source moves; no merge | review=201; POST 201; new head=810078acffffa773f1a8dc97de504ac48fa673b4; final=stale (source moved); merged=False | PASS |
| D8 | queue not required: intent is not a queue entry | 201; in_queue=false; merged | POST 201; in_queue=False; final=merged | PASS |
| D10 | branch-protection change stales a queued intent | intent stale 'branch protection changed'; no ref move | POST 201; PATCH rule=200; intent=stale (branch protection changed); main unchanged=True | PASS |
| E1 | enable auto-merge on a mergeable PR | 201; merges automatically; row consumed; completed audit | POST 201; state=merged after 1.1s; GET auto_merge=404; completed events=1 | PASS |
| E2 | disable auto-merge | enable 201; GET 200; DELETE 200; row gone; approval afterwards does not merge | enable=201; GET=200; DELETE=200; GET after=404; intents before approval=0; review=201; state=open | PASS |
| E3 | enable auto-merge with failing required checks | 201; waits (no intent); success check triggers merge | enable=201; status failure=201; intents while failing=0; GET=200; success=201; state=merged; GET after=404 | PASS |
| E4a | auto-merge on a permanently conflicting PR (#141, sync) | auto row cancelled; exactly one conflicted intent; repeated triggers create none | enable=201; after 3s GET auto_merge=404; conflicted intents=1; after triggers GET=404; conflicted=1 | PASS |
| E4b | auto-merge, asynchronously discovered conflict (#141) | auto-merge row cancelled as soon as the queued intent is conflicted; exactly one conflicted intent | enable=201; initial intent=queued; after target move: intents=['conflicted:shared.txt'], auto_merge GET=200; after one trigger: intents=['conflicted:shared.txt', 'conflicted:merge conflicts: shared.txt'], GET=404 — the queued intent was conflicted asynchronously; the auto-merge row stayed enabled (GET 200) and the later trigger raised a second conflicted intent (issue #141) | FAIL |
| E5 | enable auto-merge by read/triage | 403 not permitted | erin=403 {"error":"not permitted"}; frank=403 {"error":"not permitted"} | PASS |
| E6 | enable auto-merge on a draft PR | 409 pull request is a draft | 409 {"error":"pull request is a draft"} | PASS |
| E7 | enable auto-merge on a closed PR | 409 pull request is not open | 409 {"error":"pull request is not open"} | PASS |
| E8 | merged notification enqueued for the author | one pull_request.merged notification for carol's merge; none for t09's own merge | status=200; total=10; gate-4=1 actor=carol; A1 self-merge=0 | PASS |
| E9 | enable auto-merge with an invalid method | 400 | 400 {"error":"invalid input"} | PASS |
| E10 | enabler's own push re-raises the auto-merge intent | after the author pushes a new head, a fresh intent is raised; PR merges without another trigger | enable=201; initial intent=queued; push=e7daf65fc34d6700c161d2dc3309af14de9cf14d; after 7s intents=['stale:source moved']; auto_merge GET=200; pr=open — old intent went stale; on_push's re-drive raced the still-live intent and was swallowed (auto_merge.rs maybe_auto_merge / pushes.rs retargeted loop) | FAIL |
| E10b | another trigger recovers a stalled auto-merge | a status on the new head raises a fresh intent and merges | after status: intents=['stale:source moved', 'merged:']; pr=merged | PASS |
| E11 | auto-merge with method rebase | either merges or records a terminal intent state | enable=201; GET auto_merge=200; intents=0; state=open — the 500 from the rebase merge-tree is swallowed; auto-merge stays enabled and never acts (rebase blocker) | FAIL |
| E12 | another user's push cancels the auto-merge | auto-merge row deleted; no intent; PR open | enable=201; GET before=200; carol push rc=0; GET after=404; intents=0; state=open | PASS |
| F1 | large branch (150 commits) rebase: no 408 | POST returns well under the request timeout; eventually merged or an explicit async conflicted result | POST 201 in 0.09s; intent after POST=queued; after 8s=executing; main unchanged=True — no 408, but the intent hangs in a non-terminal state (worker storage error; lease 3600s) | FAIL |
| F2 | large genuinely conflicting rebase never silently merges | explicit async conflicted result (or refused); main unchanged | POST 201 in 0.09s; intent after 8s=executing reason=None; main unchanged=True | PASS |
| F3 | a large rebase that hits a worker error reaches a terminal state | merged, conflicted or failed; cancel possible | intent state=executing; delete=409 {"error":"merge intent is already executing"}; lease=executing\|2026-11-13 22:20:59.756664+00 — stuck executing with a 3600s lease; DELETE refuses an executing intent | FAIL |
| G1 | two simultaneous merge attempts on the same PR | exactly one 201 and one 409; one intent; one ref update; main advances once | HTTP=[201, 409]; intents=1; ref_updates=1; first-parent commits added=1; merge parents=2, second==head | PASS |
| G2 | merge intent revalidation after the target moves | intent stale 'target moved'; ref unchanged; no candidate applied | POST 201; intent=stale (target moved); result_oid=None; main==pushed tip=True | PASS |
| G3 | two different PRs merged simultaneously | no lost update or corruption; the loser is stale and can be re-driven | HTTP A=201 B=201; states=['stale', 'merged']; retry=201 -> merged; both files on main=[True, True] | PASS |
| G4 | merge intent revalidation after the source moves | intent stale 'source moved'; ref unchanged | POST 201; push rc=0; intent=stale (source moved); result_oid=None | PASS |
| G5 | merge intent revalidation after a required check changes | intent stale 'approvals or checks changed'; no ref move | POST 201; intent=stale (approvals or checks changed); main unchanged=True | PASS |
| H1 | merge completes within a few seconds | intent merged in < 5s | 0.51s from the first POST to a terminal merged intent | PASS |
| H4 | no duplicate journal entries for merges | one ref_update per merged intent, matching result_oid; no duplicate journal sequences | merged=27; missing journal_sequence=0; mismatched ref_update=0; duplicate ref_updates=0; duplicate repository_pushes=0 | PASS |
| H5 | one merged audit/outbox record per merge | merged audit count equals merged PR count; no duplicates | audit rows=27; merged PRs=27; duplicate audit ids=0 | PASS |
| H6 | no duplicate notification for one merge | exactly one notification row | notifications for gate-4/t09 = 1 | PASS |
| H7 | merge intents map to distinct journal commits | strictly increasing journal sequences | non-monotonic pairs=0 | PASS |
| H8 | audit actor is the account that merged | carol's merge records actor_id=carol | carol audit row=efa50857-8c67-437c-b419-993acbc8b27b | PASS |
| H9 | outbox event matches the journaled merge | one outbox row per merged intent with the result oid and target ref | matching outbox rows=27; merged intents=27 | PASS |
| I1 | pull page shows the merge box with all four methods | 200; method select with merge/squash/rebase/ff_only | status=503; methods found=[]; body=<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-sca — the page computes mergeability and the rebase dry-run crashes (same defect as A3) | FAIL |
| I2 | merged pull request page shows the merged state | 200; Merged badge; no merge form | status=200; has Merged badge=True; has merge form=False | PASS |
| I3 | up-to-date PR page renders the disabled merge button and reason | 200; 'already up to date' reason; disabled button | status=200; reason=True; disabled=True | PASS |
| I4 | queued PR page shows queue position and a cancel control | 200; 'Merge queue position: N'; a cancel control | enqueue=201; page=503; queue position text=None; cancel control=False — the page is 503 from the rebase defect; even with a 200 it only renders the position, no cancel form exists in pulls_ui.rs | FAIL |

## Defects

### 1. Rebase merge is entirely broken and every normal pull request page fails (blocker)
- Severity: **blocker**
- Repro (as t09, PR #5 `feature-c` → `main`, head `e09fddd9708c4d4030acd9558caba6c77ff3706c`):
  ```bash
  TOKEN=$(curl -s http://127.0.0.1:8080/api/v1/login -H 'Content-Type: application/json' \
    -d '{"username":"t09","password":"<test-password>"}' | jq -r .token)
  curl -s -w '\n%{http_code}\n' -X POST \
    "http://127.0.0.1:8080/api/v1/organizations/lab09/repositories/t09-merge/pulls/5/merge" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"method":"rebase","expected_head_oid":"e09fddd9708c4d4030acd9558caba6c77ff3706c"}'
  # 500 {"error":"storage operation failed"}  (no intent row is created)

  curl -s -w '\n%{http_code}\n' \
    "http://127.0.0.1:8080/api/v1/organizations/lab09/repositories/t09-merge/pulls/5" \
    -H "Authorization: Bearer $TOKEN"
  # 500 {"error":"storage operation failed"}
  ```
  Browser: `GET /organizations/lab09/repositories/t09-merge/pulls/5` → **503 Unavailable**.
  The same `GET /pulls/1` on `acme/web` (read-only) also returns 500, so this is not lab-local.
- Proof of the Git-level cause:
  ```bash
  docker compose exec -T app git --version
  # git version 2.39.5
  docker compose exec -T app sh -c '
    R=/var/lib/githearth/repositories/01/01e29062e6414c999446f2f0e71aba45.git
    git -C $R merge-tree --write-tree -z --name-only --no-messages \
      --merge-base=579ed5938a0d56f0ae102ec6feeea416b08b549a \
      refs/heads/main refs/heads/feature-c'
  # error: unknown option `merge-base=...'            rc=129
  ```
- Expected: 201 and a merged or conflicted intent; a 200 pull page.
- Actual: 500 from the API, 503 from the HTML page, no intent for a small branch. Deployed
  Git 2.39.5 (Debian bookworm package, `Dockerfile:17-18`: `debian:bookworm-slim` +
  `apt-get install git`) does not accept `git merge-tree --merge-base=<oid>`, which
  `crates/githearth-git/src/merge.rs:285` always passes for a rebase. The startup check
  only enforces ≥ 2.38 (`crates/githearth-git/src/layout.rs:67`, message
  "git {major}.{minor} is older than 2.38"), and ADR 0004 line 61 claims 2.43 is in use.
- Impact: the rebase method can never succeed (A3, C4); every pull page whose rebase
  mergeability is not short-circuited as `up_to_date` is unavailable (I1, I4); auto-merge
  with method rebase is silently inert (E11). A 150-commit branch does not 408, but the
  intent is then stranded (defect 5).
- Scope: **systematic**, every repository on this deployment.

### 2. An outstanding change request does not block a merge (major)
- Severity: **major**
- Repro:
  ```bash
  # PR review-2 -> main, head H; no other gates
  curl -s -X POST .../pulls/<n>/reviews -H "Authorization: Bearer $CAROL" \
    -H 'Content-Type: application/json' -d '{"verdict":"approved","commit_id":"H","body":"ok"}'
  curl -s -X POST .../pulls/<n>/reviews -H "Authorization: Bearer $DAVE" \
    -H 'Content-Type: application/json' -d '{"verdict":"changes_requested","commit_id":"H","body":"no"}'
  curl -s -X POST .../pulls/<n>/merge -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' -d '{"method":"merge","expected_head_oid":"H"}'
  # 201, intent merged, main moves
  ```
  Observed in B12 (no required approvals) and B18 (required_approvals=1, one approval plus
  one change request).
- Expected: 409 refusal, ref unchanged (as on GitHub and as the domain expects).
- Actual: 201, the merge executes and the PR becomes `merged`.
- Evidence: `crates/githearth-server/src/merge_intents.rs:248-287` (`merge_allowed`) builds
  merge facts from approvals, required checks and code owners only;
  `crates/githearth-authz/policies/builtin.cedar:112-133` has no change-request forbid.
  `pulls.rs` computes `changes_requested` for display (line 229/537) but the merge path
  never reads it.
- Scope: systematic.

### 3. Auto-merge silently stalls when the enabler pushes to the PR branch (major)
- Severity: **major**
- Repro:
  ```bash
  # main protected with require_merge_queue=true; PR auto-push2 -> main
  curl -s -X POST .../pulls/<n>/auto_merge -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' -d '{"method":"merge"}'      # 201, intent queued
  git push origin auto-push2        # same account, immediately after the 201
  # 7s later:
  #   intents = ['stale:source moved']
  #   GET /auto_merge = 200 (still enabled)
  #   PR = open, no fresh intent
  # a later status on the new head raises a new intent and it merges (E10b)
  ```
- Expected: the push re-targets the auto-merge row and raises a fresh intent, so the PR
  merges without another trigger (the code comment in `auto_merge.rs` promises exactly this).
- Actual: the old intent is marked `stale`; `on_push`'s re-drive ran while the old intent was
  still live, `create_intent` returned 409 `an open merge intent already exists`, and
  `maybe_auto_merge` dropped it (`_ => {}`, `auto_merge.rs:594-623`). Nothing retries after
  the worker stales the row, so auto-merge remains "enabled" but inert.
- Evidence: E10 (`intents=['stale:source moved']`, `GET /auto_merge=200`, PR open); the
  re-drive loop is `pushes.rs:452-457`, the swallowed 409 is `auto_merge.rs:607-623`.
- Scope: systematic race; whichever of push/worker wins decides whether the feature works.

### 4. Auto-merge stays enabled after an asynchronously discovered conflict, and a later trigger creates a duplicate conflicted intent (#141) (major)
- Severity: **major**
- Repro: queue rule (`require_merge_queue=true`, direct pushes allowed), PR `auto-q4` edits
  `shared.txt` line 3, enable auto-merge, then move `main` on the same line:
  ```bash
  curl -s -X POST .../pulls/<n>/auto_merge -d '{"method":"merge"}'   # 201, intent queued
  git push origin main-conflict-4:main                                # target moves, same line
  # 6s later: intent = conflicted:shared.txt ; GET /auto_merge = 200 (still enabled)
  # one status on the head:
  #   intents = ['conflicted:shared.txt', 'conflicted:merge conflicts: shared.txt']
  #   GET /auto_merge = 404 (now cancelled)
  ```
- Expected: the auto-merge row is cancelled as soon as the queued intent is conflicted;
  exactly one conflicted intent, no duplicate audit records.
- Actual: the row stays enabled; the next trigger creates a second conflicted intent and
  only then cancels the row. The synchronous conflicting case (E4a) is correct.
- Evidence: E4b. The cancel branch only runs when `create_intent` returns `Ok` with a
  conflicted intent (`auto_merge.rs:607-621`); an intent that becomes conflicted later is
  transitioned by the worker in `merge_worker.rs:150-154`, which never touches
  `auto_merges`.
- Scope: systematic for async conflicts (queue re-target, target race).

### 5. A worker storage error leaves a merge intent `executing` for an hour and uncancellable (#140) (major)
- Severity: **major**
- Repro: create a ~150-commit branch (`big-clean`), open a PR, request a rebase merge:
  ```bash
  curl -s -w '\n%{http_code}\n' -X POST .../pulls/9/merge \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"method":"rebase","expected_head_oid":"8bab8f80b06e14128650fa8c3cae94561bc994b3"}'
  # 201 in 0.09s (no 408); intent queued
  curl -s .../pulls/9/merge -H "Authorization: Bearer $TOKEN"
  # state=executing, lease_expires_at=2026-11-13 22:20:59+00 (one hour after the claim)
  curl -s -X DELETE .../pulls/9/merge -H "Authorization: Bearer $TOKEN"
  # 409 {"error":"merge intent is already executing"}
  ```
  A 150-commit *conflicting* rebase behaves the same: no explicit `conflicted` result, no
  merge, intent stuck (F2 only passes because the ref never moves).
- Expected: an explicit async `conflicted` (or `failed`) result; a cancelable intent.
- Actual: the worker's `MergeError` path logs and leaves the row (`merge_worker.rs:211-223`);
  the lease is `git_timeout_seconds * 6` = 3600 s (`merge_worker.rs:234`, `config.rs` default
  600); `DELETE` refuses an executing intent (`merge_intents.rs:792-794`). With
  `MAX_ATTEMPTS=5`, a stranded intent survives up to five further one-hour leases before the
  attempt cap marks it failed (about six hours in total).
- Evidence: F1 and F3. This is a generic worker-robustness defect, exposed here by the rebase
  crash: any transient storage error strands a merge the same way.
- Scope: systematic whenever `execute_intent` returns `MergeError`.

### 6. `ff_only` on a non-fast-forwardable PR is accepted, then fails asynchronously (minor)
- Severity: **minor**
- Repro: PR `feature-d` → `main` with `main` ahead of the branch:
  ```bash
  curl -s -w '\n%{http_code}\n' -X POST .../pulls/6/merge \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"method":"ff_only","expected_head_oid":"8d58d0f3e212ed59a910707e125d7938ff6cb100"}'
  # 201; intent state=stale, failure_reason="target is not an ancestor of the source"
  ```
- Expected: 409/422 at the API (the domain's contract), no ref move.
- Actual: 201 then `stale`; no ref move. The synchronous mergeability check deliberately
  excludes `ff_only` (`merge_intents.rs:556-561` only maps merge/squash/rebase), and the
  worker maps `Candidate::NotFastForward` to `Stale` (`merge.rs:552-555`).
- Scope: deterministic.

### 7. The pull page has no cancel control for a queued merge (minor)
- Severity: **minor**
- Repro: queue a PR (`require_merge_queue=true`, POST merge) and open its page.
- Expected: queue position **and** a way to cancel, per the domain's UI criterion.
- Actual: the page is 503 in practice because of defect 1; independently, the template only
  renders `<p class="muted">Merge queue position: N</p>` (`crates/githearth-server/src/pulls_ui.rs:1042-1045`)
  and no form or button that issues the existing `DELETE .../merge`. Cancellation exists only
  in the JSON API (D5 passes).
- Scope: systematic UI gap.

## Notes / skipped

- Every failing case was re-run once before it was recorded; B12/B18/E10/E4b/F1 reproduced
  on the retry, and the races that were lost (D5/D6/G2/E4b first attempts) were retried with
  a fresh branch and are reported from a completed run.
- **B6 skipped**: `grace` carries an `admin` grant in `lab09` (the fixture has cumulative
  grants: carol=write+maintain, dave=maintain+admin, erin=read+triage, frank=member+triage,
  grace=member+admin), so it is not a member-only probe. Effective permissions were verified
  from `grants` with read-only SQL.
- **D9 skipped**: cancellation of an `executing` intent was not observed during quick merges
  (the window is < 100 ms); defect 5 exercises the same refusal on a stuck intent (F3).
- **Rebase success path untestable** because of defect 1: A3, C4, E11, F1 and F3 all hit it.
  There is no way to obtain a successful rebase merge on this deployment.
- Cross-repository/fork merges and merge-queue batching are ADR 0004 open items and were not
  tested.
- The pull JSON does not expose `queue_position` or `merged_by`; queue positions were read
  with the server's own `queue_position` query in read-only SQL, and `merged_by` from the DB.
  The UI queue position could not be seen because the page is 503.
- The merged page does render correctly (I2), the up-to-date page renders its disabled state
  (I3), and an existing required-check success on a *different* commit correctly fails to
  authorize a merge (B16).
- Journal/audit integrity is good: 27 merged intents, each with exactly one matching
  `ref_updates` row and one merged audit/outbox record, no duplicate journal sequences or
  notifications (H4-H9).
- One transient 405 on `POST /organizations/lab09/repositories` at session start resolved on
  retry (201); treated as environment noise, not a product defect.
- The instance is shared with other testers; the server log shows unrelated 500s in other
  repositories. All evidence above is from `lab09/t09-merge` unless stated otherwise.

## Round 2 — fixed build (2026-11-14)

Tester: t09. Instance: http://127.0.0.1:8080, app image corresponding to `7e1ded3`
(`fix/m4-lab`; Git 2.47.3 and all four fixes present); `git version 2.47.3` inside
`githearth-app-1`; `/health/live` and `/health/ready` both 200. Round 1 ran on `main`
b820454 with Git 2.39.5.

Round 2 verdict: **PASS WITH DEFECTS**. The blocker is fixed, the change-request gate and
the auto-merge/intent lifecycle defects are fixed, and worker failures no longer strand an
intent. Two round-1 minor defects remain; one new minor behaviour note is recorded.
Cases: 31 run, 29 pass, 2 fail, 0 skipped.

Fixture: new repository `lab09/t09-r2` (26 PRs, 20 merged) plus `lab09/t09-wf` for the
worker-failure probe. Same accounts as round 1 (t09 owner/merger, carol and dave reviewers,
erin read). No app or database restart; all protection rules were created and deleted inside
`lab09/t09-r2`; database access was read-only. No acme/beta setting changed (one read-only
`GET` on acme PR #1 was used to confirm the detail-read fix is not lab-local).

### Results (round-2 re-run)

| # | Case | Expected | Actual | Verdict |
| --- | --- | --- | --- | --- |
| R2-01 | Rebase merge (round-1 blocker; PR #3) | 201; merged; linear replay, no merge commit | POST 201 in 0.109s; intent merged in 1s, result `ab07657`; first parent `3fe6b68`→`f25c2d9`; `rb.txt` present; original author preserved; 0 merge commits | PASS |
| R2-02 | PR detail reads (blocker) | 200 for every PR, including merged/conflicted | `t09-r2` 26/26 = 200 (open, merged, conflicted, stale, cancelled); `t09-wf` 2/2 = 200; `acme/web` PR #1 = 200 | PASS |
| R2-03 | PR HTML pages (blocker) | 200 for open, merged, conflicted and 150-commit PRs | pages 24/3/6/7 = 200 (6257/4866/6090/26417 bytes); merged badge and conflict text rendered | PASS |
| R2-04 | Merge commit (smoke) | 201; 2 parents, second == head | PR #1: 201; merged in <1s; second parent `df712a7` == head | PASS |
| R2-05 | Squash (smoke) | 201; one parent; content, title/body/Merged-by | PR #2: 201; merged 2s; 1 parent; s1+s2+s3; msg `feat-squash\n\nr2 fixture\n\nMerged-by: t09` | PASS |
| R2-06 | ff_only fast-forward (smoke) | 201; merged; main == branch head (no new commit) | PR #17: 201; merged 2s; main `64ee51b` == head | PASS |
| R2-07 | ff_only non-fast-forwardable | 409/422 at the API; no ref move | PR #4: 201, then intent `stale` "target is not an ancestor of the source"; main unchanged — same as round-1 defect 6 | FAIL |
| R2-08 | Conflicting merge (smoke) | 409 naming the file; no ref move | PR #6: POST 409, intent `conflicted` "merge conflicts: shared.txt"; main unchanged | PASS |
| R2-09 | Conflicting rebase (smoke) | 409 conflicted (no 500) | PR #6 rebase: 409, `conflicted` "merge conflicts: shared.txt"; main unchanged | PASS |
| R2-10 | Outstanding change request blocks merge (rule with required_approvals=1) | 409; no ref move; no merge form | PR #14: carol approve 201, dave changes_requested 201; merge 409 `{"error":"the pull request has an outstanding change request"}`; main unchanged; page 200 shows "Changes requested" and no merge form | PASS |
| R2-11 | Fresh approval clears the change request | next merge 201; merged | dave approves the same head (approvals 2, changes_requested 0); merge 201; merged 2s | PASS |
| R2-12 | Change request blocks merge with no protection rule (round-1 B12) | 409; then approval merges | PR #25: merge 409 same message; dave approves; merge 201; merged 1s | PASS |
| R2-13 | Change request arriving after the intent is queued | intent refuses; no ref move | PR #26: merge 201 queued; dave changes_requested immediately; intent `stale` "approvals or checks changed"; `ra.txt` absent from main | PASS |
| R2-14 | Queue enqueue, order, retarget (smoke) | 201 in_queue; merged in FIFO order; later entries retarget | q1–q3 (PR 9–11): all 201 `in_queue=true`, expected target `9525b58`; merged in order with journal 73<75<77; q2 expected `6e0165a`, q3 expected `1940f62`; all files on main | PASS |
| R2-15 | Cancel a queued entry via DELETE (smoke) | 200 cancelled; no merge | q4 position 1; DELETE 200 `state=cancelled`; `q4.txt` absent | PASS |
| R2-16 | Cancel a queued entry by closing the PR (smoke) | intent cancelled; no merge | q5 position 2; close 200; intent `cancelled`; `q5.txt` absent | PASS |
| R2-17 | 150-commit rebase: no 408 (smoke) | POST fast; terminal result | PR #7: POST 201 in 0.118s; merged 5s; 150 first-parent commits, 0 merge commits, `big-1..150` on main | PASS |
| R2-18 | 150-commit genuinely conflicting rebase | explicit async `conflicted` or refusal; no merge | PR #8: POST 201 (detail shows rebase `unchecked`); intent `conflicted` "shared.txt" within 1s; main unchanged | PASS |
| R2-19 | Concurrency, same PR (smoke) | one 201 + one 409; one intent; one ref move | PR #18: HTTP [201, 409 "an open merge intent already exists"]; one intent; merged 1s | PASS |
| R2-20 | Concurrency, two different PRs (smoke) | no lost update; loser stale and retryable | PR #19/#20: [201, 201]; one merged, one `stale` "target moved"; retry 201 → merged 3s; both files on main | PASS |
| R2-21 | Auto-merge happy path | merges; row consumed | PR #15: enable 201; merged 3s; GET auto_merge 404 | PASS |
| R2-22 | Auto-merge with method rebase (round-1 E11) | merges (no silent 500) | PR #23: enable 201; intent merged 2s, method `rebase`; row 404; linear replay | PASS |
| R2-23 | Auto-merge on a permanently conflicting PR: one intent, row stays enabled | exactly one conflicted intent across repeated triggers; row 200 | PR #16: enable 201; one `conflicted` "merge conflicts: shared.txt" (0s); after a status, a review and a target move (merge of #22) intents still = 1 and GET auto_merge = 200 each time | PASS |
| R2-24 | Conflict-resolving enabler push raises a fresh intent and merges | fresh intent queued; merged; row consumed | push `a527e7c` at 22:26:26.377; new intent created 22:26:26.501 (+124ms); merged 22:26:28.480 (1.98s); intents = 1 conflicted + 1 merged; row 404 | PASS |
| R2-25 | Enabler push while an intent is queued (round-1 E10 stall) | old queued intent cancelled; fresh intent; no `already_queued` stall | PR #21: enable at 22:25:17.787; push at 22:25:18.058; old intent cancelled at 18.129; new intent created same instant; merged at 18.433 (375ms after push); content `one two`; row 404 | PASS |
| R2-26 | Deterministic worker failure: attempts bounded, no 1h strand | short terminal failure, bounded attempts | `t09-wf` PR #2, repo dir hidden after POST: 5 warnings at 22:27:08.296–08.305 (9ms), `lease_attempt=6`, state `failed` "too many attempts" at 22:27:08.306 — 0.946s after creation (round 1: `executing` for ~1h). PR #3, `objects` dir hidden: `journal replay error: storage error: Git failed: fatal: not a git repository`; 5 attempts in 9.5ms; failed 0.105s after creation | PASS |
| R2-27 | Worker recovery after the failure clears | a fresh merge succeeds | repo restored; PR #2 retry 201 → merged 2s (main `f41c839`); PR #3 retry 201 → merged 2s (main `2d85c6b`) | PASS |
| R2-28 | Repeated merge POST on a permanently conflicting PR | reuse the recorded conflict, one intent | PR #6 POST ×2: both 409 with the same operation_id; one `conflicted` intent for method merge (plus one for rebase) | PASS |
| R2-29 | Journal/outbox integrity | one ref_update/outbox per merged intent; no duplicate conflicted events | 20 merged intents, 20 matching ref_updates, 0 missing, 20 merged outbox rows; 4 conflicted intents = 4 `merge_intent_conflicted` audit events for lab09 (no duplicate events from the triggers) | PASS |
| R2-30 | Queued PR page: position and cancel control | 200; position; a cancel control | page 200 with "Merge queue position: 1"; no cancel form/button (round-1 defect 7 unchanged) | FAIL |
| R2-31 | Merge box offers all four methods | 200; merge/squash/rebase/ff_only | PR #24 page 200; merge select has Merge commit, Squash and merge, Rebase and merge, Fast-forward only (also on the auto-merge form) | PASS |

### Timings (measured)

| Action | Latency |
| --- | --- |
| Rebase merge POST (2 commits, PR #3) | 0.109s → merged at 1s |
| Rebase merge POST (150 commits, PR #7) | 0.118s → merged at 5s |
| Merge commit / squash / ff_only / queue entries | POST 201 immediately, terminal 1–3s |
| Conflicting merge or rebase | 409 returned synchronously (small branch); big conflicting rebase `conflicted` within 1s |
| Change request → merge refusal | immediate 409 at intent creation; queued-intent variant stales at worker pass (~2s) |
| Auto-merge conflict: enable → conflicted intent | 0s (synchronous) |
| Auto-merge conflict: resolving push → new intent | +124ms; merged 1.98s after creation |
| Auto-merge enabler push while queued | old intent cancelled +71ms after push; merged +375ms |
| Worker deterministic failure | 5 attempts in 9–9.5ms; terminal 0.946s (repo dir hidden) / 0.105s (`objects/` hidden) after intent creation; lease was 3600s |

### Remaining defects

#### 1. `ff_only` on a non-fast-forwardable PR is still accepted, then fails asynchronously (minor)
- Severity: **minor** (unchanged round-1 defect 6)
- Repro:
  ```bash
  # PR #4 feature-nonff -> main, head 9864815e0d995a55147438445f6661f846546bb0
  curl -s -w '\n%{http_code}\n' -X POST \
    "http://127.0.0.1:8080/api/v1/organizations/lab09/repositories/t09-r2/pulls/4/merge" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"method":"ff_only","expected_head_oid":"9864815e0d995a55147438445f6661f846546bb0"}'
  # 201 {"state":"queued"}; ~2s later: state=stale,
  # failure_reason="target is not an ancestor of the source"; main unchanged
  ```
- Expected: 409/422 at the API with no ref move (as the domain contract and round-1 A5 expect).
- Actual: 201 then `stale`; no ref move. The synchronous mergeability precheck still excludes `ff_only`.
- Scope: deterministic.

#### 2. The queued pull page still has no cancel control (minor)
- Severity: **minor** (unchanged round-1 defect 7)
- Repro: with `require_merge_queue=true`, enqueue PR #24 and open
  `GET /organizations/lab09/repositories/t09-r2/pulls/24`.
- Expected: queue position **and** a way to cancel, per the domain UI criterion.
- Actual: page is now 200 (was 503 from the rebase defect) and shows
  `Merge queue position: 1`, but there is no form or button that issues the existing
  `DELETE .../merge`; cancellation exists only in the JSON API (R2-15 passes).
- Scope: systematic UI gap.

#### 3. New: worker retries have no spacing, so the 5-attempt budget burns in milliseconds (minor)
- Severity: **minor** (new behaviour note, not a regression of the requirement)
- Repro: hide the repository directory (or only its `objects/` directory) after a merge intent
  is queued (as in R2-26).
- Expected/design comment: "release it to retry on the next pass"; attempts stay bounded.
- Actual: the lease is set to `now() - 1s`, so `run_once` immediately reclaims the intent in
  the same pass. In the repo-dir probe all five `merge intent execution failed` warnings are
  logged within 9ms (`22:27:08.296`–`08.305`) and attempt 6 marks it `failed` "too many
  attempts" at `22:27:08.306` (0.946s after creation). In the `objects/` probe the error is
  `journal replay error: storage error: Git failed: fatal: not a git repository`, five
  attempts in 9.5ms, terminal 0.105s after creation. No intent is stranded for the lease
  (requirement met), but there is no pause between attempts: a transient storage/journal
  error lasting more than a few milliseconds consumes the whole budget and fails the merge
  (a manual retry then works, R2-27).
- Scope: systematic for `MergeError::Storage`/`Replay`/`Join` (the storage failure surfaced
  as `MergeError::Replay` wrapping a Git storage error; both are non-database and share the
  released-lease branch).

### Fixed, confirmed

- **Blocker:** deployed Git is 2.47.3; `git merge-tree --merge-base` works. Rebase merge
  succeeds and every PR detail/page tested is 200 (previously 500/503).
- **Change-request gate:** 409 at intent creation, stale at worker revalidation, and the
  merge form disappears; a fresh approval on the same head clears it (with and without a
  protection rule).
- **Auto-merge conflict lifecycle:** a conflicting merge is recorded once, repeated
  status/review/push triggers reuse it (same operation_id), the row stays enabled, and a
  conflict-resolving push by the enabler raises a fresh intent that merges. The queued
  enabler-push race no longer stalls with `already_queued`: the obsolete intent is cancelled
  in the same transaction as the new insert.
- **Worker failure:** a deterministic non-database error releases the lease and the intent
  reaches a terminal `failed` in 0.95s instead of sitting `executing` for an hour.

### Notes

- The 150-commit *rebase* mergeability in PR detail reads `unchecked`
  (merge/squash read `conflicts`); the worker still records an explicit `conflicted`
  result with the path, so nothing merges silently.
- Repeated `POST /merge` on a permanently conflicted head returns 409 with the **same
  operation_id** and creates no second row (PR #6).
- Queue positions were read with the server's own `queue_position` query in read-only SQL
  (q4=1, q5=2); q1–q3 merged before a position could be sampled (created 0.35s apart).
- The worker probe renamed the `t09-wf` repository directory for <1s. The reconciler logged
  `malformed repository directory; not touched` for the temporary name; after restore,
  pushes and merges work (R2-27). This is test-induced, not a product error.
- The narrow race where the enabler pushes while the intent is already `executing` (rather
  than `queued`/`validating`) was not exercised; the fix only cancels queued/validating
  intents, so that window remains a theoretical residual.
- B6 and D9 remain skipped for the same reasons as round 1. Cross-repository/fork merges
  and merge-queue batching remain ADR 0004 open items and were not tested.
