CREATE TABLE operations (
 actor_id uuid NOT NULL REFERENCES accounts(id),
 key text NOT NULL CHECK (key ~ '^[!-~]{1,128}$'),
 request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
 status text NOT NULL CHECK (status IN ('running', 'done')),
 response_status int NULL,
 response_body jsonb NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 expires_at timestamptz NOT NULL,
 PRIMARY KEY (actor_id, key)
);
CREATE INDEX operations_expires ON operations(expires_at);

CREATE TABLE repositories (
 id uuid PRIMARY KEY,
 organization_id uuid NOT NULL REFERENCES organizations(id),
 name text NOT NULL CHECK (
  length(name) BETWEEN 1 AND 100
  AND name ~ '^[A-Za-z0-9._-]+$'
  AND name <> '.'
  AND name <> '..'
  AND name !~ '\.git$'
 ),
 description text NOT NULL DEFAULT '' CHECK (length(description) <= 1000),
 visibility text NOT NULL CHECK (visibility IN ('private', 'internal', 'public')) DEFAULT 'private',
 archived boolean NOT NULL DEFAULT false,
 status text NOT NULL CHECK (status IN ('creating', 'active', 'deleting', 'deleted', 'failed')),
 operation_id uuid NOT NULL UNIQUE,
 created_by uuid NOT NULL REFERENCES accounts(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 deleted_at timestamptz NULL
);
CREATE UNIQUE INDEX repositories_org_name_unique ON repositories (organization_id, lower(name)) WHERE status <> 'deleted';
CREATE INDEX repositories_pending ON repositories (status) WHERE status NOT IN ('active', 'deleted');

CREATE FUNCTION check_repository_status_transition() RETURNS trigger AS $$
BEGIN
 IF OLD.status = 'creating' AND NEW.status NOT IN ('active', 'failed', 'creating') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'creating may only transition to active, failed, or itself';
 END IF;
 IF OLD.status = 'active' AND NEW.status NOT IN ('deleting', 'active') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'active may only transition to deleting or itself';
 END IF;
 IF OLD.status = 'deleting' AND NEW.status NOT IN ('deleted', 'deleting') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'deleting may only transition to deleted or itself';
 END IF;
 IF OLD.status = 'failed' AND NEW.status NOT IN ('creating', 'deleting', 'failed') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'failed may only transition to creating, deleting, or itself';
 END IF;
 IF OLD.status = 'deleted' AND NEW.status <> 'deleted' THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'deleted may not transition';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER repositories_check_transition BEFORE UPDATE OF status ON repositories
 FOR EACH ROW EXECUTE FUNCTION check_repository_status_transition();

CREATE TABLE repository_redirects (
 organization_id uuid NOT NULL REFERENCES organizations(id),
 name text NOT NULL,
 repository_id uuid NOT NULL REFERENCES repositories(id),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX repository_redirects_lookup ON repository_redirects (organization_id, lower(name));
