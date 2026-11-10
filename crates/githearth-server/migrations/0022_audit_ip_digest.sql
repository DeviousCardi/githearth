-- Make client IPs prunable without breaking the tamper-evident chain (#98).
-- A per-row HMAC key binds the IP into the hash, so the raw IP can be NULLed
-- later once the key is deleted; the digest stays behind and the chain still
-- verifies because the hash never contained the raw IP.

-- One row per key. Keys are deleted to "shred" the binding between a digest and
-- the IP it came from (IPv4 is brute-forceable while the key still exists), so
-- there is deliberately no foreign key from audit_events to this table.
CREATE TABLE audit_ip_keys (
    id uuid PRIMARY KEY,
    key bytea NOT NULL CHECK (octet_length(key) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE audit_events
    ADD COLUMN client_ip_key_id uuid,
    ADD COLUMN client_ip_digest bytea,
    ADD CONSTRAINT audit_events_ip_digest_check CHECK (
        (client_ip_key_id IS NULL AND client_ip_digest IS NULL)
        OR (client_ip_key_id IS NOT NULL AND client_ip_digest IS NOT NULL AND octet_length(client_ip_digest) = 32)
    );

-- V3 of the canonical encoding. A row that carries a digest appends the key id
-- and the 32-byte digest (the raw IP is NOT part of the hash); otherwise it
-- falls back to V2, so legacy rows and NULL-IP rows hash byte-for-byte
-- identically to today.
CREATE FUNCTION audit_canonical_v3(
    id uuid, organization_id uuid, actor_id uuid, kind text, seq bigint, created_at timestamptz,
    client_ip inet, key_id uuid, digest bytea
) RETURNS bytea LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN digest IS NOT NULL THEN
            audit_canonical(id, organization_id, actor_id, kind, seq, created_at)
            || uuid_send(key_id) || digest
        ELSE
            audit_canonical_v2(id, organization_id, actor_id, kind, seq, created_at, client_ip)
    END
$$;

-- Same trigger as 0019, but hashing via the v3 encoding that includes the key id
-- and digest when present.
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
        NEW.prev_hash || audit_canonical_v3(NEW.id, NEW.organization_id, NEW.actor_id, NEW.kind, NEW.seq, NEW.created_at, NEW.client_ip, NEW.client_ip_key_id, NEW.client_ip_digest)
    );
    UPDATE audit_chain SET last_seq = NEW.seq, last_hash = NEW.hash WHERE audit_chain.chain = c;
    RETURN NEW;
END $$ LANGUAGE plpgsql;

-- Allow exactly one kind of UPDATE: pruning the raw IP from a row that was
-- written with a digest. Every other column must be unchanged. DELETE and
-- TRUNCATE remain rejected by audit_reject_write.
CREATE OR REPLACE FUNCTION audit_allow_ip_prune() RETURNS trigger AS $$
BEGIN
    IF OLD.client_ip IS NOT NULL
       AND NEW.client_ip IS NULL
       AND OLD.client_ip_digest IS NOT NULL
       AND (OLD.id, OLD.organization_id, OLD.actor_id, OLD.kind, OLD.created_at, OLD.seq, OLD.prev_hash, OLD.hash, OLD.client_ip_key_id, OLD.client_ip_digest)
         IS NOT DISTINCT FROM
         (NEW.id, NEW.organization_id, NEW.actor_id, NEW.kind, NEW.created_at, NEW.seq, NEW.prev_hash, NEW.hash, NEW.client_ip_key_id, NEW.client_ip_digest)
    THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION USING ERRCODE = 'check_violation', MESSAGE = 'audit log is append-only';
END $$ LANGUAGE plpgsql;

DROP TRIGGER audit_events_reject_update ON audit_events;
CREATE TRIGGER audit_events_allow_ip_prune BEFORE UPDATE ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_allow_ip_prune();
