CREATE TABLE issues (
 id uuid PRIMARY KEY,
 repository_id uuid NOT NULL REFERENCES repositories(id),
 number integer NOT NULL,
 title text NOT NULL CHECK (length(title) BETWEEN 1 AND 256),
 body text NOT NULL DEFAULT '' CHECK (length(body) <= 65536),
 author_id uuid NOT NULL REFERENCES accounts(id),
 state text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 closed_at timestamptz NULL,
 UNIQUE (repository_id, number)
);

CREATE TABLE issue_counters (
 repository_id uuid PRIMARY KEY REFERENCES repositories(id),
 last integer NOT NULL
);

CREATE TABLE issue_comments (
 id uuid PRIMARY KEY,
 issue_id uuid NOT NULL REFERENCES issues(id),
 author_id uuid NOT NULL REFERENCES accounts(id),
 body text NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX issue_comments_issue ON issue_comments (issue_id, created_at, id);

CREATE FUNCTION check_issue_state() RETURNS trigger AS $$
BEGIN
 IF NEW.state = 'closed' THEN
  NEW.closed_at := COALESCE(NEW.closed_at, now());
 ELSE
  NEW.closed_at := NULL;
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER issues_check_state BEFORE INSERT OR UPDATE OF state, closed_at ON issues
 FOR EACH ROW EXECUTE FUNCTION check_issue_state();

CREATE FUNCTION check_issue_immutable() RETURNS trigger AS $$
BEGIN
 IF NEW.number IS DISTINCT FROM OLD.number
  OR NEW.repository_id IS DISTINCT FROM OLD.repository_id
  OR NEW.author_id IS DISTINCT FROM OLD.author_id
  OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'issue number, repository, author and creation time are immutable';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER issues_check_immutable BEFORE UPDATE ON issues
 FOR EACH ROW EXECUTE FUNCTION check_issue_immutable();
