-- Record which credential authorised each push, keyed on the push's
-- operation id, so a later step can add credential_kind and credential_id
-- to the repository.pushed event (issue #67).
CREATE TABLE push_credentials (
    operation_id uuid PRIMARY KEY,
    credential_kind text NOT NULL CHECK (credential_kind IN ('password', 'token', 'ssh_key')),
    credential_id uuid NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((credential_kind = 'password') = (credential_id IS NULL))
);
CREATE INDEX push_credentials_created_at ON push_credentials (created_at);
