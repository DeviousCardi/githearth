# M3 requirement accounting and sign-off

Date: 2026-11-13. State: **accounted for; awaiting two reviewer sign-offs.** This
document maps every M3 item in [04-forge-roadmap.md](../plan/04-forge-roadmap.md#m3--reviews-and-safe-merges)
— the build list *and* the exit list — to the code and tests that implement it,
with a status per item. It is the Phase 5 gate ("every M3 requirement is
accounted for") from [08-two-person-roadmap.md](../plan/08-two-person-roadmap.md).

Each claim below was verified by reading the named source and test files; nothing
is asserted from memory. Status is one of:

- **verified** — implemented in code and covered by an executable test.
- **in PR** — being built on a parallel branch, not merged here.

Two items are excluded from this branch by the phase plan: **drafts** (branch
`m3/drafts`) and **tag protection** (branch `m3/tag-protection`). They are marked
**in PR** below and are deliberately not touched by this work.

## Build list → code + tests

| # | M3 build item | Code | Tests | Status |
| --- | --- | --- | --- | --- |
| 1 | Pull/merge requests | `crates/githearth-server/src/pulls.rs` (API), `pulls_ui.rs` (UI), migration `0009_pull_requests.sql` | `tests/pulls.rs`, `tests/pulls_ui.rs` | verified |
| 2 | Comparisons | `crates/githearth-git/src/browse.rs` (`compare`), `crates/githearth-server/src/browse.rs` (`/compare/{spec}`), `browse_ui.rs` | `tests/browse.rs` (`compare_*`), `tests/browse_ui.rs` | verified |
| 3 | Threaded inline reviews | `pulls.rs` (comments with `commit_id`/`path`/`line`/`side`/`in_reply_to`), migration `0035_inline_review_comments.sql` | `tests/pulls.rs` (`threads_resolve_and_unresolve`, `replies_attach_to_the_root_not_to_another_reply`, `inline_comments_carry_side_and_turn_outdated`), `tests/pulls_ui.rs` | verified |
| 4 | Suggestions | `crates/githearth-git/src/suggestion.rs`, `pulls.rs` (`/comments/{id}/apply`) | `crates/githearth-git/tests/suggestion.rs`, `tests/suggestions.rs`, `tests/pulls_ui.rs` (`inline_comment_renders_and_applies_a_suggestion`) | verified |
| 5 | Approvals / change requests | `pulls.rs` (reviews, verdicts `approved`/`changes_requested`/`commented`) | `tests/pulls.rs` (`approval_counts_only_at_the_current_head`, `latest_verdict_per_reviewer_wins`), `tests/merge_intents.rs` (`digest_changes_when_a_change_request_arrives_at_the_same_head`) | verified |
| 6 | Drafts | PR #129 (branch `m3/drafts`) | — | in PR |
| 7 | Reviewers / code owners | `pulls.rs` (review requests), `codeowners.rs`, migrations `0018`/`0020`/`0034` | `tests/review_requests.rs`, `tests/codeowners.rs` | verified |
| 8 | Checks | `statuses.rs`, migrations `0023_commit_statuses.sql`, `0033_merge_checks.sql` | `tests/statuses.rs` | verified |
| 9 | Branch protection | `protection.rs`, migration `0010_branch_protection.sql` | `tests/protection.rs`, `tests/git_push.rs` | verified |
| 9b | Tag protection | PR #130 (branch `m3/tag-protection`) | — | in PR |
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
| E1 | Merge intents bind source/target OIDs, candidate result, checks/approvals, policy generation and operation ID | `merge_intents.rs` (`create_intent` records `source_oid`, `expected_target_oid`, `policy_generation`, `checks_approvals_digest`, `operation_id`; `record_merge_result` records `result_oid`) | `tests/merge_intents.rs`, `tests/m3_exit.rs` | verified |
| E2 | Expected-target mutation; duplicate workers reuse the outcome | `merge.rs` (`execute_intent` compare-and-swap via `githearth_git::merge::update_ref_expected`, journal lookup), `merge_worker.rs` (lease) | `tests/merge_execute.rs` (`second_call_returns_already_merged_without_a_second_ref_update`, `two_concurrent_calls_for_one_intent_merge_exactly_once`), `tests/merge_worker.rs` | verified |
| E3 | Changed tips, permissions and policies require revalidation | `merge.rs` (`execute_intent` re-checks pull state, `source_oid`, `expected_target_oid`, `policy_generation`, actor grant, approvals/checks digest) | `tests/merge_execute.rs` (`stale_cases_return_stale_and_leave_the_ref`, `a_required_check_flipping_after_the_intent_is_stale`), `tests/m3_exit.rs` (`revoking_the_merger_between_intent_and_execution_goes_stale`) | verified |
| E4 | Fork contributions | `forks.rs`, `pulls.rs` (`source_repository_id`), `merge.rs` (cross-repo fetch), migration `0038` | `tests/forks.rs` (`cross_repo_pull_requests_merge_by_each_method`, `merge_intent_revalidates_recorded_refs`) | verified |
| E5 | Stale approvals/checks | `merge_intents.rs` (`dismiss_stale_approvals`), `statuses.rs` (status bound to a commit), migration `0037` | `tests/merge_intents.rs` (`stale_approval_does_not_count_by_default`, `stale_approval_counts_when_dismissal_is_off`, `a_status_for_a_different_sha_never_authorizes_the_merge`), `tests/merge_queue.rs` (`a_stale_approval_cannot_authorize_a_different_head_in_the_queue`) | verified |
| X1 | Concurrent merges cannot overwrite accepted history | `merge.rs` + `merge_worker.rs` (per-repository lock + compare-and-swap) | `tests/m3_exit.rs` (`two_concurrent_merges_into_one_target_one_wins_and_history_is_never_rewound`) | verified |
| X2 | Stale checks cannot authorize different code | `merge_intents.rs` + `statuses.rs` (digest binds checks to the source oid) | `tests/m3_exit.rs` (`a_passing_check_on_head_a_cannot_authorize_merging_head_b`) | verified |
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
- No additional production gap was found beyond drafts and tag protection, which
  are in PR #129 and PR #130 respectively and out of scope for this branch.

## Sign-off

Two reviewers each confirm the tables above. Leave fields blank until the review
is performed.

### Reviewer 1

- Name:
- Date:
- Verdict (approve / request changes):
- Notes and problems found:

### Reviewer 2

- Name:
- Date:
- Verdict (approve / request changes):
- Notes and problems found:
