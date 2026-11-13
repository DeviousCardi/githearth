# ADR 0009: Draft pull requests

Date: 2026-11-13.

Status: **accepted**.

## Context

Roadmap M3 lists "drafts": a pull request that is opened before it is ready for
review. A draft must not merge, and it must not notify requested reviewers or
code owners until its author marks it ready. GitHub models this as a `draft`
flag that is orthogonal to the open/closed state, gates the merge and reviewers
panels, and automatically requests code owners when the pull request is marked
ready.

Nothing for drafts existed before this ADR: a pull request was either open or
closed, and every open pull request requested code-owner reviews and could be
merged, queued or auto-merged immediately.

## Decision

### Data model

- `pull_requests.draft` (boolean, default `false`, additive migration 0039)
  marks a pull request as not ready for review. It is independent of `state`:
  closing or merging a draft keeps whatever value it had, and the
  "one open pull per (source repository, source branch, target branch)" pair
  uniqueness is unchanged — a draft still owns its pair.

### Authorization

Marking ready and converting back to a draft use the author or the existing
`pull_request.close` (triage/write) permission, evaluated through the existing
Cedar/PBAC facts. No new action is added: the author check is an equality on
`author_id`, and a non-author falls back to the same triage decision the
close/reopen path uses. Denials map to 403 for a reader and 404 otherwise,
matching the other pull request mutations.

### Merging a draft

- `merge_intents::create_intent` refuses a draft with `409 "pull request is a
  draft"`, before any branch lookup or merge authorization, exactly where a
  closed pull request is refused.
- `auto_merge` refuses a draft the same way, and `may_merge` / `may_enable`
  report `false` so the browser hides the merge and auto-merge forms.
- Converting a pull request back to a draft cancels any queued or validating
  merge intent and any pending auto-merge, exactly as closing does. An already
  `executing` intent is settled by `execute_intent`, which revalidates and
  returns `Stale { reason: "pull request is a draft" }`. The toggle and the
  executor serialize on the repository's storage lock: `execute_intent` holds
  it across its draft re-read and the target ref compare-and-swap, and
  `set_draft` takes it before its transaction. A toggle therefore either lands
  before the executor's re-read (the executor sees the draft and returns
  `Stale`) or waits until the ref compare-and-swap has committed (the pull is
  merged, so `require_open` refuses the toggle with 409).

### Review requests and notifications

A draft does not notify reviewers or code owners. The choice made here is the
GitHub semantics rather than "accept requests but defer the notification":

- Opening a pull request as a draft skips the `codeowners.request_reviews` job;
  a push to a draft's head branch also skips re-requesting owners.
- `POST .../requested_reviewers` on a draft is refused with `409 "pull request
  is a draft"`.
- Marking a draft ready enqueues the `codeowners.request_reviews` job, so code
  owners are requested at that point (and notified through the existing
  review-request job).

The alternative — accepting review requests on a draft and holding their
notifications until it is marked ready — was rejected because it would leave a
requested reviewer named but un-notified, then require a notification sweep on
ready, adding durable state for no product benefit.

### Events

Marking ready emits the `pull_request.ready_for_review` audit + outbox event;
converting back emits `pull_request.converted_to_draft`. Both are emitted only
on an actual transition, so an idempotent replay of the same request emits
nothing and returns the current detail.

## Consequences

- A draft still accepts reviews and comments (approving a draft is not refused);
  only the merge, queue, auto-merge, review-request and code-owner paths are
  gated. This matches GitHub, where reviewers can comment on a draft.
- `execute_intent` and the code-owner job re-read the pull request, so the
  draft gate is revalidated at execution time, not only at intent-creation time.
- The `draft` flag is reported on the pull request JSON and rendered as a badge
  and the ready/convert controls in the server-rendered UI.
