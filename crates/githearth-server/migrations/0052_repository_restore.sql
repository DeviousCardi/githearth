-- Repository restore from trash (M4.1 U23c, ADR 0003 revision).
--
-- A deleted repository is restored in place: `deleted -> restoring -> active`.
-- A failed directory move goes back to `deleted`, so the operator can retry;
-- a restore that follows a purge or hits a name collision never leaves the row
-- stuck in `restoring`.
--
-- Every other transition is unchanged from 0003. A forward migration: 0003
-- is not edited, so an installation that has already applied it gets the same
-- rules by replacing the function and the CHECK constraint in place.

ALTER TABLE repositories DROP CONSTRAINT repositories_status_check;
ALTER TABLE repositories ADD CONSTRAINT repositories_status_check CHECK (
 status IN ('creating', 'active', 'deleting', 'deleted', 'failed', 'restoring')
);

CREATE OR REPLACE FUNCTION check_repository_status_transition() RETURNS trigger AS $$
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
 IF OLD.status = 'deleted' AND NEW.status NOT IN ('deleted', 'restoring') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'deleted may only transition to restoring or itself';
 END IF;
 IF OLD.status = 'restoring' AND NEW.status NOT IN ('deleted', 'restoring', 'active') THEN
  RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'restoring may only transition to active, deleted, or itself';
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
