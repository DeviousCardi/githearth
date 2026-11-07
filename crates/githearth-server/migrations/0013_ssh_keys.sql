-- SSH public keys for Git transport (ADR 0005, Person A).
--
-- A key is registered by its SHA-256 fingerprint (what the SSH listener passes
-- into `ssh_keys::authenticate`) and maps to exactly one account. Only the
-- canonical OpenSSH line (re-serialized without the comment) is stored, never a
-- private key. A key can be revoked; this sets `revoked_at`, so:
--   - a revoked key fails authentication like an unknown one, and
--   - the fingerprint stays UNIQUE across all accounts, including revoked rows,
--     so a revoked key must be explicitly deleted before the same key can be
--     re-added (by anyone).
CREATE TABLE ssh_keys (
 id uuid PRIMARY KEY,
 account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
 algorithm text NOT NULL,
 public_key text NOT NULL,
 fingerprint text NOT NULL UNIQUE,
 created_at timestamptz NOT NULL DEFAULT now(),
 -- NULL means the key never expires on its own.
 expires_at timestamptz,
 last_used_at timestamptz,
 revoked_at timestamptz
);
CREATE INDEX ssh_keys_account_id ON ssh_keys(account_id);
