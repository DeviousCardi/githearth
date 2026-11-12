-- Forks and cross-repository pull requests (M3 "fork contributions").

-- A fork records the repository it was forked from; NULL means "not a fork".
-- The parent is only ever soft-deleted (status = 'deleted'), so the foreign key
-- can stay plain (no cascade): a deleted parent keeps its history readable.
ALTER TABLE repositories
  ADD COLUMN parent_repository_id uuid NULL REFERENCES repositories(id);
CREATE INDEX repositories_parent_repository
  ON repositories (parent_repository_id) WHERE parent_repository_id IS NOT NULL;

-- Cross-repository pull requests: the head branch lives in
-- `source_repository_id` (a fork of the target); the base branch lives in
-- `repository_id`. Same-repository pulls keep both equal (backfilled below).
ALTER TABLE pull_requests
  ADD COLUMN source_repository_id uuid REFERENCES repositories(id);
UPDATE pull_requests SET source_repository_id = repository_id;
ALTER TABLE pull_requests ALTER COLUMN source_repository_id SET NOT NULL;

-- A same-repository pull must keep its two branch names distinct; a
-- cross-repository pull may use the same branch name on both sides (the fork's
-- "main" targeting the parent's "main"). Drop the old same-name check and
-- replace it with one that only applies to same-repository pulls.
DO $$ DECLARE
    cn text;
BEGIN
    -- Scope to the current schema: a shared test database may hold several
    -- migrated schemas, each with its own `pull_requests` table and a
    -- `pull_requests_distinct_branches` check that also contains
    -- `source_branch <> target_branch`. Without the namespace filter the DO
    -- block would match a foreign schema's constraint and try to drop it here.
    SELECT con.conname INTO cn
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace ns ON ns.oid = rel.relnamespace
    WHERE ns.nspname = current_schema()
      AND rel.relname = 'pull_requests' AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) LIKE '%source_branch <> target_branch%'
    ORDER BY con.conname
    LIMIT 1;
    IF cn IS NOT NULL THEN
        EXECUTE format('ALTER TABLE pull_requests DROP CONSTRAINT %I', cn);
    END IF;
END $$;
ALTER TABLE pull_requests ADD CONSTRAINT pull_requests_distinct_branches
  CHECK (source_repository_id <> repository_id OR source_branch <> target_branch);

-- Rebuild the one-open-per-pair uniqueness across the source repository too.
DROP INDEX pull_requests_one_open_per_pair;
CREATE UNIQUE INDEX pull_requests_one_open_per_pair
  ON pull_requests (repository_id, source_repository_id, source_branch, target_branch)
  WHERE state = 'open';

-- The source repository of a pull request is immutable, like its branches.
CREATE FUNCTION check_pull_request_source_repo_immutable() RETURNS trigger AS $$
BEGIN
  IF NEW.source_repository_id IS DISTINCT FROM OLD.source_repository_id THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation',
      MESSAGE = 'pull request source repository is immutable';
  END IF;
  RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER pull_requests_check_source_repo_immutable BEFORE UPDATE ON pull_requests
  FOR EACH ROW EXECUTE FUNCTION check_pull_request_source_repo_immutable();

-- Why a pull request was closed: NULL for a normal author/triage close, or a
-- short reason when the system closes it (a fork being deleted or made
-- inaccessible). Reported on the pull request and in the close audit event.
ALTER TABLE pull_requests ADD COLUMN closed_reason text NULL;
