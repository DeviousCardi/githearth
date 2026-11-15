-- Pins for the rest of the keep-set U11 names (ADR 0013 "What is live", 7).
--
-- 0045 pinned pull-request heads only. The plan's keep-set is every object ID
-- PostgreSQL references: a pull request's head *and base*, review-comment
-- commits and commit statuses, alongside merge candidates. This migration
-- gives `pinned_commits` the subject columns those kinds need and backfills
-- the rows that already exist. 0045 is not edited: it has been applied.
--
-- Every pin stores the commit it keeps in `commit_oid`, whichever kind it is,
-- and every new subject column is nullable, so the keep-set query keeps its
-- shape: read the live `commit_oid`s of one repository. `retain_until` NULL
-- means "while the subject row exists" (a comment pin, the latest status of
-- a context); a timestamp means the pin is live while it is in the future.
-- The keep-set query tests both, so no background task has to keep a live pin
-- alive.

-- Where a fork pull request's head was copied into its target, and how many
-- bytes that has cost. The target's own `objects/` is what its quota measures;
-- this ledger is the attribution and the audit trail for the per-fork fetch
-- cap (`housekeeping_fork_fetch_limit_bytes`). The read path writes it; nothing
-- reads it to decide a request.
CREATE TABLE repository_fork_fetches (
    repository_id uuid NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    source_repository_id uuid NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    bytes bigint NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    fetches bigint NOT NULL DEFAULT 0 CHECK (fetches >= 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, source_repository_id)
);

-- `pull_request_id` was NOT NULL: only a head pin had one. A base pin names
-- its pull request in its own column, and a status pin may belong to none.
ALTER TABLE pinned_commits ALTER COLUMN pull_request_id DROP NOT NULL;
ALTER TABLE pinned_commits ALTER COLUMN retain_until DROP NOT NULL;

ALTER TABLE pinned_commits
    ADD COLUMN base_of_pull_request_id uuid NULL
        REFERENCES pull_requests(id) ON DELETE CASCADE,
    ADD COLUMN review_comment_id uuid NULL
        REFERENCES pull_request_comments(id) ON DELETE CASCADE,
    ADD COLUMN commit_status_id uuid NULL
        REFERENCES commit_statuses(id) ON DELETE CASCADE,
    ADD COLUMN review_commit_oid text NULL
        CHECK (review_commit_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$');

-- One pin per subject. A fork pull request has a head pin in its source and a
-- second one in its target for the same commit, so 0045's
-- `UNIQUE (pull_request_id, commit_oid)` becomes unique *per repository*: the
-- target-side row must not collide with the source's. 0045 is not edited; its
-- constraint is replaced here, and the head upsert moves to the new key.
ALTER TABLE pinned_commits
    DROP CONSTRAINT pinned_commits_pull_request_id_commit_oid_key;
ALTER TABLE pinned_commits
    ADD CONSTRAINT pinned_commits_one_head_per_repository
    UNIQUE (repository_id, pull_request_id, commit_oid);
CREATE UNIQUE INDEX pinned_commits_base ON pinned_commits (base_of_pull_request_id)
    WHERE base_of_pull_request_id IS NOT NULL;
CREATE UNIQUE INDEX pinned_commits_review_comment ON pinned_commits (review_comment_id)
    WHERE review_comment_id IS NOT NULL;
CREATE UNIQUE INDEX pinned_commits_status ON pinned_commits (commit_status_id)
    WHERE commit_status_id IS NOT NULL;
-- A reviewed commit has no per-row subject (reviews carry no id this table
-- could reference before 0046); one pin per (repository, reviewed commit)
-- serves every review of that commit while the repository holds them.
CREATE UNIQUE INDEX pinned_commits_review_commit
    ON pinned_commits (repository_id, review_commit_oid)
    WHERE review_commit_oid IS NOT NULL;

-- Columns and reason must describe exactly one kind, so a row can neither
-- claim two subjects nor name none.
ALTER TABLE pinned_commits DROP CONSTRAINT pinned_commits_reason_check;
ALTER TABLE pinned_commits
    ADD CONSTRAINT pinned_commits_reason_check CHECK (reason IN (
        'pull_request_head', 'pull_request_base', 'review_commit',
        'review_comment', 'commit_status', 'backfill')),
    ADD CONSTRAINT pinned_commits_subject_kind CHECK (
        ((reason IN ('pull_request_head', 'backfill')
            AND pull_request_id IS NOT NULL)
            OR (reason = 'pull_request_base'
                AND pull_request_id IS NULL AND base_of_pull_request_id IS NOT NULL)
            OR (reason = 'review_commit'
                AND pull_request_id IS NULL AND review_commit_oid IS NOT NULL)
            OR (reason = 'review_comment'
                AND pull_request_id IS NULL AND review_comment_id IS NOT NULL)
            OR (reason = 'commit_status'
                AND pull_request_id IS NULL AND commit_status_id IS NOT NULL))
        AND base_of_pull_request_id IS NULL
            = (reason <> 'pull_request_base')
        AND commit_status_id IS NULL = (reason <> 'commit_status')
        AND (reason = 'review_comment') = (review_comment_id IS NOT NULL)
        AND (reason = 'review_commit') = (review_commit_oid IS NOT NULL));

-- Backfill 1 (pull-request bases) is not here: a pull request row does not
-- record its base commit and there is no branch table to join, so the base is
-- read from Git and backfilled by `pins::backfill_bases` at server start,
-- before the scheduler can enqueue a run. It covers every open request and
-- every closed or merged one inside the retention window, and records in
-- `pull_request_base_backfill_skips` (0047) the ones whose target branch is
-- gone. Every request opened after this migration is pinned by its own create
-- transaction.

-- Backfill 2: every anchored review comment. Pinned while the comment exists.
-- A comment pin carries no `pull_request_id`: 0045's
-- `UNIQUE (pull_request_id, commit_oid)` belongs to head pins, and a comment's
-- pull request is reachable through its own row.
INSERT INTO pinned_commits
    (repository_id, review_comment_id, commit_oid, reason, retain_until)
SELECT pr.repository_id, c.id, c.commit_id, 'review_comment', NULL
FROM pull_request_comments c
JOIN pull_requests pr ON pr.id = c.pull_request_id
WHERE c.commit_id IS NOT NULL
ON CONFLICT DO NOTHING;

-- Backfill 2b: every reviewed commit (0045 pinned review commits under a
-- pull request's head; this is the kind-owned row). Pinned while the review
-- exists, so no timestamp.
INSERT INTO pinned_commits
    (repository_id, review_commit_oid, commit_oid, reason, retain_until)
SELECT pr.source_repository_id, r.commit_id, r.commit_id, 'review_commit', NULL
FROM pull_request_reviews r
JOIN pull_requests pr ON pr.id = r.pull_request_id
ON CONFLICT DO NOTHING;

-- Backfill 3: the latest status of each (repository, commit, context), the
-- one that still counts. A superseded status is not pinned on upgrade; the
-- retention after supersession applies to rows written from now on.
INSERT INTO pinned_commits
    (repository_id, commit_status_id, commit_oid, reason, retain_until)
SELECT s.repository_id, s.id, s.commit_id, 'commit_status', NULL
FROM (
    SELECT DISTINCT ON (repository_id, commit_id, context) id
    FROM commit_statuses
    ORDER BY repository_id, commit_id, context, seq DESC
) latest
JOIN commit_statuses s ON s.id = latest.id
ON CONFLICT DO NOTHING;
