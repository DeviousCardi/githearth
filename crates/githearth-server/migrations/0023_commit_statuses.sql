-- Commit statuses/checks (M3): an append-only per-commit status log keyed by
-- context, plus the list of required status check contexts on branch
-- protection. A status records the exact commit it was posted for, so a
-- status for one commit can never authorize a merge of a different commit.

CREATE TABLE commit_statuses (
  id uuid PRIMARY KEY,
  repository_id uuid NOT NULL REFERENCES repositories(id),
  commit_id text NOT NULL CHECK (commit_id ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  context text NOT NULL CHECK (length(context) BETWEEN 1 AND 255),
  state text NOT NULL CHECK (state IN ('pending', 'success', 'failure', 'error')),
  description text NULL CHECK (description IS NULL OR length(description) <= 65536),
  target_url text NULL CHECK (target_url IS NULL OR length(target_url) <= 4096),
  creator_id uuid NOT NULL REFERENCES accounts(id),
  seq bigint GENERATED ALWAYS AS IDENTITY,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Newest first for a commit; the latest-per-context reads use the context
-- variant with DISTINCT ON (context) ... ORDER BY context, seq DESC.
CREATE INDEX commit_statuses_sha ON commit_statuses (repository_id, commit_id, seq DESC);
CREATE INDEX commit_statuses_sha_context
  ON commit_statuses (repository_id, commit_id, context, seq DESC);

-- Statuses are append-only: a CI system reports successive results, and the
-- latest per context is what counts. No update or delete is ever legitimate.
CREATE FUNCTION forbid_commit_status_change() RETURNS trigger AS $$
BEGIN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'commit statuses are append-only';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER commit_statuses_append_only
  BEFORE UPDATE OR DELETE ON commit_statuses
  FOR EACH ROW EXECUTE FUNCTION forbid_commit_status_change();

-- Branch protection: the bare `require_checks` toggle becomes the list of
-- required status check contexts. A non-empty list means checks are required;
-- the merge path treats the rule as requiring every listed context to be
-- `success` on the pull request's current head commit.
ALTER TABLE branch_protection_rules DROP COLUMN require_checks;
ALTER TABLE branch_protection_rules
  ADD COLUMN required_status_checks text[] NOT NULL DEFAULT '{}';
