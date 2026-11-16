-- Per-account and per-organization data keys and crypto-shredding
-- (M11a increment 8A, case U36; ADR 0015 "Deletion", ADR 0014 §3).
--
-- 0056 is M4.2's envelope and is the last migration on main. This branch takes
-- the next free number, 0057; the identity-schema lane takes 0058 so the two
-- never share a number and apply in version order on a fresh install.
--
-- A `secret_data_keys` row whose `owner_key_id` is set wraps its 32-byte data
-- key under that owner's data key instead of directly under the installation
-- key. The owner data key is itself wrapped by the installation key and is
-- kept in operator custody outside PostgreSQL and outside every archive (see
-- `data_keys.rs` and docs/operations/secrets-at-rest.md). Deleting the
-- operator-custody key therefore makes every secret under it unreadable, in
-- the live database and in every backup taken before the deletion, while the
-- account/organization row and its plaintext profile columns remain.
--
-- `owner_kind` and `owner_id` make the owner explicit so a backup manifest can
-- name the key without key material, and so a key cannot silently be used for
-- another account. All three columns are NULL for installation-level secrets
-- (the audit IP key), which are exactly the "pre-key" rows a pre-key backup
-- cannot shred.
ALTER TABLE secret_data_keys
    ADD COLUMN owner_kind text,
    ADD COLUMN owner_id uuid,
    ADD COLUMN owner_key_id uuid;

ALTER TABLE secret_data_keys
    ADD CONSTRAINT secret_data_keys_owner_pair CHECK (
        (owner_kind IS NULL) = (owner_id IS NULL)
        AND (owner_kind IS NULL) = (owner_key_id IS NULL)
    ),
    ADD CONSTRAINT secret_data_keys_owner_kind CHECK (
        owner_kind IS NULL OR owner_kind IN ('account', 'organization')
    );

CREATE INDEX secret_data_keys_owner ON secret_data_keys(owner_kind, owner_id);
CREATE INDEX secret_data_keys_owner_key ON secret_data_keys(owner_key_id);
