-- CODEOWNERS (M3): a protected branch may additionally demand that every
-- changed file with a code owner has an approving review from one of its
-- owners. The CODEOWNERS file itself is read from the target branch's tip at
-- decision time (never stored), so no new table is needed here.
ALTER TABLE branch_protection_rules
    ADD COLUMN require_code_owner_review boolean NOT NULL DEFAULT false;
