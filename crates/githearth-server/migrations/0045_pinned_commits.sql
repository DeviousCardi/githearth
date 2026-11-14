-- Pull-request head pins (ADR 0013 "What is live", 7).
--
-- A pin keeps one commit -- and everything it reaches -- out of a
-- housekeeping prune even when no ref reaches it. It is created in the same
-- transaction that opens a pull request, in the repository that owns the head
-- (the source repository for a fork pull request). Pins are rows, not Git
-- refs: they are never advertised and no client can fetch them.
--
-- `retain_until` is the retention timestamp. An open pull request is always
-- kept (the housekeeping run re-pins its current head); a closed or merged one
-- stays readable until that timestamp, then its objects are ordinary
-- unreachable objects again.
CREATE TABLE pinned_commits (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id uuid NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_id uuid NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    commit_oid text NOT NULL
        CHECK (commit_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    reason text NOT NULL
        CHECK (reason IN ('pull_request_head', 'backfill')),
    retain_until timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (pull_request_id, commit_oid)
);

-- The keep-set query reads every live pin of one repository.
CREATE INDEX pinned_commits_repository ON pinned_commits (repository_id, retain_until);
CREATE INDEX pinned_commits_pull ON pinned_commits (pull_request_id);

-- Backfill: every head OID a pull request already recorded. The merge intent
-- and auto-merge rows hold the head a merge was validated against; a review
-- records the commit it reviewed. A pull request with none of these is pinned
-- by its next housekeeping run from the live branch tip. The backfill uses the
-- default 30-day window; later pins and refreshes use
-- `housekeeping_pin_retention_days`.
INSERT INTO pinned_commits (repository_id, pull_request_id, commit_oid, reason, retain_until)
SELECT pr.source_repository_id, pr.id, head.oid, 'backfill', now() + interval '30 days'
FROM pull_requests pr
JOIN LATERAL (
    SELECT source_oid AS oid FROM merge_intents m
    WHERE m.pull_request_id = pr.id
    UNION
    SELECT source_oid FROM auto_merges a WHERE a.pull_request_id = pr.id
    UNION
    SELECT commit_id FROM pull_request_reviews r WHERE r.pull_request_id = pr.id
) head ON true
ON CONFLICT (pull_request_id, commit_oid) DO NOTHING;
