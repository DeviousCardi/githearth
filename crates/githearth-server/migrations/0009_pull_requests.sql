-- Pull requests share the issue number sequence (issue_counters), so an
-- issue and a pull request never have the same number in one repository.
CREATE TABLE pull_requests (
 id uuid PRIMARY KEY,
 repository_id uuid NOT NULL REFERENCES repositories(id),
 number integer NOT NULL,
 title text NOT NULL CHECK (length(title) BETWEEN 1 AND 256),
 body text NOT NULL DEFAULT '' CHECK (length(body) <= 65536),
 author_id uuid NOT NULL REFERENCES accounts(id),
 -- Branch names without the refs/heads/ prefix; both in this repository.
 source_branch text NOT NULL CHECK (length(source_branch) BETWEEN 1 AND 255),
 target_branch text NOT NULL CHECK (length(target_branch) BETWEEN 1 AND 255),
 state text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 closed_at timestamptz NULL,
 UNIQUE (repository_id, number),
 CHECK (source_branch <> target_branch)
);
CREATE UNIQUE INDEX pull_requests_one_open_per_pair
 ON pull_requests (repository_id, source_branch, target_branch) WHERE state = 'open';

CREATE TRIGGER pull_requests_check_state BEFORE INSERT OR UPDATE OF state, closed_at ON pull_requests
 FOR EACH ROW EXECUTE FUNCTION check_issue_state();

CREATE FUNCTION check_pull_request_immutable() RETURNS trigger AS $$
BEGIN
 IF NEW.number IS DISTINCT FROM OLD.number
  OR NEW.repository_id IS DISTINCT FROM OLD.repository_id
  OR NEW.author_id IS DISTINCT FROM OLD.author_id
  OR NEW.source_branch IS DISTINCT FROM OLD.source_branch
  OR NEW.target_branch IS DISTINCT FROM OLD.target_branch
  OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'pull request number, repository, author, branches and creation time are immutable';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER pull_requests_check_immutable BEFORE UPDATE ON pull_requests
 FOR EACH ROW EXECUTE FUNCTION check_pull_request_immutable();

-- A comment is either general or anchored to a line of a file at an exact
-- commit; replies join their parent's thread and carry no anchor.
CREATE TABLE pull_request_comments (
 id uuid PRIMARY KEY,
 pull_request_id uuid NOT NULL REFERENCES pull_requests(id),
 author_id uuid NOT NULL REFERENCES accounts(id),
 body text NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
 seq integer NOT NULL,
 commit_id text NULL CHECK (commit_id ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 path text NULL CHECK (length(path) BETWEEN 1 AND 4096),
 line integer NULL CHECK (line >= 1),
 in_reply_to uuid NULL REFERENCES pull_request_comments(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (pull_request_id, seq),
 CHECK ((commit_id IS NULL) = (path IS NULL) AND (path IS NULL) = (line IS NULL)),
 CHECK (in_reply_to IS NULL OR commit_id IS NULL)
);

CREATE TABLE pull_request_comment_counters (
 pull_request_id uuid PRIMARY KEY REFERENCES pull_requests(id),
 last integer NOT NULL
);

-- Every review names the head commit it was given on. Only reviews at the
-- current head count, so pushing new commits makes earlier approvals stale.
CREATE TABLE pull_request_reviews (
 id uuid PRIMARY KEY,
 seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
 pull_request_id uuid NOT NULL REFERENCES pull_requests(id),
 reviewer_id uuid NOT NULL REFERENCES accounts(id),
 verdict text NOT NULL CHECK (verdict IN ('approved', 'changes_requested', 'commented')),
 commit_id text NOT NULL CHECK (commit_id ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 body text NOT NULL DEFAULT '' CHECK (length(body) <= 65536),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX pull_request_reviews_head ON pull_request_reviews (pull_request_id, commit_id, reviewer_id, seq);

CREATE FUNCTION forbid_review_change() RETURNS trigger AS $$
BEGIN
 RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'reviews are append-only';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER pull_request_reviews_append_only BEFORE UPDATE OR DELETE ON pull_request_reviews
 FOR EACH ROW EXECUTE FUNCTION forbid_review_change();
