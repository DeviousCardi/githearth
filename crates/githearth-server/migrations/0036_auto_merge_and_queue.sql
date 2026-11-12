-- Auto-merge and merge queue (M3).
--
-- Auto-merge is a per-pull-request flag: an account with merge permission
-- chooses a merge method, and the server raises a merge intent automatically
-- once the pull request becomes mergeable. A merge queue entry is a merge
-- intent whose target branch protection demands `require_merge_queue`; the
-- worker re-targets such an intent against the current target tip instead of
-- failing it when an earlier entry merged and moved the target.

ALTER TABLE merge_intents
  ADD COLUMN in_queue boolean NOT NULL DEFAULT false;

ALTER TABLE branch_protection_rules
  ADD COLUMN require_merge_queue boolean NOT NULL DEFAULT false;

-- One auto-merge per pull request. `source_oid` is the head recorded when
-- auto-merge was enabled, so a later head change (by anyone but the enabler)
-- cancels it. `enabled_by` is the account whose permission is re-checked on
-- every trigger and again at execution time.
CREATE TABLE auto_merges (
 pull_request_id uuid PRIMARY KEY REFERENCES pull_requests(id),
 method text NOT NULL CHECK (method IN ('merge', 'squash', 'rebase', 'ff_only')),
 source_oid text NOT NULL CHECK (source_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
 enabled_by uuid NOT NULL REFERENCES accounts(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX merge_intents_queue ON merge_intents (created_at)
  WHERE in_queue AND state IN ('queued', 'validating', 'executing');
