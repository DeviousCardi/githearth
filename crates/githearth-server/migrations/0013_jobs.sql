-- Durable job runner (ADR 0006).
CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    organization_id uuid REFERENCES organizations(id) ON DELETE CASCADE,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    dedupe_key text NOT NULL,
    state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','running','done','dead')),
    not_before timestamptz NOT NULL DEFAULT now(),
    attempt integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 5,
    lease_holder uuid,
    lease_expires_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_claimable ON jobs(not_before) WHERE state = 'pending';
CREATE INDEX jobs_running_expiry ON jobs(lease_expires_at) WHERE state = 'running';
CREATE UNIQUE INDEX jobs_dedupe_active ON jobs(kind, dedupe_key) WHERE state IN ('pending','running');
