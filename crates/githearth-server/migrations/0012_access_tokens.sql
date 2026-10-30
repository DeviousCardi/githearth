-- Personal access tokens (Task: access tokens).
--
-- A token authenticates its account over the JSON API and the Git Smart HTTP
-- transport, but unlike a session it cannot satisfy fresh-auth (sudo) checks
-- and is further limited by its scopes. Only the SHA-256 of the token string
-- is stored; the plaintext is shown once at creation.
CREATE TABLE access_tokens (
 id uuid PRIMARY KEY,
 account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
 token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
 -- Scopes must name at least one of the known capabilities and nothing else.
 scopes text[] NOT NULL CHECK (
  cardinality(scopes) > 0
  AND scopes <@ ARRAY['repo:read', 'repo:write']::text[]
 ),
 created_at timestamptz NOT NULL DEFAULT now(),
 -- NULL means the token never expires on its own.
 expires_at timestamptz,
 last_used_at timestamptz,
 revoked_at timestamptz
);
CREATE INDEX access_tokens_account_id ON access_tokens(account_id);
CREATE INDEX access_tokens_revoked ON access_tokens(revoked_at) WHERE revoked_at IS NOT NULL;
