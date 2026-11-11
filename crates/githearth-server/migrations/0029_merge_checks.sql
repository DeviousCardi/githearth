-- Mergeability cache (M3 conflicts): whether a pull request head merges
-- cleanly into its target tip for one enabled merge method. Keyed by
-- (head oid, target oid) so a new head or target recomputes instead of
-- serving a stale answer; `method` keeps merge/squash/rebase distinct.
CREATE TABLE merge_checks (
    repository_id uuid NOT NULL REFERENCES repositories(id),
    head_oid text NOT NULL CHECK (head_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    target_oid text NOT NULL CHECK (target_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
    method text NOT NULL CHECK (method IN ('merge', 'squash', 'rebase')),
    status text NOT NULL CHECK (status IN ('clean', 'conflicts', 'unrelated', 'up_to_date')),
    conflicts text[] NOT NULL DEFAULT '{}',
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, head_oid, target_oid, method)
);
