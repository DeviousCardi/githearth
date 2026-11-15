-- Requests whose base pin the start-up backfill could not write.
--
-- A pull request row never stored its base commit and 0046 cannot read Git, so
-- `housekeeping::backfill_bases` pins the target branch's current tip for every
-- request that predates pins. A target branch that was deleted before the
-- upgrade has no tip: the request's base cannot be reconstructed, and nothing
-- else protects it from the first prune. That request is recorded here with
-- its state and the reason, so an operator can see which bases must be rescued
-- before maintenance runs, and `githearth-server doctor` names them instead of
-- the request disappearing in silence.
--
-- One row per pull request (the backfill re-writes it on each start); it
-- cascades with the request, and the backfill deletes it when a later pass does
-- write the pin.
CREATE TABLE pull_request_base_backfill_skips (
    pull_request_id uuid PRIMARY KEY REFERENCES pull_requests(id) ON DELETE CASCADE,
    repository_id uuid NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    state text NOT NULL,
    target_branch text NOT NULL,
    reason text NOT NULL CHECK (reason IN ('target_branch_missing')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX pull_request_base_backfill_skips_repository
    ON pull_request_base_backfill_skips (repository_id);
