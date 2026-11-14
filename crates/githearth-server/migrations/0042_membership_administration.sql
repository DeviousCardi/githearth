-- Membership administration (M4.1 acceptance case U04).
--
-- Membership rows gain the time the account joined, which the members API and
-- page show and which the audit trail (organization.member_added) records
-- alongside. Existing rows get the migration time, which is the only fact
-- available for them; nothing else about a membership changes.

ALTER TABLE memberships ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
