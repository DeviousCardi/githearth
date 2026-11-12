-- Branch protection gains two flags (M3).
--
-- `dismiss_stale_approvals` (default true) keeps the existing semantics: an
-- approval is counted only at the current head, so a push that moves the head
-- makes earlier approvals stop counting. Existing rules keep those semantics
-- because the column defaults to true.
--
-- `require_signed_commits` (default false) refuses pushes to a protected
-- branch unless every commit the push introduces carries an SSH signature that
-- verifies against a key registered to the committer's account.
ALTER TABLE branch_protection_rules
 ADD COLUMN dismiss_stale_approvals boolean NOT NULL DEFAULT true;
ALTER TABLE branch_protection_rules
 ADD COLUMN require_signed_commits boolean NOT NULL DEFAULT false;
