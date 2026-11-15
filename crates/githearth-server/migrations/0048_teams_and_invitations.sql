-- Teams and invitations (M4.1 acceptance cases U05 and U06).
--
-- Teams, team membership and grants already exist (migrations 0002 and 0004);
-- this migration adds only what is missing:
--
-- - `invitations`: one row per invitation, addressed to an organization and
--   optionally a team, carrying the invited email address, its organization
--   role, the SHA-256 of the token (never the token), an expiry, and the
--   single-use and revocation times. Acceptance creates the membership and the
--   team membership in the same transaction as the grant, and the row records
--   who accepted it (`used_by`), so an accepted invitation can be listed but
--   never replayed.
-- - Revoking a team's grants, a team member and the team itself stay ordinary
--   UPDATE and DELETE statements: grants are revoked, never deleted, and the
--   triggers from 0002/0004 already forbid editing them.

CREATE TABLE invitations (
  id uuid PRIMARY KEY,
  organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
  team_id uuid NULL REFERENCES teams(id) ON DELETE CASCADE,
  email text NOT NULL CHECK (length(email) BETWEEN 3 AND 320 AND email = lower(email)),
  role text NOT NULL CHECK (role IN ('member', 'owner')),
  token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
  invited_by uuid NOT NULL REFERENCES accounts(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  -- The expiry is stored as given; it may be pulled forward (never pushed
  -- back, see `check_invitation_update`), so it cannot be tied to created_at.
  expires_at timestamptz NOT NULL,
  used_at timestamptz NULL,
  used_by uuid NULL REFERENCES accounts(id),
  revoked_at timestamptz NULL,
  revoked_by uuid NULL REFERENCES accounts(id),
  CHECK ((used_at IS NULL) = (used_by IS NULL)),
  CHECK ((revoked_at IS NULL) = (revoked_by IS NULL)),
  CHECK (used_at IS NULL OR revoked_at IS NULL)
);
CREATE INDEX invitations_organization_id ON invitations(organization_id, created_at DESC);
CREATE INDEX invitations_email ON invitations(email) WHERE used_at IS NULL AND revoked_at IS NULL;

-- The invited team must belong to the invited organization, like every other
-- reference a grant makes.
CREATE FUNCTION check_invitation_team() RETURNS trigger AS $$
BEGIN
  IF NEW.team_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM teams WHERE id = NEW.team_id AND organization_id = NEW.organization_id
  ) THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'invitation team must belong to the invitation organization';
  END IF;
  RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER invitations_check_team BEFORE INSERT OR UPDATE OF team_id, organization_id ON invitations
  FOR EACH ROW EXECUTE FUNCTION check_invitation_team();

-- Who, what and where are fixed at creation; the expiry may only be shortened,
-- so an operator can expire an unredeemed invitation early but never extend
-- one that lapsed. An accepted invitation is final, and revocation is one-way:
-- `used_by`, `revoked_at` and `revoked_by` may be set once and can never be
-- cleared or rewritten, so no `UPDATE` can un-revoke a live invitation or
-- change who accepted it.
CREATE FUNCTION check_invitation_update() RETURNS trigger AS $$
BEGIN
  IF NEW.organization_id IS DISTINCT FROM OLD.organization_id
    OR NEW.team_id IS DISTINCT FROM OLD.team_id
    OR NEW.email IS DISTINCT FROM OLD.email
    OR NEW.role IS DISTINCT FROM OLD.role
    OR NEW.token_hash IS DISTINCT FROM OLD.token_hash
    OR NEW.invited_by IS DISTINCT FROM OLD.invited_by
    OR NEW.created_at IS DISTINCT FROM OLD.created_at
    OR NEW.expires_at > OLD.expires_at THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'invitations are created, accepted or revoked, never edited';
  END IF;
  IF OLD.used_at IS NOT NULL AND NEW.used_at IS DISTINCT FROM OLD.used_at THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'an invitation is accepted once';
  END IF;
  -- Revocation is one-way and the acceptance record is final: a later UPDATE
  -- may not clear a revocation or rewrite who accepted the invitation.
  IF OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'a revoked invitation stays revoked';
  END IF;
  IF OLD.revoked_by IS NOT NULL AND NEW.revoked_by IS DISTINCT FROM OLD.revoked_by THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'a revoked invitation stays revoked';
  END IF;
  IF OLD.used_by IS NOT NULL AND NEW.used_by IS DISTINCT FROM OLD.used_by THEN
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'an invitation is accepted once';
  END IF;
  RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE TRIGGER invitations_check_update BEFORE UPDATE ON invitations
  FOR EACH ROW EXECUTE FUNCTION check_invitation_update();
