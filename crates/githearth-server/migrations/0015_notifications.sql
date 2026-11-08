-- In-app notifications (ADR 0006, Person A side).
CREATE TABLE notifications (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('pull_request.merged', 'pull_request.reviewed')),
    event_id uuid NOT NULL,
    pull_request_id uuid NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    actor_id uuid REFERENCES accounts(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    read_at timestamptz,
    UNIQUE (event_id, account_id)
);
CREATE INDEX notifications_account_created ON notifications (account_id, created_at DESC);
