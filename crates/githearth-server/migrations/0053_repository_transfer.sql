-- Repository transfer (M4.1 U07, ADR 0003).
--
-- The transfer itself needs no new repository column: the owner is
-- `repositories.organization_id`, the name is `repositories.name`, the old
-- name is a `repository_redirects` row (which now answers 410 instead of
-- redirecting) and the revoked grants are the existing `revoked_at` columns.
-- Two things are missing for it to be safe:
--
-- 1. A name a transfer is about to take at the target must not be handed to a
--    concurrent create. The claim row records the intent inside the transfer's
--    transaction and is rolled back with a refusal. Because an uncommitted row
--    is invisible to another transaction, the reservation itself is a
--    transaction-scoped advisory lock on (organization_id, lower(name)), taken
--    by the create, rename and transfer paths; the claim is the intent record
--    and the create path also refuses a claim it can see. The create path
--    honours both in `repositories::insert_creating_impl`.
CREATE TABLE repository_claims (
 organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 name text NOT NULL CHECK (
  length(name) BETWEEN 1 AND 100
  AND name ~ '^[A-Za-z0-9._-]+$'
  AND name <> '.'
  AND name <> '..'
  AND name !~ '\.git$'
 ),
 expires_at timestamptz NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);
-- One live claim per name: of two transfers racing for the same target name,
-- the loser refuses (`409`); it never picks another name.
CREATE UNIQUE INDEX repository_claims_name ON repository_claims (organization_id, lower(name));
CREATE INDEX repository_claims_expiry ON repository_claims (expires_at);

-- 2. The source side of a transfer leaves a row in `repository_redirects`
--    pointing at a repository that no longer belongs to that organization.
--    ADR 0003 already says such a path is gone, and the create path already
--    retires the row when the name is reused. Marking where the row came from
--    keeps "moved away" distinguishable from a same-organization rename, so a
--    lookup can answer 410 for the former and redirect the latter.
ALTER TABLE repository_redirects
  ADD COLUMN transferred_at timestamptz NULL;
CREATE INDEX repository_redirects_transferred ON repository_redirects (organization_id)
  WHERE transferred_at IS NOT NULL;
