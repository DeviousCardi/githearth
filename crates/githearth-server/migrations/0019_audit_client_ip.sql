-- Record the client IP on audit events inside the tamper-evident chain.
-- client_ip is NULL for rows produced outside an HTTP request scope (jobs,
-- CLI, SSH, background work), and for every row that predates this migration.

ALTER TABLE audit_events
 ADD COLUMN client_ip inet;

-- V2 of the canonical encoding appends an optional, length-prefixed client IP
-- so that a row with a NULL client_ip hashes byte-for-byte identically to the
-- V1 encoding. The trailing field is unambiguous because V1 ends in several
-- fixed-width fields: id || organization_id || actor_id || len(kind) || kind
-- || seq || created_at microseconds, all big-endian.
CREATE FUNCTION audit_canonical_v2(
 id uuid, organization_id uuid, actor_id uuid, kind text, seq bigint, created_at timestamptz, client_ip inet
) RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
 SELECT audit_canonical(id, organization_id, actor_id, kind, seq, created_at)
     || CASE WHEN client_ip IS NULL THEN ''::bytea
             ELSE int8send(octet_length(host(client_ip))) || convert_to(host(client_ip), 'UTF8')
        END
$$;

-- Same trigger as 0007, but hashing via the v2 encoding that includes client_ip.
CREATE OR REPLACE FUNCTION audit_assign() RETURNS trigger AS $$
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
   NEW.prev_hash || audit_canonical_v2(NEW.id, NEW.organization_id, NEW.actor_id, NEW.kind, NEW.seq, NEW.created_at, NEW.client_ip)
 );
 UPDATE audit_chain SET last_seq = NEW.seq, last_hash = NEW.hash WHERE audit_chain.chain = c;
 RETURN NEW;
END $$ LANGUAGE plpgsql;
