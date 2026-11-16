-- M4.2 increment 7B: token subjects and non-account principals
-- (docs/decisions/0014-platform-slice.md section 2).
--
-- Until now every access token was a row on an account and every audit actor
-- was an account. This migration separates the three token subjects:
--
--   * account        — a person's token (the only subject that existed);
--   * service_account — an organization-owned non-human identity;
--   * run             — one CI run, resolving to the run and the account that
--                       triggered it.
--
-- Existing rows are preserved exactly in effect: every current token stays an
-- account token, keeps its scopes and expiry, and is marked `legacy = true`
-- with the pinned legacy deadline (the first instant of the UTC day after
-- 2027-06-30, i.e. refused from 2027-07-01T00:00:00Z).  The audit chain is rewritten once with the V4
-- canonical encoding (which now covers actor kind and the actor's account) and
-- then re-verified in `seq` order under the same link check `audit::verify`
-- performs.
--
-- This file has never been released. An earlier unreleased revision rehashed
-- each chain in `created_at, id` order and renumbered `seq` to match; on a chain
-- whose `created_at` order differed from its `seq` order (two concurrent
-- transactions can produce that) it permuted `seq` and broke the chain, and
-- overwrote the only record of the original order. Such a database is not
-- repairable by re-running this migration: restore it from a backup. The
-- version below never renumbers and never permutes `seq`.

-- ---------------------------------------------------------------------------
-- Service accounts and the run stub
-- ---------------------------------------------------------------------------

-- An organization-owned non-human identity. M4.3 owns its administration;
-- M4.2 owns the principal, its tokens/keys and admission.
CREATE TABLE service_accounts (
 id uuid PRIMARY KEY,
 organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
 created_by uuid NOT NULL REFERENCES accounts(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 disabled_at timestamptz
);
CREATE INDEX service_accounts_organization ON service_accounts (organization_id);

-- Runs arrive in M6. This is the stub record M4.2's run-token subject resolves
-- against: a run belongs to one organization, is triggered by one account, and
-- is over when `ended_at` is set (or its status is not `running`).
CREATE TABLE ci_runs (
 id uuid PRIMARY KEY,
 organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
 triggered_by uuid NOT NULL REFERENCES accounts(id),
 status text NOT NULL DEFAULT 'running'
  CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
 started_at timestamptz NOT NULL DEFAULT now(),
 ended_at timestamptz,
 CHECK ((status = 'running') = (ended_at IS NULL))
);
CREATE INDEX ci_runs_triggered_by ON ci_runs (triggered_by);

-- ---------------------------------------------------------------------------
-- Token subjects
-- ---------------------------------------------------------------------------

ALTER TABLE access_tokens
 ADD COLUMN subject_kind text NOT NULL DEFAULT 'account'
  CHECK (subject_kind IN ('account', 'service_account', 'run')),
 ADD COLUMN service_account_id uuid REFERENCES service_accounts(id) ON DELETE CASCADE,
 ADD COLUMN run_id uuid REFERENCES ci_runs(id) ON DELETE CASCADE,
 -- Canonical resolved resource selectors, or NULL for a legacy token.
 ADD COLUMN resource_set jsonb,
 -- 'api', 'git' and/or 'registry'. Legacy tokens keep both transports.
 ADD COLUMN audience text[] NOT NULL DEFAULT ARRAY['api', 'git']::text[],
 -- True for the pre-M4.2 two-scope tokens; see the deprecation deadline.
 ADD COLUMN legacy boolean NOT NULL DEFAULT true,
 ADD COLUMN legacy_deadline timestamptz;

-- Every existing token is legacy, keeps its scopes/expiry, and expires (as a
-- credential) at the legacy cutoff: the first instant of the UTC day after the
-- configured date, so the date an operator reads is the date enforced and the
-- refusal instant is exactly the next UTC midnight. The predicate in both
-- lookups is `legacy_deadline <= now()`, so a row is refused at and after this
-- instant and accepted for every microsecond of the stated day. An explicit
-- `AT TIME ZONE 'UTC'` keeps the value from moving with the session time zone.
UPDATE access_tokens
   SET legacy = true,
       legacy_deadline = ('2027-06-30'::date + 1)::timestamp AT TIME ZONE 'UTC';

ALTER TABLE access_tokens ALTER COLUMN account_id DROP NOT NULL;

ALTER TABLE access_tokens ADD CONSTRAINT access_tokens_subject_check CHECK (
 (subject_kind = 'account'
   AND account_id IS NOT NULL AND service_account_id IS NULL AND run_id IS NULL)
 OR (subject_kind = 'service_account'
   AND account_id IS NULL AND service_account_id IS NOT NULL AND run_id IS NULL)
 OR (subject_kind = 'run'
   AND account_id IS NULL AND service_account_id IS NULL AND run_id IS NOT NULL)
);

ALTER TABLE access_tokens ADD CONSTRAINT access_tokens_audience_check CHECK (
 cardinality(audience) > 0
 AND audience <@ ARRAY['api', 'git', 'registry']::text[]
);

ALTER TABLE access_tokens ADD CONSTRAINT access_tokens_resource_set_check CHECK (
 resource_set IS NULL OR jsonb_typeof(resource_set) = 'array'
);

-- A legacy token is an account token with no resource set and only the two
-- historical scopes. Every other token must carry a resource set and a finite
-- expiry.
--
-- Scope to the current schema: a shared test database may hold several
-- migrated schemas, each with its own `access_tokens` table and a scopes
-- constraint. Without the namespace filter the query would find another
-- schema's constraint and `DROP CONSTRAINT` would fail with 42704.
DO $$ DECLARE
    cn text;
BEGIN
    SELECT con.conname INTO cn
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace ns ON ns.oid = rel.relnamespace
    WHERE ns.nspname = current_schema()
      AND rel.relname = 'access_tokens' AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) LIKE '%scopes%'
    ORDER BY con.conname
    LIMIT 1;
    IF cn IS NOT NULL THEN
        EXECUTE format('ALTER TABLE access_tokens DROP CONSTRAINT %I', cn);
    END IF;
END $$;
ALTER TABLE access_tokens ADD CONSTRAINT access_tokens_scopes_check CHECK (
 cardinality(scopes) > 0
 AND scopes <@ ARRAY[
  'repo:read', 'repo:write', 'package:read', 'package:write'
 ]::text[]
);
ALTER TABLE access_tokens ADD CONSTRAINT access_tokens_legacy_check CHECK (
 (legacy
   AND subject_kind = 'account'
   AND resource_set IS NULL
   AND legacy_deadline IS NOT NULL
   AND scopes <@ ARRAY['repo:read', 'repo:write']::text[])
 OR (NOT legacy
   AND resource_set IS NOT NULL
   AND expires_at IS NOT NULL)
);

CREATE INDEX access_tokens_service_account ON access_tokens (service_account_id)
 WHERE service_account_id IS NOT NULL;
CREATE INDEX access_tokens_run ON access_tokens (run_id) WHERE run_id IS NOT NULL;
CREATE INDEX access_tokens_legacy ON access_tokens (legacy_deadline) WHERE legacy;

-- ---------------------------------------------------------------------------
-- Service-account SSH keys
-- ---------------------------------------------------------------------------

ALTER TABLE ssh_keys
 ADD COLUMN subject_kind text NOT NULL DEFAULT 'account'
  CHECK (subject_kind IN ('account', 'service_account')),
 ADD COLUMN service_account_id uuid REFERENCES service_accounts(id) ON DELETE CASCADE;
ALTER TABLE ssh_keys ALTER COLUMN account_id DROP NOT NULL;
ALTER TABLE ssh_keys ADD CONSTRAINT ssh_keys_subject_check CHECK (
 (subject_kind = 'account' AND account_id IS NOT NULL AND service_account_id IS NULL)
 OR (subject_kind = 'service_account' AND account_id IS NULL AND service_account_id IS NOT NULL)
);
CREATE INDEX ssh_keys_service_account ON ssh_keys (service_account_id)
 WHERE service_account_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Grants may name a service account
-- ---------------------------------------------------------------------------

ALTER TABLE grants DROP CONSTRAINT grants_subject_kind_check;
ALTER TABLE grants ADD CONSTRAINT grants_subject_kind_check CHECK (
 subject_kind IN ('account', 'team', 'service_account')
);

CREATE OR REPLACE FUNCTION check_grant_insert() RETURNS trigger AS $$
BEGIN
    IF NEW.role NOT IN ('read', 'triage', 'write', 'maintain', 'admin', 'member', 'owner') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant role must be a built-in role';
    END IF;
    IF NEW.scope_kind = 'repository' AND NEW.role IN ('member', 'owner') THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'role member and owner are not allowed on repositories';
    END IF;
    IF NEW.scope_kind = 'repository' THEN
        IF NOT EXISTS (
            SELECT 1 FROM repositories WHERE id = NEW.repository_id AND organization_id = NEW.scope_id AND status <> 'deleted'
        ) THEN
            RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'granted repository must belong to the scope organization and not be deleted';
        END IF;
    END IF;
    CASE NEW.subject_kind
        WHEN 'account' THEN
            IF NOT EXISTS (
                SELECT 1 FROM memberships WHERE account_id = NEW.subject_id AND organization_id = NEW.scope_id
            ) THEN
                RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant subject account must be a member of the scope organization';
            END IF;
        WHEN 'team' THEN
            IF NOT EXISTS (
                SELECT 1 FROM teams WHERE id = NEW.subject_id AND organization_id = NEW.scope_id
            ) THEN
                RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant subject team must belong to the scope organization';
            END IF;
        WHEN 'service_account' THEN
            IF NOT EXISTS (
                SELECT 1 FROM service_accounts WHERE id = NEW.subject_id AND organization_id = NEW.scope_id
            ) THEN
                RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant subject service account must belong to the scope organization';
            END IF;
    END CASE;
    IF NEW.expires_at IS NOT NULL AND NEW.expires_at <= clock_timestamp() THEN
        RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'grant expiry must be in the future';
    END IF;
    RETURN NEW;
END $$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------------------
-- Audit actors
-- ---------------------------------------------------------------------------

ALTER TABLE audit_events
 ADD COLUMN actor_kind text NOT NULL DEFAULT 'account'
  CHECK (actor_kind IN ('account', 'service_account', 'run')),
 -- The account an actor acts for: the account itself for an account actor, NULL
 -- for a service account, and the triggering account for a run.
 ADD COLUMN actor_account_id uuid REFERENCES accounts(id) ON DELETE RESTRICT;

-- `actor_id` now names whichever principal row the kind selects, so it cannot
-- keep a foreign key to accounts.
ALTER TABLE audit_events DROP CONSTRAINT audit_events_actor_id_fkey;
-- The append-only update guard is lifted for this one migration; it is
-- recreated below with the new columns included in its comparison.
DROP TRIGGER audit_events_allow_ip_prune ON audit_events;
UPDATE audit_events SET actor_account_id = actor_id WHERE actor_kind = 'account';

-- An audit row cannot contradict its own actor: an account actor is its own
-- account, a service account carries no account, and a run names the account
-- that triggered it. Without this an actor could be two subjects, or none.
ALTER TABLE audit_events ADD CONSTRAINT audit_events_actor_check CHECK (
 (actor_kind = 'account' AND actor_account_id = actor_id)
 OR (actor_kind = 'service_account' AND actor_account_id IS NULL)
 OR (actor_kind = 'run' AND actor_account_id IS NOT NULL)
);

-- The V4 canonical encoding extends V1/V2/V3 with the actor kind and the
-- actor's account. Everything after those two fields is byte-for-byte the
-- previous encoding, so the verifier can mirror one function.
CREATE FUNCTION audit_canonical_v4(
 id uuid, organization_id uuid, actor_id uuid, actor_kind text, actor_account_id uuid,
 kind text, seq bigint, created_at timestamptz,
 client_ip inet, key_id uuid, digest bytea
) RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
 SELECT uuid_send(id)
     || uuid_send(COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'))
     || uuid_send(actor_id)
     || int8send(octet_length(actor_kind)) || convert_to(actor_kind, 'UTF8')
     || uuid_send(COALESCE(actor_account_id, '00000000-0000-0000-0000-000000000000'))
     || int8send(octet_length(kind)) || convert_to(kind, 'UTF8')
     || int8send(seq)
     || int8send((EXTRACT(EPOCH FROM created_at) * 1000000)::bigint)
     || CASE
          WHEN digest IS NOT NULL THEN uuid_send(key_id) || digest
          WHEN client_ip IS NOT NULL THEN
            int8send(octet_length(host(client_ip))) || convert_to(host(client_ip), 'UTF8')
          ELSE ''::bytea
        END
$$;

CREATE OR REPLACE FUNCTION audit_assign() RETURNS trigger AS $$
DECLARE
 c uuid;
BEGIN
 -- An account actor acts as itself. Service account actors have no account;
 -- run actors carry their triggering account from the writer.
 IF NEW.actor_kind = 'account' AND NEW.actor_account_id IS NULL THEN
  NEW.actor_account_id := NEW.actor_id;
 END IF;
 -- `actor_id` has no foreign key any more (it names one of three tables), so
 -- check the referent here. The CHECK constraint below covers the shape; this
 -- covers the existence, which the dropped FK used to do for accounts.
 IF NEW.actor_kind = 'account'
    AND NOT EXISTS (SELECT 1 FROM accounts WHERE id = NEW.actor_id) THEN
  RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
   MESSAGE = 'audit actor account does not exist';
 ELSIF NEW.actor_kind = 'service_account'
    AND NOT EXISTS (SELECT 1 FROM service_accounts WHERE id = NEW.actor_id) THEN
  RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
   MESSAGE = 'audit actor service account does not exist';
 ELSIF NEW.actor_kind = 'run'
    AND NOT EXISTS (SELECT 1 FROM ci_runs WHERE id = NEW.actor_id) THEN
  RAISE EXCEPTION USING ERRCODE = 'foreign_key_violation',
   MESSAGE = 'audit actor run does not exist';
 END IF;
 c := COALESCE(NEW.organization_id, '00000000-0000-0000-0000-000000000000');
 INSERT INTO audit_chain (chain, last_seq, last_hash)
 VALUES (c, 0, decode(repeat('00', 32), 'hex'))
 ON CONFLICT (chain) DO NOTHING;
 SELECT last_seq + 1, last_hash INTO NEW.seq, NEW.prev_hash
   FROM audit_chain WHERE audit_chain.chain = c FOR UPDATE;
 NEW.hash := sha256(
   NEW.prev_hash || audit_canonical_v4(
     NEW.id, NEW.organization_id, NEW.actor_id, NEW.actor_kind, NEW.actor_account_id,
     NEW.kind, NEW.seq, NEW.created_at, NEW.client_ip, NEW.client_ip_key_id, NEW.client_ip_digest)
 );
 UPDATE audit_chain SET last_seq = NEW.seq, last_hash = NEW.hash WHERE audit_chain.chain = c;
 RETURN NEW;
END $$ LANGUAGE plpgsql;

-- Rehash every existing chain under V4, in the order the chain was written
-- (`seq`), which is the order `audit::verify` reads. The rows keep their `seq`
-- — this block never renumbers — so `outbox_events (chain, seq)` keeps pointing
-- at the same event, and only prev_hash/hash change.
--
-- `created_at` is not a safe order: it defaults to the transaction start time
-- while `seq` is assigned under the chain lock in commit order, so two
-- concurrent transactions can invert them. Ordering by (created_at, id) was the
-- defect in the unreleased first revision of this file.
DO $$
DECLARE
 r RECORD;
 c uuid;
 prev_c uuid := NULL;
 prev_h bytea := decode(repeat('00', 32), 'hex');
 new_h bytea;
BEGIN
 IF EXISTS (SELECT 1 FROM audit_events WHERE seq IS NULL) THEN
  RAISE EXCEPTION 'audit_events has a row with no seq; refusing to rehash an incomplete chain';
 END IF;
 FOR r IN
  SELECT COALESCE(organization_id, '00000000-0000-0000-0000-000000000000') AS chain,
         id, organization_id, actor_id, actor_kind, actor_account_id, kind, seq, created_at,
         client_ip, client_ip_key_id, client_ip_digest
  FROM audit_events
  ORDER BY chain, seq
 LOOP
  c := r.chain;
  IF c IS DISTINCT FROM prev_c THEN
   prev_h := decode(repeat('00', 32), 'hex');
   prev_c := c;
  END IF;
  new_h := sha256(prev_h || audit_canonical_v4(
    r.id, r.organization_id, r.actor_id, r.actor_kind, r.actor_account_id,
    r.kind, r.seq, r.created_at, r.client_ip, r.client_ip_key_id, r.client_ip_digest));
  UPDATE audit_events SET prev_hash = prev_h, hash = new_h WHERE id = r.id;
  prev_h := new_h;
 END LOOP;

 -- The chain head is the last row in seq order, which is what verify checks.
 UPDATE audit_chain SET last_seq = 0, last_hash = decode(repeat('00', 32), 'hex');
 FOR r IN
  SELECT DISTINCT ON (COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'))
         COALESCE(organization_id, '00000000-0000-0000-0000-000000000000') AS chain,
         seq, hash
  FROM audit_events
  ORDER BY COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'), seq DESC
 LOOP
  UPDATE audit_chain SET last_seq = r.seq, last_hash = r.hash WHERE chain = r.chain;
 END LOOP;
END $$;

-- Independent verification, mirroring `audit::verify`: walk every chain in
-- `seq` order, recompute each link, compare the stored prev_hash/hash, and
-- compare the chain head with its last row. A rehash that permuted the order
-- fails the migration loudly instead of shipping a broken audit log.
DO $$
DECLARE
 r RECORD;
 prev_c uuid := NULL;
 prev_h bytea := decode(repeat('00', 32), 'hex');
 expect bytea;
BEGIN
 FOR r IN
  SELECT COALESCE(organization_id, '00000000-0000-0000-0000-000000000000') AS chain,
         id, organization_id, actor_id, actor_kind, actor_account_id, kind, seq, created_at,
         prev_hash, hash, client_ip, client_ip_key_id, client_ip_digest
  FROM audit_events
  ORDER BY chain, seq
 LOOP
  IF r.chain IS DISTINCT FROM prev_c THEN
   prev_h := decode(repeat('00', 32), 'hex');
   prev_c := r.chain;
  END IF;
  expect := sha256(prev_h || audit_canonical_v4(
    r.id, r.organization_id, r.actor_id, r.actor_kind, r.actor_account_id,
    r.kind, r.seq, r.created_at, r.client_ip, r.client_ip_key_id, r.client_ip_digest));
  IF r.prev_hash IS DISTINCT FROM prev_h OR r.hash IS DISTINCT FROM expect THEN
   RAISE EXCEPTION 'audit chain % is broken at seq %', r.chain, r.seq;
  END IF;
  prev_h := expect;
 END LOOP;

 IF EXISTS (
  SELECT 1 FROM audit_chain ch
  LEFT JOIN LATERAL (
   SELECT e.seq, e.hash FROM audit_events e
   WHERE COALESCE(e.organization_id, '00000000-0000-0000-0000-000000000000') = ch.chain
   ORDER BY e.seq DESC LIMIT 1
  ) last ON TRUE
  WHERE ch.last_seq IS DISTINCT FROM COALESCE(last.seq, 0)
     OR ch.last_hash IS DISTINCT FROM
        COALESCE(last.hash, decode(repeat('00', 32), 'hex'))
 ) THEN
  RAISE EXCEPTION 'audit chain head does not match its last row';
 END IF;
END $$;

-- Recreate the one allowed UPDATE (IP pruning) with the new columns included in
-- the unchanged-columns comparison, so actor fields cannot move under it.
CREATE OR REPLACE FUNCTION audit_allow_ip_prune() RETURNS trigger AS $$
BEGIN
    IF OLD.client_ip IS NOT NULL
       AND NEW.client_ip IS NULL
       AND OLD.client_ip_digest IS NOT NULL
       AND (OLD.id, OLD.organization_id, OLD.actor_id, OLD.actor_kind, OLD.actor_account_id,
            OLD.kind, OLD.created_at, OLD.seq, OLD.prev_hash, OLD.hash,
            OLD.client_ip_key_id, OLD.client_ip_digest)
         IS NOT DISTINCT FROM
         (NEW.id, NEW.organization_id, NEW.actor_id, NEW.actor_kind, NEW.actor_account_id,
          NEW.kind, NEW.created_at, NEW.seq, NEW.prev_hash, NEW.hash,
          NEW.client_ip_key_id, NEW.client_ip_digest)
    THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'audit log is append-only';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER audit_events_allow_ip_prune BEFORE UPDATE ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_allow_ip_prune();
