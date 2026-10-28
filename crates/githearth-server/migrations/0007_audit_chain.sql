-- Tamper-evident audit chain (Task AU, IAM design).
-- audit_events rows form a per-organization hash chain written in the same
-- transaction as the change they describe. organization_id IS NULL rows form
-- one more chain, the "site chain", keyed by the nil UUID.

ALTER TABLE audit_events
 ADD COLUMN seq bigint NULL,
 ADD COLUMN prev_hash bytea NULL,
 ADD COLUMN hash bytea NULL;

CREATE INDEX audit_events_org_seq ON audit_events (organization_id, seq) WHERE seq IS NOT NULL;

-- One row per chain: the organization id, or the nil UUID for the site chain.
CREATE TABLE audit_chain (
 chain uuid PRIMARY KEY,
 last_seq bigint NOT NULL,
 last_hash bytea NOT NULL
);

-- Canonical, unambiguous encoding of a row, shared by the insert trigger, the
-- backfill and the verifier:
--   id (16 bytes)
--   organization_id (16 bytes; nil UUID when NULL)
--   actor_id (16 bytes)
--   length(kind) (8 bytes big-endian) || kind (UTF-8)
--   seq (8 bytes big-endian)
--   created_at microseconds since epoch (8 bytes big-endian)
CREATE FUNCTION audit_canonical(
 id uuid, organization_id uuid, actor_id uuid, kind text, seq bigint, created_at timestamptz
) RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
 SELECT uuid_send(id)
     || uuid_send(COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'))
     || uuid_send(actor_id)
     || int8send(octet_length(kind)) || convert_to(kind, 'UTF8')
     || int8send(seq)
     || int8send((EXTRACT(EPOCH FROM created_at) * 1000000)::bigint)
$$;

CREATE FUNCTION audit_assign() RETURNS trigger AS $$
DECLARE
 c uuid;
BEGIN
 c := COALESCE(NEW.organization_id, '00000000-0000-0000-0000-000000000000');
 -- Make the chain row exist, then lock it; serializes writers per chain.
 INSERT INTO audit_chain (chain, last_seq, last_hash)
 VALUES (c, 0, decode(repeat('00', 32), 'hex'))
 ON CONFLICT (chain) DO NOTHING;
 SELECT last_seq + 1, last_hash INTO NEW.seq, NEW.prev_hash
   FROM audit_chain WHERE audit_chain.chain = c FOR UPDATE;
 NEW.hash := sha256(
   NEW.prev_hash || audit_canonical(NEW.id, NEW.organization_id, NEW.actor_id, NEW.kind, NEW.seq, NEW.created_at)
 );
 UPDATE audit_chain SET last_seq = NEW.seq, last_hash = NEW.hash WHERE audit_chain.chain = c;
 RETURN NEW;
END $$ LANGUAGE plpgsql;

CREATE TRIGGER audit_events_assign BEFORE INSERT ON audit_events
 FOR EACH ROW EXECUTE FUNCTION audit_assign();

-- Backfill rows that predate the migration, per chain in (created_at, id)
-- order, with the same hash function. prev_hash for a row is the hash of the
-- previous row in its chain; the first row's prev_hash is 32 zero bytes. Runs
-- before the append-only triggers, which would refuse the UPDATE.
DO $$
DECLARE
 r RECORD;
 c uuid;
 prev_c uuid := NULL;
 prev_h bytea;
 new_h bytea;
 n bigint;
BEGIN
 FOR r IN
  SELECT id, organization_id, actor_id, kind, created_at FROM audit_events
  ORDER BY (organization_id IS NOT NULL), organization_id, created_at, id
 LOOP
  c := COALESCE(r.organization_id, '00000000-0000-0000-0000-000000000000');
  IF c IS DISTINCT FROM prev_c THEN
   n := 0;
  END IF;
  prev_c := c;
  n := n + 1;
  prev_h := decode(repeat('00', 32), 'hex');
  IF n > 1 THEN
   SELECT hash INTO prev_h FROM audit_events e
   WHERE COALESCE(e.organization_id, '00000000-0000-0000-0000-000000000000') = c
     AND e.seq = n - 1;
  END IF;
  new_h := sha256(prev_h || audit_canonical(r.id, r.organization_id, r.actor_id, r.kind, n, r.created_at));
  UPDATE audit_events SET seq = n, prev_hash = prev_h, hash = new_h WHERE id = r.id;
 END LOOP;

 FOR r IN
  SELECT COALESCE(organization_id, '00000000-0000-0000-0000-000000000000') AS c,
         max(seq) AS last_seq
  FROM audit_events GROUP BY 1
 LOOP
  INSERT INTO audit_chain (chain, last_seq, last_hash)
  VALUES (r.c, r.last_seq,
   (SELECT hash FROM audit_events e
    WHERE COALESCE(e.organization_id, '00000000-0000-0000-0000-000000000000') = r.c
      AND e.seq = r.last_seq))
  ON CONFLICT (chain) DO NOTHING;
 END LOOP;
END $$;

-- The log is append-only.
CREATE FUNCTION audit_reject_write() RETURNS trigger AS $$
BEGIN
 RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'audit log is append-only';
END $$ LANGUAGE plpgsql;
CREATE TRIGGER audit_events_reject_update BEFORE UPDATE ON audit_events
 FOR EACH ROW EXECUTE FUNCTION audit_reject_write();
CREATE TRIGGER audit_events_reject_delete BEFORE DELETE ON audit_events
 FOR EACH ROW EXECUTE FUNCTION audit_reject_write();
CREATE TRIGGER audit_events_reject_truncate BEFORE TRUNCATE ON audit_events
 FOR EACH STATEMENT EXECUTE FUNCTION audit_reject_write();
