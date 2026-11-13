-- Tag protection (M3): per-repository rules that restrict who may create,
-- update (move) and delete matching tags. Tags carry no pull-request or
-- approval concept, so a rule is a pattern plus the minimum role allowed to
-- create a matching tag, and two switches that (by default) forbid moving and
-- deleting one. The protection generation is bumped on every rule change so
-- cached branch rulesets (and merge intents) revalidate exactly as they do for
-- branch rules.
CREATE TABLE tag_protection_rules (
  id uuid PRIMARY KEY,
  repository_id uuid NOT NULL REFERENCES repositories(id),
  pattern text NOT NULL,
  -- Minimum role allowed to create (and, when allow_update/allow_delete are
  -- set, to update/delete) a matching tag. Default 'admin' restricts creation
  -- to repository admins and organization owners.
  allowed_to_create text NOT NULL DEFAULT 'admin',
  allow_update boolean NOT NULL DEFAULT false,
  allow_delete boolean NOT NULL DEFAULT false,
  created_by uuid NOT NULL REFERENCES accounts(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (repository_id, pattern),
  CHECK (allowed_to_create IN ('admin', 'maintain', 'write', 'triage', 'read')),
  -- A pattern is a concrete tag name, or a prefix wildcard with exactly one
  -- trailing '*' (`v1.0.0`, `v*`, `release/*`). No leading '/', no '//', no
  -- trailing '/' and no '*' anywhere but as the final character. The
  -- authoritative validation is tag_protection::parse_pattern; the lookaheads
  -- below additionally reject a '.' or '..' segment so a row inserted outside
  -- the API cannot hold a pattern the engine would silently ignore.
  CHECK (
    pattern ~ '^(?!\.\.?(/|\*|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|\*|$))[A-Za-z0-9._-]+)*((/\*)|(\*))?$'
  )
);
