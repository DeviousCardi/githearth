CREATE TABLE accounts (
 id uuid PRIMARY KEY,
 username text NOT NULL UNIQUE CHECK (length(username) BETWEEN 3 AND 64),
 password_hash text NOT NULL,
 is_admin boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE sessions (
 token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
 account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 expires_at timestamptz NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_account_id ON sessions(account_id);
CREATE INDEX sessions_expiry ON sessions(expires_at);
CREATE TABLE organizations (
 id uuid PRIMARY KEY,
 slug text NOT NULL UNIQUE CHECK (length(slug) BETWEEN 1 AND 64),
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE memberships (
 organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 role text NOT NULL CHECK (role IN ('owner', 'member')),
 PRIMARY KEY (organization_id, account_id)
);
CREATE INDEX memberships_account_id ON memberships(account_id, organization_id);
CREATE TABLE audit_events (
 id uuid PRIMARY KEY,
 organization_id uuid REFERENCES organizations(id),
 actor_id uuid NOT NULL REFERENCES accounts(id),
 kind text NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE outbox_events (
 id uuid PRIMARY KEY REFERENCES audit_events(id),
 organization_id uuid REFERENCES organizations(id),
 kind text NOT NULL,
 payload jsonb NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 delivered_at timestamptz
);
