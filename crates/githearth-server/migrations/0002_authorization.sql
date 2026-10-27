ALTER TABLE accounts ADD COLUMN suspended boolean NOT NULL DEFAULT false;
ALTER TABLE organizations ADD COLUMN suspended boolean NOT NULL DEFAULT false;
ALTER TABLE sessions ADD COLUMN authenticated_at timestamptz NOT NULL DEFAULT now();

CREATE TABLE teams (
 id uuid PRIMARY KEY,
 organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 parent_id uuid REFERENCES teams(id),
 slug text NOT NULL CHECK (length(slug) BETWEEN 1 AND 64),
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (organization_id, slug)
);
CREATE FUNCTION check_team_parent() RETURNS trigger AS $$
DECLARE
 depth integer;
 cycles integer;
BEGIN
 IF NEW.parent_id IS NULL THEN
  RETURN NEW;
 END IF;
 IF NOT EXISTS (
  SELECT 1 FROM teams p WHERE p.id = NEW.parent_id AND p.organization_id = NEW.organization_id
 ) THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'parent team must be in the same organization';
 END IF;
 WITH RECURSIVE ancestors AS (
  SELECT id, parent_id FROM teams WHERE id = NEW.parent_id
  UNION ALL
  SELECT t.id, t.parent_id FROM teams t JOIN ancestors a ON t.id = a.parent_id
 )
 SELECT count(*), count(*) FILTER (WHERE id = NEW.id) INTO depth, cycles FROM ancestors;
 IF cycles > 0 THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'team parent must not create a cycle';
 END IF;
 IF depth + 1 > 8 THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'team nesting must be at most 8 levels';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER teams_check_parent BEFORE INSERT OR UPDATE OF parent_id, organization_id ON teams
 FOR EACH ROW EXECUTE FUNCTION check_team_parent();

CREATE TABLE team_members (
 team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
 account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 PRIMARY KEY (team_id, account_id)
);
CREATE INDEX team_members_account_id ON team_members(account_id);
CREATE FUNCTION check_team_member() RETURNS trigger AS $$
BEGIN
 IF NOT EXISTS (
  SELECT 1 FROM memberships m JOIN teams t ON t.organization_id = m.organization_id
  WHERE t.id = NEW.team_id AND m.account_id = NEW.account_id
 ) THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'team member must be a member of the team organization';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER team_members_check_member BEFORE INSERT OR UPDATE OF team_id, account_id ON team_members
 FOR EACH ROW EXECUTE FUNCTION check_team_member();

CREATE TABLE grants (
 id uuid PRIMARY KEY,
 role text NOT NULL,
 subject_kind text NOT NULL CHECK (subject_kind IN ('account', 'team')),
 subject_id uuid NOT NULL,
 scope_kind text NOT NULL CHECK (scope_kind = 'organization'),
 scope_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 expires_at timestamptz NULL,
 granted_by uuid NOT NULL REFERENCES accounts(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 revoked_at timestamptz NULL,
 revoked_by uuid NULL REFERENCES accounts(id),
 CHECK ((revoked_at IS NULL) = (revoked_by IS NULL))
);
CREATE INDEX grants_subject_subject_id ON grants(subject_kind, subject_id) WHERE revoked_at IS NULL;
CREATE FUNCTION check_grant_insert() RETURNS trigger AS $$
BEGIN
 IF NEW.role NOT IN ('read', 'triage', 'write', 'maintain', 'admin', 'member', 'owner') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant role must be a built-in role';
 END IF;
 CASE NEW.subject_kind
  WHEN 'account' THEN
   IF NOT EXISTS (
    SELECT 1 FROM memberships WHERE account_id = NEW.subject_id AND organization_id = NEW.scope_id
   ) THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant subject account must be a member of the scope organization';
   END IF;
  WHEN 'team' THEN
   IF NOT EXISTS (
    SELECT 1 FROM teams WHERE id = NEW.subject_id AND organization_id = NEW.scope_id
   ) THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant subject team must belong to the scope organization';
   END IF;
 END CASE;
 IF NEW.expires_at IS NOT NULL AND NEW.expires_at <= clock_timestamp() THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant expiry must be in the future';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER grants_check_insert BEFORE INSERT ON grants
 FOR EACH ROW EXECUTE FUNCTION check_grant_insert();
CREATE FUNCTION check_grant_update() RETURNS trigger AS $$
BEGIN
 IF NEW.role IS DISTINCT FROM OLD.role
  OR NEW.subject_kind IS DISTINCT FROM OLD.subject_kind
  OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
  OR NEW.scope_kind IS DISTINCT FROM OLD.scope_kind
  OR NEW.scope_id IS DISTINCT FROM OLD.scope_id
  OR NEW.expires_at IS DISTINCT FROM OLD.expires_at
  OR NEW.granted_by IS DISTINCT FROM OLD.granted_by
  OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grants are granted or revoked, never edited';
 END IF;
 IF OLD.revoked_at IS NOT NULL OR OLD.revoked_by IS NOT NULL
  OR NEW.revoked_at IS NULL OR NEW.revoked_by IS NULL THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grants may be revoked only once';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER grants_check_update BEFORE UPDATE ON grants
 FOR EACH ROW EXECUTE FUNCTION check_grant_update();
CREATE FUNCTION check_grant_delete() RETURNS trigger AS $$
BEGIN
 IF EXISTS (SELECT 1 FROM organizations WHERE id = OLD.scope_id) THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grants are never deleted, only revoked';
 END IF;
 RETURN NULL;
END $$ LANGUAGE plpgsql;
CREATE CONSTRAINT TRIGGER grants_check_delete AFTER DELETE ON grants
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_grant_delete();

INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by)
SELECT gen_random_uuid(), m.role, 'account', m.account_id, 'organization', m.organization_id, m.account_id
FROM memberships m;

CREATE TABLE authz_events (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 seq bigint UNIQUE,
 kind text NOT NULL,
 subject_id uuid NOT NULL,
 organization_id uuid NULL,
 at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE authz_event_counter (
 singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
 value bigint NOT NULL
);
INSERT INTO authz_event_counter (value) VALUES (0);

CREATE FUNCTION emit_authz_event() RETURNS trigger AS $$
DECLARE
 subject uuid;
 organization uuid;
BEGIN
 CASE TG_TABLE_NAME
  WHEN 'accounts' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END;
  WHEN 'organizations' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END;
  WHEN 'memberships' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.account_id ELSE NEW.account_id END;
  WHEN 'teams' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.id ELSE NEW.id END;
  WHEN 'team_members' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.account_id ELSE NEW.account_id END;
  WHEN 'grants' THEN subject := CASE WHEN TG_OP = 'DELETE' THEN OLD.subject_id ELSE NEW.subject_id END;
 END CASE;
 CASE TG_TABLE_NAME
  WHEN 'organizations' THEN organization := subject;
  WHEN 'memberships' THEN organization := CASE WHEN TG_OP = 'DELETE' THEN OLD.organization_id ELSE NEW.organization_id END;
  WHEN 'teams' THEN organization := CASE WHEN TG_OP = 'DELETE' THEN OLD.organization_id ELSE NEW.organization_id END;
  WHEN 'team_members' THEN organization := (
   SELECT organization_id FROM teams WHERE id = (CASE WHEN TG_OP = 'DELETE' THEN OLD.team_id ELSE NEW.team_id END)
  );
  WHEN 'grants' THEN organization := CASE WHEN TG_OP = 'DELETE' THEN OLD.scope_id ELSE NEW.scope_id END;
  ELSE organization := NULL;
 END CASE;
 INSERT INTO authz_events (kind, subject_id, organization_id)
 VALUES (TG_ARGV[0], subject, organization);
 RETURN NULL;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER accounts_event AFTER UPDATE OF suspended, is_admin ON accounts
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('account');
CREATE TRIGGER organizations_event AFTER UPDATE OF suspended ON organizations
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('organization');
CREATE TRIGGER memberships_event AFTER INSERT OR UPDATE OR DELETE ON memberships
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('membership');
CREATE TRIGGER teams_event AFTER INSERT OR UPDATE OR DELETE ON teams
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('team');
CREATE TRIGGER team_members_event AFTER INSERT OR UPDATE OR DELETE ON team_members
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('team_member');
CREATE TRIGGER grants_event AFTER INSERT OR UPDATE OR DELETE ON grants
 FOR EACH ROW EXECUTE FUNCTION emit_authz_event('grant');

CREATE FUNCTION assign_authz_event_seq() RETURNS trigger AS $$
DECLARE
 assigned bigint;
BEGIN
 UPDATE authz_event_counter SET value = value + 1 RETURNING value INTO assigned;
 UPDATE authz_events SET seq = assigned WHERE id = NEW.id;
 PERFORM pg_notify('authz_events', assigned::text);
 RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE CONSTRAINT TRIGGER authz_events_assign_seq AFTER INSERT ON authz_events
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION assign_authz_event_seq();
