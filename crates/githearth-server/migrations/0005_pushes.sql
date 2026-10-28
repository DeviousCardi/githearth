ALTER TABLE repositories ADD COLUMN journal_applied bigint NOT NULL DEFAULT 0;

CREATE TABLE repository_pushes (
 repository_id uuid NOT NULL REFERENCES repositories(id),
 journal_sequence bigint NOT NULL CHECK (journal_sequence > 0),
 operation_id uuid NOT NULL,
 actor_id uuid NOT NULL REFERENCES accounts(id),
 applied_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (repository_id, journal_sequence)
);

CREATE TABLE ref_updates (
 repository_id uuid NOT NULL,
 journal_sequence bigint NOT NULL,
 ref_name text NOT NULL CHECK (ref_name ~ '^refs/(heads|tags)/.+'),
 old_oid text NOT NULL CHECK (old_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 new_oid text NOT NULL CHECK (new_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 PRIMARY KEY (repository_id, journal_sequence, ref_name),
 FOREIGN KEY (repository_id, journal_sequence) REFERENCES repository_pushes
);
