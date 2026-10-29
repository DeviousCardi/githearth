-- Merge intents (M3, Person A side of ADR 0004).
--
-- A merge intent is the durable, application-side record that stands in for an
-- atomic commit across PostgreSQL and Git. The Git side (`execute_intent`,
-- Person B) moves the ref and journals the transaction; this table records the
-- validated intent, its state machine, and (via record_merge_result) the merged
-- outcome once the journal record is replayed.

-- `pull_requests.state` gains `merged`; `merged_at`/`merged_by` are set by
-- record_merge_result in the same transaction as the intent transition.
ALTER TABLE pull_requests
 DROP CONSTRAINT pull_requests_state_check;
ALTER TABLE pull_requests
 ADD CONSTRAINT pull_requests_state_check
  CHECK (state IN ('open', 'closed', 'merged'));
ALTER TABLE pull_requests
 ADD COLUMN merged_at timestamptz NULL,
 ADD COLUMN merged_by uuid NULL REFERENCES accounts(id);

CREATE TABLE merge_intents (
 operation_id uuid PRIMARY KEY,
 repository_id uuid NOT NULL REFERENCES repositories(id),
 pull_request_id uuid NOT NULL REFERENCES pull_requests(id),
 source_ref text NOT NULL CHECK (length(source_ref) BETWEEN 1 AND 255),
 source_oid text NOT NULL CHECK (source_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 target_ref text NOT NULL CHECK (length(target_ref) BETWEEN 1 AND 255),
 expected_target_oid text NOT NULL CHECK (expected_target_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 method text NOT NULL CHECK (method IN ('merge', 'squash', 'rebase', 'ff_only')),
 candidate_oid text NULL CHECK (candidate_oid IS NULL OR candidate_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 policy_generation bigint NOT NULL CHECK (policy_generation >= 0),
 checks_approvals_digest text NOT NULL,
 actor_id uuid NOT NULL REFERENCES accounts(id),
 state text NOT NULL DEFAULT 'queued'
  CHECK (state IN ('queued', 'validating', 'executing', 'merged', 'conflicted', 'stale', 'failed', 'cancelled')),
 lease_holder uuid NULL,
 lease_attempt integer NULL CHECK (lease_attempt IS NULL OR lease_attempt >= 0),
 lease_expires_at timestamptz NULL,
 result_oid text NULL CHECK (result_oid IS NULL OR result_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 journal_sequence bigint NULL CHECK (journal_sequence IS NULL OR journal_sequence > 0),
 failure_reason text NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);

-- At most one non-terminal intent per pull request. A merged (or otherwise
-- terminal) intent stays as history; a new intent may then be raised.
CREATE UNIQUE INDEX merge_intents_one_non_terminal_per_pull
 ON merge_intents (pull_request_id)
 WHERE state IN ('queued', 'validating', 'executing');

CREATE INDEX merge_intents_repository ON merge_intents (repository_id);
CREATE INDEX merge_intents_repository_seq ON merge_intents (repository_id, journal_sequence)
 WHERE journal_sequence IS NOT NULL;

-- The stable ordering of intents for one pull request, for "latest intent" reads.
CREATE INDEX merge_intents_pull_created ON merge_intents (pull_request_id, created_at DESC);
