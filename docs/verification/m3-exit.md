# M3 exit-criteria verification

Date: 2026-11-11. State: **verified.** One new integration test file,
`crates/githearth-server/tests/m3_exit.rs`, proves each roadmap M3 exit
criterion against the real server, Git storage and PostgreSQL. It reuses the
fixture and pattern of `tests/merge_worker.rs`, `tests/merge_intents.rs`,
`tests/reconcile.rs` and `tests/pulls*.rs`. Every fixture is scoped to its own
repository/rows in the shared test database, so the tests are safe to run in
parallel with the rest of the suite.

## Environment

| Item | Value |
| --- | --- |
| Source revision | this branch (`m3/exit-suite`) |
| Host toolchain | rustc 1.99.1 |
| Git | 2.43.0 |
| Test database | `GITHEARTH_TEST_DATABASE_URL=postgres://postgres:pg@127.0.0.1:55467/ag_exit` |

## Commands

| Command | Result |
| --- | --- |
| `GITHEARTH_TEST_DATABASE_URL=... cargo test --test m3_exit -- --ignored` | **Passed, 9/9 tests** |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo fmt --all` | Clean |

## Criterion → test → result

| # | Exit criterion | Test (`tests/m3_exit.rs`) | Result |
| --- | --- | --- | --- |
| 1 | Concurrent merges cannot overwrite accepted history | `two_concurrent_merges_into_one_target_one_wins_and_history_is_never_rewound` | PASS — two intents race on one target; exactly one `merged`, the other `stale`, the target tip is the winner and the root commit remains an ancestor (never rewound). |
| 2 | Stale checks cannot authorize different code | `a_passing_check_on_head_a_cannot_authorize_merging_head_b` | PASS — a `success` status for head A, then a push to head B, makes the merge of B `403` with no intent queued. |
| 3 | Lost metadata after a Git commit reconciles | `lost_metadata_after_git_commit_is_reconciled_exactly_once` | PASS — the ref is moved and journaled without metadata, then journal replay records the merge exactly once (idempotent on a second replay). |
| 4 | Duplicate delivery cannot merge twice | `duplicate_delivery_merges_once_with_one_journal_entry_and_one_event` | PASS — re-delivering the same operation yields `merged` once, one `repository_pushes` row, one `ref_updates` row, one `pull_request.merged` outbox event. |
| 5 | Revocation races obey documented admission semantics | `revoking_the_merger_between_intent_and_execution_goes_stale` | PASS — revoking the merger's grant between intent creation and execution ends the intent `stale` with `actor no longer authorized` (ADR 0004). |
| 6 | UI/API exercise merge methods, conflict, cancellation | `each_supported_merge_method_merges_successfully`, `a_conflicting_merge_is_refused_without_moving_the_ref`, `a_queued_merge_can_be_cancelled_through_the_api`, `ui_merge_form_offers_methods_queues_and_cancels` | PASS — `merge`/`squash`/`rebase`/`ff_only` all merge; a conflicting candidate ends `conflicted` with the ref unmoved; a queued intent is cancelled through the API and reported on the page; the merge form offers every method and queues an intent. |
| 7 | Draft pull requests | `tests/drafts.rs` | PASS — a PR can be created as a draft; drafts refuse merge, merge-queue and auto-merge (`409 pull request is a draft`); review requests are refused on a draft; marking ready emits `pull_request.ready_for_review` and re-requests code owners; converting back emits `pull_request.converted_to_draft` and cancels queued intents/auto-merge; author-or-triage authorization with 403/404 denial paths (ADR 0009). |

## Notes

- No production code changes were required: each criterion is already satisfied
  by the merge intent, worker, journal replay and reconciler implemented for
  ADR 0004.
- Criterion 5's admission semantics are documented in
  [ADR 0004](../decisions/0004-merge-execution.md) ("Revocation race (admission
  rule)"): authorization is re-evaluated under the per-repository lock, and a
  revocation that commits after that point applies to later admissions, not the
  in-flight ref transaction.
