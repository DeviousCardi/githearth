-- Find the current CHECK constraint name for scope_kind before dropping it.
DO $$ DECLARE
    cn text;
BEGIN
    SELECT con.conname INTO cn
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    WHERE rel.relname = 'grants' AND con.contype = 'c' AND pg_get_constraintdef(con.oid) LIKE '%scope_kind%';
    EXECUTE format('ALTER TABLE grants DROP CONSTRAINT %I', cn);
END $$;

ALTER TABLE grants ADD COLUMN repository_id uuid NULL REFERENCES repositories(id);

ALTER TABLE grants ADD CONSTRAINT grants_scope_kind_check CHECK (
    scope_kind IN ('organization', 'repository')
    AND (scope_kind = 'repository') = (repository_id IS NOT NULL)
);

CREATE OR REPLACE FUNCTION check_grant_insert() RETURNS trigger AS $$
BEGIN
    IF NEW.role NOT IN ('read', 'triage', 'write', 'maintain', 'admin', 'member', 'owner') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant role must be a built-in role';
    END IF;
    IF NEW.scope_kind = 'repository' AND NEW.role IN ('member', 'owner') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'role member and owner are not allowed on repositories';
    END IF;
    IF NEW.scope_kind = 'repository' THEN
        IF NOT EXISTS (
            SELECT 1 FROM repositories WHERE id = NEW.repository_id AND organization_id = NEW.scope_id AND status <> 'deleted'
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'granted repository must belong to the scope organization and not be deleted';
        END IF;
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

CREATE OR REPLACE FUNCTION check_grant_update() RETURNS trigger AS $$
BEGIN
    IF NEW.role IS DISTINCT FROM OLD.role
        OR NEW.subject_kind IS DISTINCT FROM OLD.subject_kind
        OR NEW.subject_id IS DISTINCT FROM OLD.subject_id
        OR NEW.scope_kind IS DISTINCT FROM OLD.scope_kind
        OR NEW.scope_id IS DISTINCT FROM OLD.scope_id
        OR NEW.repository_id IS DISTINCT FROM OLD.repository_id
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

CREATE INDEX grants_repository_id_active ON grants(repository_id) WHERE revoked_at IS NULL AND repository_id IS NOT NULL;
