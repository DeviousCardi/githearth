-- Envelope encryption for stored secrets (ADR 0014 §3, M4.2 7C).
--
-- 0055 is the token-subjects/principals migration (7B) on main; this branch was
-- cut before it and takes the next number, 0056, so the two apply in version
-- order on a fresh install and neither renumbers the other.
--
-- Every stored secret has its own random 32-byte data key. The data key is
-- wrapped by the operator's installation key and recorded in
-- `secret_data_keys`; the secret itself is encrypted under the data key and
-- recorded in `stored_secrets`. Neither table ever holds the installation key.
--
-- The migration is schema-only because the installation key is not a database
-- value. Existing plaintext `audit_ip_keys.key` rows are encrypted by the
-- binary at `migrate`/`serve` start-up, in bounded batches, before any request
-- is served. `key` stays nullable for exactly that hand-over: after the
-- backfill every row has `secret_id` and `key IS NULL`. The 32-byte length
-- check belonged to the plaintext column and is dropped because the column now
-- holds only migration input.
CREATE TABLE secret_data_keys (
    id uuid PRIMARY KEY,
    kek_id text NOT NULL,
    algorithm text NOT NULL,
    wrap_method text NOT NULL,
    wrapped_key bytea NOT NULL CHECK (octet_length(wrapped_key) > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX secret_data_keys_kek_id ON secret_data_keys(kek_id);

CREATE TABLE stored_secrets (
    id uuid PRIMARY KEY,
    name text NOT NULL UNIQUE,
    data_key_id uuid NOT NULL REFERENCES secret_data_keys(id),
    nonce bytea NOT NULL CHECK (octet_length(nonce) = 12),
    ciphertext bytea NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE audit_ip_keys
    ADD COLUMN secret_id uuid REFERENCES stored_secrets(id);

CREATE INDEX audit_ip_keys_secret_id ON audit_ip_keys(secret_id);

-- The plaintext column becomes migration input only. A row with `secret_id`
-- NULL and `key` present is a row the backfill has not reached; a row with both
-- NULL is corruption and start-up refuses it rather than silently treating the
-- row as a usable key.
ALTER TABLE audit_ip_keys ALTER COLUMN key DROP NOT NULL;

ALTER TABLE audit_ip_keys DROP CONSTRAINT IF EXISTS audit_ip_keys_key_check;
