-- Branch protection (M3): per-repository rules that tighten the ref.write grant
-- at the ref decision, after rule matching.
CREATE TABLE branch_protection_rules (
 id uuid PRIMARY KEY,
 repository_id uuid NOT NULL REFERENCES repositories(id),
 pattern text NOT NULL,
 allow_force_push boolean NOT NULL DEFAULT false,
 allow_deletion boolean NOT NULL DEFAULT false,
 require_pull_request boolean NOT NULL DEFAULT false,
 required_approvals integer NOT NULL DEFAULT 0 CHECK (required_approvals BETWEEN 0 AND 10),
 require_checks boolean NOT NULL DEFAULT false,
 created_by uuid NOT NULL REFERENCES accounts(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (repository_id, pattern),
 -- A pattern is a concrete branch name, or a glob with exactly one trailing
 -- '*' segment-free wildcard (`main`, `release/*`). No leading '/', no '..',
 -- no '//', no trailing '/' and no '*' anywhere but as the final character.
 CHECK (
  pattern ~ '^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*(/\*)?$'
 )
);

-- One generation per repository, bumped in the same transaction as every rule
-- create/update/delete, so a cached ruleset can detect staleness.
CREATE TABLE protection_generations (
 repository_id uuid PRIMARY KEY REFERENCES repositories(id),
 generation bigint NOT NULL CHECK (generation >= 0)
);
