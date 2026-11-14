-- Repository housekeeping results (ADR 0013).
--
-- One row per repository that has ever been maintained. The scheduler reads
-- `last_run_at` to decide which repositories are due; the job writes the sizes
-- and outcome in the same transaction as its audit event.
CREATE TABLE repository_housekeeping (
    repository_id uuid PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    bytes_before bigint NOT NULL DEFAULT 0 CHECK (bytes_before >= 0),
    bytes_after bigint NOT NULL DEFAULT 0 CHECK (bytes_after >= 0),
    expired_objects integer NOT NULL DEFAULT 0 CHECK (expired_objects >= 0),
    last_run_at timestamptz,
    last_outcome text NOT NULL DEFAULT 'never'
        CHECK (last_outcome IN ('never', 'completed', 'deferred', 'failed')),
    last_error text,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Due scan: oldest run first among active repositories.
CREATE INDEX repository_housekeeping_due ON repository_housekeeping (last_run_at);
