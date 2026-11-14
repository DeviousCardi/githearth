-- Account recovery (M4.1, acceptance case U17).
--
-- An administrator reset never hands over a usable password. It sets
-- `must_change_password` and issues a single-use code whose SHA-256 digest is
-- stored here; the plaintext code is shown to the administrator once and never
-- logged. Until the account redeems the code and stores a new password, every
-- credential path (sign-in, bearer sessions, access tokens, SSH keys and Git
-- admission) refuses it.
ALTER TABLE accounts ADD COLUMN must_change_password boolean NOT NULL DEFAULT false;

CREATE TABLE password_reset_tokens (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  -- SHA-256 of the code. The code itself is never stored.
  token_hash bytea NOT NULL UNIQUE,
  expires_at timestamptz NOT NULL,
  used_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
-- Redemption looks up one live token for an account; expiry sweeps are by age.
CREATE INDEX password_reset_tokens_account ON password_reset_tokens(account_id)
  WHERE used_at IS NULL;
CREATE INDEX password_reset_tokens_expiry ON password_reset_tokens(expires_at);
