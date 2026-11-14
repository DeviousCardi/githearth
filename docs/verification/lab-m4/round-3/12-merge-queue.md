# Merge queue and merge execution — M4 closure round 3

Tester: t12. Date: 2026-11-14. Instance: main `800cad4`.
Verdict: PASS
Cases: 109 run, 108 pass, 0 fail, 1 skipped.

## Fixtures

Four private repositories in `lab12`, all owned by `t12`:

- `r3-methods-1794639714` — merge methods, conflicts, ff-only, stale, concurrency, 150-commit rebase, UI forms.
- `r3-mq-1794639714` — branch protection `main` with `require_merge_queue=true`.
- `r3-auto-1794639714` — protection `main` with `required_approvals=1`,
  `dismiss_stale_approvals=false`; auto-merge and change-request cases.
- `r3-policy-1794641530` — protection added after an intent, for policy-generation stale.

Second accounts used: `bob` (write, approvals/change requests/foreign pushes),
`carol` (maintain, stale change request), `erin` (read, authorization checks).
Intent rows were read with read-only SQL only. No repository source was edited.

## Results

### Merge methods and validation

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 1 | merge method creates a merge commit and merges the PR | 201 queued -> merged; PR merged | 201 queued; merged; merged | PASS |
| 2 | squash merges with the PR title | 201 -> merged | 201 merged | PASS |
| 3 | rebase replays the source commits | 201 -> merged | 201 merged | PASS |
| 4 | ff_only fast-forwards the target | 201, target == source head | 201; tip=bc8b51e1 source=bc8b51e1 | PASS |
| 5 | ff_only non-fast-forward refused before queueing | 409, 0 intents, repeatable | 409 {'error': 'target is not an ancestor of the source'}; 0; 409 | PASS |
| 6 | wrong expected_head_oid refused | 409 commit is not the head | 409 {'error': 'commit is not the pull request head'} | PASS |
| 7 | invalid method / uppercase oid / unknown field | 400 for each | 400 {'error': 'invalid input'}; 400 {'error': 'invalid input'}; 400 {'error': 'invalid input'} | PASS |
| 8 | closed pull request refused | 409 not open | 409 {'error': 'pull request is not open'} | PASS |
| 9 | draft pull request refused | 409 pull request is a draft | 409 {'error': 'pull request is a draft'} | PASS |
| 10 | conflict reported by mergeability | merge/squash/rebase all conflicts | {'merge': 'conflicts', 'squash': 'conflicts', 'rebase': 'conflicts'} | PASS |
| 11 | conflicting merge records a conflicted intent | 409 state=conflicted with paths | 409 conflicted; merge conflicts: README.md | PASS |
| 12 | repeated conflict reuses the recorded intent | same operation_id, 1 row | True rows=1 | PASS |
| 13 | conflicting squash and rebase refused | 409 conflicted for each method | 409 conflicted; 409 conflicted | PASS |
| 14 | conflict moves no ref and warns in the UI | target unchanged, warning shown | target unchanged; conflict warning shown | PASS |
| 15 | one terminal conflicted intent per method | 3 rows | 3 | PASS |
| 16 | already-contained branch fails as nothing to merge | state failed, reason nothing to merge | 201 failed nothing to merge | PASS |
| 17 | second live merge refused | 409 already exists | 201/409 {'error': 'an open merge intent already exists for this pull request'} | PASS |
| 18 | idempotency replay returns one intent and one outbox event | same op, 1 intent, 1 outbox row | 201/201 True; 1; 1 | PASS |
| 19 | concurrent duplicate merge requests | 201 + 409, one intent | [201, 409] rows=1 state=merged | PASS |

### Merge queue

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 20 | enqueue sets in_queue=true | 201, in_queue true | 201 True; 201 | PASS |
| 21 | queue positions 1 and 2 | positions 1 and 2 | p1=1; p2=2 | PASS |
| 22 | queued UI state and cancel control | Merge: Queued and Cancel queued merge | Merge: Queued and cancel control shown | PASS |
| 23 | both queue entries merge | merged | merged; merged | PASS |
| 24 | second entry retargets to the first result | expected_target == first result | 42ff3939 vs 42ff3939 | PASS |
| 25 | target ends at the last result in creation order | tip == last result, first parent is previous result | 9b1ec61e 9b1ec61e; 42ff3939 | PASS |
| 26 | cancel API cancels a live entry | 200 state cancelled | 200 cancelled | PASS |
| 27 | cancelled entry never merges and moves no ref | cancelled, PR open, target unchanged | cancelled pr=open; 9b1ec61e->9b1ec61e | PASS |
| 28 | cancelled entry loses its position | no position, Merge: Cancelled | no position; Merge: Cancelled shown | PASS |
| 29 | cancel writes one audit event | 1 | 1 | PASS |
| 30 | UI cancel form cancels and redirects | 303 to the PR page, state cancelled | 303 to the PR page; cancelled | PASS |
| 31 | cancel with no live intent | 404 | 404 {'error': 'not found'} | PASS |
| 32 | read-only user cannot cancel | 403 | 403 {'error': 'not permitted'} | PASS |
| 33 | GET merge returns the queue intent | 200, in_queue true | 200 True | PASS |
| 34 | remaining entry moves to position 1 after a cancel | position 1 | position 1 | PASS |

### Auto-merge

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 35 | enable stores a row and raises no intent before approvals | 201, GET 200, merge 404 | 201 merge; 200; 404 | PASS |
| 36 | approval triggers the merge and consumes the row | merged, row 404, completed event | 201; merged; merged; 404; 1 | PASS |
| 37 | conflicting auto-merge keeps the row enabled | row 200, one conflicted intent | 201; 201; 200 (row present); 1 | PASS |
| 38 | repeated trigger dedupes the conflict | same op, one row | True rows=1 | PASS |
| 39 | enabler push re-raises and merges | merged, row consumed | merged None; 404 | PASS |
| 40 | enable is audited | >=2 events | 7 | PASS |
| 41 | disable API removes the row | 200 then 404 | 201; 200 merge; 404 | PASS |
| 42 | disable UI form removes the row | 303 then 404 | 303 /organizations/lab12/repositories/r3-auto-1794639714/pulls/4; 404 | PASS |
| 43 | non-enabler push cancels auto-merge | 404, cancelled event, no intent | 201; 404; 3; 404 | PASS |
| 44 | validation and authorization | 400 / 403 / 409 | 400 {'error': 'invalid input'}; 403 {'error': 'not permitted'}; 409 {'error': 'pull request is not open'} | PASS |
| 45 | GET auto-merge when none | 404 | 404 | PASS |

### Stale intents

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 46 | direct intent goes stale when the target moves | stale target moved | stale target moved | PASS |
| 47 | stale intent moves no ref and leaves the PR open | result_oid null, PR open | None tip=593ed873; open | PASS |
| 48 | UI reports Stale with the reason | Merge: Stale + target moved | stale=True reason=True | PASS |
| 49 | a fresh intent after stale merges | 201 merged | 201 merged; merged | PASS |

### Concurrency

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 50 | both concurrent intents accepted | 201/201 | 201/201 | PASS |
| 51 | exactly one merge wins, the other is stale | one merged, one stale target moved | ['merged', 'stale']; target moved | PASS |
| 52 | target equals the winner and one ref update is journaled | tip == winner, 1 ref update | 98968af1 98968af1; 1 | PASS |
| 53 | only the winner's file reaches the target | one of the two files | ['conca.txt'] | PASS |

### 150-commit rebase

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 54 | 150-commit rebase accepted | 201 | 201 | PASS |
| 55 | completes without 408 | merged in <240 s | merged in 4.6s | PASS |
| 56 | all 150 commits replayed | 150 | 150 | PASS |
| 57 | no merge commit from rebase | single parent | 1 | PASS |

### Worker retry and attempt cap

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 58 | attempts are capped at 6 | max 6; capped rows fail with too many attempts; none over 6 | 6; 2; 0 | PASS |
| 59 | live leases stay within 6x git timeout | 0 over 3601 s | 0 | PASS |
| 60 | error retry is spaced 10 s apart | lease now()+10 s on a non-database error | not inducible without a fault-injection hook; code path at merge_worker.rs:220-240 sets lease now()+10 s and breaks the pass | SKIP |

### Merge UI

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 61 | UI merge form submits and merges with the chosen method | 303, squash merged | 303 /organizations/lab12/repositories/r3-methods-1794639714/pulls/25; merged squash | PASS |
| 62 | UI conflict submit redirects to the conflict page | 303, page shows conflicts and Conflicted | 303; page shows conflicts and Merge: Conflicted | PASS |

### Audit and outbox

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 63 | merge intent reaches merged | merged | merged | PASS |
| 64 | one merge_intent_created audit+outbox pair | 1 | 1 | PASS |
| 65 | one pull_request.merged audit+outbox pair | 1 | 1 | PASS |
| 66 | one ref_update and one repository_pushes row | 1 and 1 | 1; 1 | PASS |
| 67 | merged outbox payload names ref and result | refs/heads/main and result oid | {"new": "574b4980a4240bfa43399b52068847f9b8698922", "old": "6a83cbf3adee552da2bba7f6efeffab914df7bbf", "ref": "refs/heads/main", "actor_id": "82997e55-fe78-4e2e-9aff-a4cebd5e5b79", "operation_id": "64 | PASS |
| 68 | replay after merge adds no events or rows | same op, counts unchanged | 201 True; 1; 1 | PASS |
| 69 | merging an already merged PR | 409 | 409 {'error': 'pull request is not open'} | PASS |
| 70 | audit chain verifies with a fresh session | 200 ok true; stale session 403 | 200 ok true; stale session 403 | PASS |

### Cancel while executing

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 71 | cancelling an executing queue merge is refused | 409 already executing | state=executing 409 {'error': 'merge intent is already executing'} | PASS |
| 72 | the executing merge still completes | merged | merged | PASS |

### Queue and auto-merge extras

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 73 | non-queue merge records in_queue=false | false | False | PASS |
| 74 | queue supports squash | merged squash, in_queue true | merged squash True | PASS |
| 75 | queue supports ff_only | merged, target == source | merged tip=cb01ed96 | PASS |
| 76 | cancelled entry can be re-enqueued and merges | cancelled,merged | 201 merged [cancelled,merged] | PASS |
| 77 | enabling auto-merge on an already approved PR merges at once | 201 merged | 201; 201 merged | PASS |
| 78 | a commit status triggers auto-merge | no intent before, merged after ci/build success | 201 357c9e10-cadd-4db7-af6b-8286a5e8fba8; 201; 404; 201; merged None | PASS |
| 79 | later squash queue entry retargets to the earlier result | expected == earlier result | f49d85b5 vs f49d85b5 | PASS |
| 80 | cancel writes exactly one cancelled audit event | 1 | 1 | PASS |
| 81 | ff_only pre-check refuses before enqueueing | 409, 0 intents | 409 intents=0 | PASS |

### UI regression smoke

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 82 | merge form offers all four methods | ff_only and rebase present | ff=True rebase=True | PASS |
| 83 | UI ff_only refusal shows the 409 notice | 409 cannot be merged right now | 409 True | PASS |
| 84 | settings page renders and lists the queue rule | Require a merge queue checked; main listed | 200 checked=True; 200 main=True | PASS |
| 85 | read-only viewer sees the queue position | position shown | position shown | PASS |
| 86 | read-only viewer gets no merge form | no merge button | no merge button | PASS |
| 87 | read-only viewer gets no cancel control | no cancel button | no cancel button | PASS |
| 88 | merged PR shows the badge and no merge form | Merged, no button | badge=True button=False | PASS |
| 89 | non-queue PR has no queue position | no position text | no position text | PASS |

### Change-request closure

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 90 | maintainer requests changes at head1 | 201 | 201 | PASS |
| 91 | approval at head2 | 201 | 201 | PASS |
| 92 | detail counts the stale change request | changes_requested=1 | 1 | PASS |
| 93 | stale change request blocks merge when dismissal is off | 409 outstanding change request | 409 {'error': 'the pull request has an outstanding change request'} | PASS |
| 94 | blocked merge creates no intent | 0 | 0 | PASS |
| 95 | change request at head1 for the dismissal case | 201 | 201 | PASS |
| 96 | detail dismisses the stale change request | changes_requested=0 | 0 | PASS |
| 97 | stale change request dismissed and merge succeeds | 201 merged | 201 merged | PASS |
| 98 | change request at head for the auth case | 201 | 201 | PASS |
| 99 | read-only caller gets 403/404 not the change-request 409 | 403 | 403 {'error': 'not permitted'} | PASS |
| 100 | a fresh approval from the same reviewer clears the request | 201 merged | 201 merged | PASS |

### Queue stress

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 101 | seven queued entries all merge | 7 merged | ['merged', 'merged', 'merged', 'merged', 'merged', 'merged', 'merged'] | PASS |
| 102 | no entry hits the attempt cap | 0 non-merged | 0 | PASS |
| 103 | entries retarget as the queue advances | >=1 retargeted | 1 | PASS |
| 104 | a deleted source branch fails alone | stale source moved | stale source moved | PASS |
| 105 | the following entry still merges | merged | merged | PASS |
| 106 | the broken entry moved no ref | target is the follower result | 4acefcb8 vs 4acefcb8 | PASS |

### Policy-generation stale

| # | Case | Expected | Actual | Verdict |
|---|---|---|---|---|
| 107 | intent goes stale when branch protection changes | stale branch protection changed | 201; 201; stale branch protection changed | PASS |
| 108 | stale policy intent moves no ref | target unchanged | 47fca956 47fca956 | PASS |
| 109 | re-merge succeeds under the new generation | 201 merged | 201 merged | PASS |

## Defects

None found. No product defect was filed this round.

## Notes

- **Worker retry spacing (case 60, skipped).** The 10 s spacing is only reachable
  on a non-database `MergeError`, and the instance exposes no fault-injection
  hook. The path is present at `crates/githearth-server/src/merge_worker.rs:220-240`
  (`lease_expires_at = now() + interval '10 seconds'` and `break` after one failed
  claim); the attempt cap at `merge_worker.rs:209-215` and the live lease bounds
  in cases 58–59 pass. Prior-round rows still show `lease_attempt=6` with
  `too many attempts`, confirming the cap is enforced in practice.
- **Audit read needs a fresh session.** `/audit` and `/audit/verify` authorize
  `organization.update`, a sensitive action (`crates/githearth-authz/src/lib.rs:69`),
  so a session older than the 15-minute sudo window gets 403. The first 403 was a
  test mistake; a fresh `t12` login returned `{"rows":332,"ok":true}`. Not a defect.
- **UI conflict merge redirects instead of rendering a 409 page.** `create_intent`
  returns the stored `conflicted` intent as `Ok((409, …))` (`merge_intents.rs:717-729`),
  and `merge_submit` treats any `Ok` as success and redirects (`pulls_ui.rs:1807-1819`).
  The redirected PR page shows “This branch has conflicts that must be resolved”
  and “Merge: Conflicted”, so the user still sees the refusal; the API returns 409.
  Recorded as a UX difference, not a defect.
- **Up-to-date merge asymmetry.** The UI disables the button for `up_to_date`
  (`pulls_ui.rs:116`), but the API accepts an intent and the worker later fails it
  with “nothing to merge” (`merge.rs:559-563`, case 16). No ref moves and the state
  is visible; not a regression, noted only.
- **Evidence for the closure items.**
  - ff-only pre-check before any insert: `merge_intents.rs:619-629` (case 5, 81).
  - change-request refusal after authorization: `merge_intents.rs:595-608` (cases 90–100).
  - conflict dedupe: `merge_intents.rs:656-729` (case 12).
  - queue retarget: `merge.rs:347-356` (cases 24, 79, 103).
  - queue position: `merge_intents.rs:829-847` (cases 21, 34).
  - cancel: `merge_intents.rs:882-961`; executing guard `merge_intents.rs:927-929` (cases 26–31, 71–72).
  - auto-merge conflict row stays enabled and enabler push re-raises:
    `auto_merge.rs:592-610` and `pushes.rs:452-457` (cases 37–39).
- **Load and timing.** The 150-commit rebase completed in 4.6 s with 150 replayed
  commits and no 408 (cases 54–57). Seven queued entries drained in order with
  retargeting and no attempt-cap failure (cases 101–103). Two concurrent direct
  merges produced exactly one merged intent, one `stale: target moved`, and one
  journaled ref update (cases 50–53).
- **Audit/outbox.** For one merge, exactly one `merge_intent_created` and one
  `pull_request.merged` audit/outbox pair exist, one `ref_updates` row and one
  `repository_pushes` row; the outbox payload carries `refs/heads/main` and the
  result oid (cases 63–67). Idempotent replay added no event or row (case 68).
  DB totals for the four test repositories: 57 intents — 40 merged, 7 cancelled,
  5 stale, 1 failed, 4 conflicted.
- **Test mistakes corrected (each re-run once).** A draft PR was created without
  `draft=true`; the first conflict fixture moved `main` on top of the feature
  branch, making it an ancestor; created/cancelled audit events carry no
  `operation_id` in their outbox payload, so counts were taken at the transaction
  timestamp; queue retarget ordering was compared in merge-completion order rather
  than PR-number order. These were harness errors, not product failures.
- No 5xx response was observed by the harness, and the app log had no merge-worker
  errors during the test window (only older `git exited non-zero` warnings from
  another repository before this run).
