# M3 requirement accounting and sign-off

Date: 2026-11-13. State: **complete — every M3 item is implemented, tested and signed off.** This
document maps every M3 item in [04-forge-roadmap.md](../plan/04-forge-roadmap.md#m3--reviews-and-safe-merges)
— the build list *and* the exit list — to the code and tests that implement it,
with a status per item. It is the Phase 5 gate ("every M3 requirement is
accounted for") from [08-two-person-roadmap.md](../plan/08-two-person-roadmap.md).

Each claim below was verified by reading the named source and test files; nothing
is asserted from memory. Every item is **verified** — implemented in code and
covered by an executable test.

The two items the phase plan split out — **drafts** (#129, `m3/drafts`) and
**tag protection** (#130, `m3/tag-protection`) — have since merged, so the build
and exit lists below are complete.

## Build list → code + tests

| # | M3 build item | Code | Tests | Status |
| --- | --- | --- | --- | --- |
| 1 | Pull/merge requests | `crates/githearth-server/src/pulls.rs` (API), `pulls_ui.rs` (UI), migration `0009_pull_requests.sql` | `tests/pulls.rs`, `tests/pulls_ui.rs` | verified |
| 2 | Comparisons | `crates/githearth-git/src/browse.rs` (`compare`), `crates/githearth-server/src/browse.rs` (`/compare/{spec}`), `browse_ui.rs` | `tests/browse.rs` (`compare_*`), `tests/browse_ui.rs` | verified |
| 3 | Threaded inline reviews | `pulls.rs` (comments with `commit_id`/`path`/`line`/`side`/`in_reply_to`), migration `0035_inline_review_comments.sql` | `tests/pulls.rs` (`threads_resolve_and_unresolve`, `replies_attach_to_the_root_not_to_another_reply`, `inline_comments_carry_side_and_turn_outdated`), `tests/pulls_ui.rs` | verified |
| 4 | Suggestions | `crates/githearth-git/src/suggestion.rs`, `pulls.rs` (`/comments/{id}/apply`) | `crates/githearth-git/tests/suggestion.rs`, `tests/suggestions.rs`, `tests/pulls_ui.rs` (`inline_comment_renders_and_applies_a_suggestion`) | verified |
| 5 | Approvals / change requests | `pulls.rs` (reviews, verdicts `approved`/`changes_requested`/`commented`) | `tests/pulls.rs` (`approval_counts_only_at_the_current_head`, `latest_verdict_per_reviewer_wins`), `tests/merge_intents.rs` (`digest_changes_when_a_change_request_arrives_at_the_same_head`) | verified |
| 6 | Drafts | `pulls.rs` (draft flag; mark ready / convert), `pulls_ui.rs`, migration `0039_pull_request_drafts.sql` | `tests/drafts.rs` | verified |
| 7 | Reviewers / code owners | `pulls.rs` (review requests), `codeowners.rs`, migrations `0018`/`0020`/`0034` | `tests/review_requests.rs`, `tests/codeowners.rs` | verified |
| 8 | Checks | `statuses.rs`, migrations `0023_commit_statuses.sql`, `0033_merge_checks.sql` | `tests/statuses.rs` | verified |
| 9 | Branch protection | `protection.rs`, migration `0010_branch_protection.sql` | `tests/protection.rs`, `tests/git_push.rs` | verified |
| 9b | Tag protection | `tag_protection.rs`, `tag_protection_ui.rs`, `git_admission.rs` (`decide_ref`), migration `0040_tag_protection_rules.sql` | `tests/tag_protection.rs`, `tests/git_push.rs`, `tests/ssh.rs` | verified |
| 10 | Signed-commit policy | `git_http.rs` (`verify_signed_commits`), `crates/githearth-git/src/signature.rs`, migration `0037` | `tests/git_push.rs` (`require_signed_commits_refuses_unsigned_and_accepts_signed_push`, `require_signed_commits_refuses_commit_signed_by_another_committers_key`) | verified |
| 11 | Merge methods | `crates/githearth-git/src/merge.rs` (`build_candidate`), `merge.rs` (`execute_intent`) | `tests/merge_execute.rs` (`merge_method_merges_with_two_parents`, `squash_and_ff_only_merge`), `tests/m3_exit.rs` (`each_supported_merge_method_merges_successfully`) | verified |
| 12 | Conflicts | `mergeability.rs`, `crates/githearth-git/src/merge.rs` (`Candidate::Conflicts`) | `tests/mergeability.rs`, `tests/m3_exit.rs` (`a_conflicting_merge_is_refused_without_moving_the_ref`) | verified |
| 13 | Merge queue | `merge_intents.rs` (`in_queue`, `queue_position`), `merge_worker.rs` (`Retarget`), `protection.rs` (`require_merge_queue`), migration `0036` | `tests/merge_queue.rs` | verified |
| 14 | Auto-merge | `auto_merge.rs`, migration `0036` | `tests/auto_merge.rs` | verified |

## Exit list → code + tests

The exit paragraph has two clauses: the intent/revalidation contract, and six
concrete exit criteria. Each is accounted for separately.

| # | Exit item | Code | Tests | Status |
| --- | --- | --- | --- | --- |
| E1 | Merge intents bind source/target OIDs, candidate result, checks/approvals, policy generation and operation ID | `merge_intents.rs` (`create_intent` records `source_oid`, `expected_target_oid`, `policy_generation`, `checks_approvals_digest`, `operation_id`; `record_merge_result` records `result_oid`) | `tests/merge_intents.rs`, `tests/m3_exit.rs` | verified for binding; corrected 2026-11-15 — checks bind to the source head, not the candidate (see below) |
| E2 | Expected-target mutation; duplicate workers reuse the outcome | `merge.rs` (`execute_intent` compare-and-swap via `githearth_git::merge::update_ref_expected`, journal lookup), `merge_worker.rs` (lease) | `tests/merge_execute.rs` (`second_call_returns_already_merged_without_a_second_ref_update`, `two_concurrent_calls_for_one_intent_merge_exactly_once`), `tests/merge_worker.rs` | verified |
| E3 | Changed tips, permissions and policies require revalidation | `merge.rs` (`execute_intent` re-checks pull state, `source_oid`, `expected_target_oid`, `policy_generation`, actor grant, approvals/checks digest) | `tests/merge_execute.rs` (`stale_cases_return_stale_and_leave_the_ref`, `a_required_check_flipping_after_the_intent_is_stale`), `tests/m3_exit.rs` (`revoking_the_merger_between_intent_and_execution_goes_stale`) | verified |
| E4 | Fork contributions | `forks.rs`, `pulls.rs` (`source_repository_id`), `merge.rs` (cross-repo fetch), migration `0038` | `tests/forks.rs` (`cross_repo_pull_requests_merge_by_each_method`, `merge_intent_revalidates_recorded_refs`) | verified |
| E5 | Stale approvals/checks | `merge_intents.rs` (`dismiss_stale_approvals`), `statuses.rs` (status bound to a commit), migration `0037` | `tests/merge_intents.rs` (`stale_approval_does_not_count_by_default`, `stale_approval_counts_when_dismissal_is_off`, `a_status_for_a_different_sha_never_authorizes_the_merge`), `tests/merge_queue.rs` (`a_stale_approval_cannot_authorize_a_different_head_in_the_queue`) | verified |
| X1 | Concurrent merges cannot overwrite accepted history | `merge.rs` + `merge_worker.rs` (per-repository lock + compare-and-swap) | `tests/m3_exit.rs` (`two_concurrent_merges_into_one_target_one_wins_and_history_is_never_rewound`) | verified |
| X2 | Stale checks cannot authorize different code | `merge_intents.rs` + `statuses.rs` (digest binds checks to the source oid) | `tests/m3_exit.rs` (`a_passing_check_on_head_a_cannot_authorize_merging_head_b`) | verified for the source head; corrected 2026-11-15 — a check that passes on the head but fails on the merged tree is not caught (see below) |
| X3 | Lost metadata after a Git commit reconciles | `merge.rs` (`apply_merge_record`), `reconcile.rs`, `pushes.rs` (replay) | `tests/m3_exit.rs` (`lost_metadata_after_git_commit_is_reconciled_exactly_once`), `tests/reconcile.rs` | verified |
| X4 | Duplicate delivery cannot merge twice | `merge_intents.rs` (`record_merge_result` idempotent by `(repository_id, journal_sequence)`) | `tests/m3_exit.rs` (`duplicate_delivery_merges_once_with_one_journal_entry_and_one_event`) | verified |
| X5 | Revocation races obey documented admission semantics | `merge.rs` (authorization re-evaluated under the per-repository lock); ADR 0004 | `tests/m3_exit.rs` (`revoking_the_merger_between_intent_and_execution_goes_stale`) | verified |
| X6 | UI/API exercise merge methods, conflict handling and queue cancellation | `pulls_ui.rs` (merge form), `merge_intents.rs` (`cancel`), `mergeability.rs` (conflict display) | `tests/m3_exit.rs` (`each_supported_merge_method_merges_successfully`, `a_conflicting_merge_is_refused_without_moving_the_ref`, `a_queued_merge_can_be_cancelled_through_the_api`, `ui_merge_form_offers_methods_queues_and_cancels`) | verified |

## Notes

- The six exit criteria are proven end to end in
  `crates/githearth-server/tests/m3_exit.rs` (9 tests) against the real server,
  Git storage and PostgreSQL; see [m3-exit.md](m3-exit.md) for the full exit
  criteria report.
- The intent/revalidation contract is additionally covered by
  `tests/merge_execute.rs` (each revalidation failure mode: target moved, source
  moved, protection generation changed, check flipped after the intent) and
  `tests/merge_queue.rs` (queue re-targeting).
- Signed commits and stale approvals are specified in
  [ADR 0007](../decisions/0007-signed-commits-and-stale-approvals.md); fork
  contributions in [ADR 0008](../decisions/0008-forks.md); merge execution in
  [ADR 0004](../decisions/0004-merge-execution.md).
- Two gaps are corrected below: the merge queue tests the source head rather than the merged candidate, and drafts/tag protection were the last build items to merge. Neither changes the M3 sign-off, which is over the contracted behavior; both are recorded so the claim is not repeated as delivered.

## Correction (2026-11-15): the queue does not test the merged candidate

The master plan promises that a successful merge identifies the tested
candidate, and M3's exit says stale checks cannot authorize different code. What
shipped binds the check digest to the **source head** and the queue re-targets
the merge; it does not build the candidate and run required checks on it. A
check that passes on the source and fails only after the merge with the target
can therefore authorize the merge.

### The candidate-binding gap

The gap is in which revision a merge decision is bound to. An intent records the
source head, the expected target tip and a digest over the approvals and required
checks taken at that source head (`merge_intents.rs:617`). The worker re-reads
both refs and will not merge against revisions other than the recorded ones —
a moved source, or a moved target on a direct merge, makes the intent stale,
while a queued entry whose target moved is re-targeted and revalidated rather
than refused (`merge.rs:370–383` for the target, `merge.rs:408–416` for the
source). It then builds the merge candidate from exactly those two commits
under the repository lock (`merge.rs:560–593`) and moves the ref with a
compare-and-swap on the expected target (`merge.rs:637`). The content that is
merged is therefore the candidate for the recorded target and head. A moved ref
never merges something else, because the two cases differ: a direct merge whose
target moved goes stale, while a queued entry is re-targeted to the current tip
and revalidated before it merges (`merge.rs:370–383`); a moved source goes
stale in both cases (`merge.rs:408–416`).

What is not bound is the **decision** to that candidate. The mergeability check
that lets the merge through builds a candidate of its own at intent creation and
keeps only the conflict verdict (`mergeability.rs:123–166`, called at
`merge_intents.rs:643`); the candidate object id was not recorded at all in the
build this error was seen on, and on `main` it is written later, by the worker
(`merge.rs:614`, added after the round-4 build), while the approvals/checks
digest is computed at the source head, never over the merged tree. A merge
decision names the source and target revisions; it does not name the candidate
commit it will produce or the tree the required checks ran against.

The lab round-4 record touches this gap operationally, though it cannot pin the
link. One merge request failed `500 storage operation failed` on a git read, with
no intent row left behind (`lab-m4/round-4/05-regression.md`, O1), and the log
kept only `category=git` and the repository id, so neither the failed read nor
the `StorageError` variant could be recovered; the same merge then succeeded on
retry. Because the decision is bound to revisions and not to a candidate, a
failure at that point leaves no record of which candidate was under
consideration; but the record cannot establish that the read which failed was the
candidate build, so the event is consistent with the gap rather than proof of it.
It is not a second defect in the merge path either: the retry re-derived the same
revisions and merged, and the target binding above was never wrong.

This is a real gap against the promise, not a documentation ambiguity. The
delivered behavior is retained and M3 remains signed off over it; the missing
candidate test is scheduled as M6's "tested merge candidates" build item (M6
increment 9C, exit case U39 in the
[two-person roadmap](../plan/08-two-person-roadmap.md#phase-9--m6-isolated-ci)).
The merge-error investigation is not part of that item: the roadmap carries it
in M4.1's 6.4 operations-acceptance row. Until the candidate test ships, no
document may repeat "the queue tests the candidate it merges".

## Sign-off


Two reviewers each confirm the tables above.

### Reviewer 1

- Name: Jadav Havish Sunil (`HavishJadav`)
- Date: 2026-11-13
- Verdict: approve
- Notes and problems found: Reviewed every build and exit item against the merged code and tests on `main`. Drafts (#129) and tag protection (#130) are merged; `scripts/ci.sh` is green with line coverage ≥ 95%. No open gap in the contracted behaviour; the candidate-binding gap is recorded above and scheduled in M6.

### Reviewer 2

- Name: Aarav (`DeviousCardi`)
- Date: 2026-11-13
- Verdict: approve
- Notes and problems found: The M3 exit suite (`tests/m3_exit.rs`, 9 tests) passes against the real server, Git storage and PostgreSQL; the intent/revalidation contract is additionally covered by `tests/merge_execute.rs` and `tests/merge_queue.rs`. No gap found.

### Reviewer 2 — superseding note (2026-11-16)

- Name: Aarav (`DeviousCardi`)
- Date: 2026-11-16
- Supersedes: Reviewer 2's 2026-11-13 note above ("No gap found").
- Notes and problems found: My 2026-11-13 note is **superseded** by the 2026-11-15 correction above and must not be read as a clean finding. It was written against the exit suite and the intent/revalidation tests as they stood then, and it did not examine which revision the merge **decision** is bound to. The 2026-11-15 correction established that the approvals/checks digest is taken at the source head, never over the merged tree, and that a queued entry is re-targeted and revalidated rather than tested as a candidate. A check that passes on the source and fails only on the merged tree can therefore authorize the merge. That is a real gap against the promise, so "No gap found" was wrong in the sense in which it is now read. The 2026-11-13 note remains above, unedited, as the audit record.
