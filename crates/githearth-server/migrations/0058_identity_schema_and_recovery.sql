-- M11a increment 8A: the identity schema, change of address and self-service
-- recovery (ADR 0015; 04-forge-roadmap M11a; case U32).
--
-- 0055 (token subjects/principals) and 0056 (the envelope scheme) are merged on
-- main. 0057 is reserved for the crypto-shredding lane, which owns the key
-- material; this application lane takes the next free number, 0058. Keeping
-- the two lanes on different numbers lets both apply in version order on a
-- fresh install without either renumbering the other.
--
-- What this migration owns, and what it deliberately consumes from M4.1/M4.2:
--
--  * the primary email, email_verifications and password_reset_tokens remain
--    M4.1's (0044, 0049, 0054). This slice extends password_reset_tokens for
--    self-service recovery rather than adding a second token scheme.
--  * every encrypted secret is a `stored_secrets` row under the envelope
--    scheme of 0056. A TOTP seed is therefore a `stored_secrets` row and
--    `totp_secrets` holds only its id; no plaintext seed column exists.
--  * the per-account and per-organization data key belongs to the
--    crypto-shredding lane. This lane owns only the reference column.

-- ---------------------------------------------------------------------------
-- Data-key references (crypto-shredding interface)
-- ---------------------------------------------------------------------------
--
-- `accounts.data_key_id` and `organizations.data_key_id` name the subject's
-- data key: the deterministic id the key lane derives and keeps in operator
-- custody. The value is not free choice. It is the first 16 bytes of
--
--     SHA-256("githearth-owner-data-key-v1\0" || kind || 0x00 || subject uuid16)
--
-- with the UUID version and variant bits set (data_keys::id_for). Both lanes
-- must produce exactly this value, so the functions below compute it in the
-- database and a CHECK refuses any other reference. The first test vector in
-- tests/identity.rs pins the algorithm against the key lane.
--
-- There is no foreign key to `secret_data_keys`. The agreed interface with the
-- key lane (m11a-b1 build, "the reference") is a plain `uuid`: the owner key is
-- a file in the operator-custody directory, not a row, and a shredded key
-- leaves no row to point at. A row FK would make provisioning fail before the
-- first secret and would invite "delete the row" as a fake shred. The
-- reference therefore survives shredding by construction, and NULL means only
-- "pre-key" (a subject predating per-subject keys), never "destroyed".
--
-- The provisioning path (8B enrolment, 8C creation, import re-provisioning)
-- creates the key file and then calls `identity::bind_account_data_key` /
-- `bind_organization_data_key`, which write this column from the derived
-- value. This lane never creates, wraps, unwraps or destroys key material. It
-- only records which key a subject's ciphertext is under.
--
-- The binding is one level down. A secret keeps its *own* random data key in
-- `secret_data_keys`; `secrets::seal_owned` wraps that per-secret key under the
-- subject's owner key and records the owner in
-- `secret_data_keys.owner_key_id/owner_kind/owner_id`. `stored_secrets
-- .data_key_id` therefore names the per-secret key row, not the owner key, and
-- the TOTP triggers below require that row's `owner_key_id` to equal the
-- account's `data_key_id`. That is the property shredding needs: deleting the
-- owner key file makes every per-secret key it wrapped, and so the seed,
-- unreadable. (Comparing `stored_secrets.data_key_id` with
-- `accounts.data_key_id` directly would refuse every legitimate `seal_owned`
-- write.)
-- Each function is self-contained rather than sharing a helper. pg_restore
-- does not track a SQL function's dependency on a function named only in its
-- body, and it restores table data with a search_path in which that unqualified
-- callee is not visible. A shared helper therefore made `restore` fail while
-- validating the CHECK on `accounts`; the two copies below differ only in the
-- kind literal and are pinned by the same test vectors as `data_keys::id_for`.
CREATE FUNCTION identity_account_data_key(owner uuid) RETURNS uuid
LANGUAGE sql IMMUTABLE STRICT AS $$
  WITH raw AS (
    SELECT substring(sha256(
      decode('6769746865617274682d6f776e65722d646174612d6b65792d763100', 'hex')
      || convert_to('account', 'UTF8') || decode('00', 'hex') || uuid_send(owner)
    ) FROM 1 FOR 16) AS bytes
  )
  SELECT encode(
    set_byte(
      set_byte(bytes, 6, (get_byte(bytes, 6) & 15) | 80),
      8, (get_byte(bytes, 8) & 63) | 128
    ),
    'hex'
  )::uuid
  FROM raw
$$;

CREATE FUNCTION identity_organization_data_key(owner uuid) RETURNS uuid
LANGUAGE sql IMMUTABLE STRICT AS $$
  WITH raw AS (
    SELECT substring(sha256(
      decode('6769746865617274682d6f776e65722d646174612d6b65792d763100', 'hex')
      || convert_to('organization', 'UTF8') || decode('00', 'hex') || uuid_send(owner)
    ) FROM 1 FOR 16) AS bytes
  )
  SELECT encode(
    set_byte(
      set_byte(bytes, 6, (get_byte(bytes, 6) & 15) | 80),
      8, (get_byte(bytes, 8) & 63) | 128
    ),
    'hex'
  )::uuid
  FROM raw
$$;

ALTER TABLE accounts ADD COLUMN data_key_id uuid;
ALTER TABLE organizations ADD COLUMN data_key_id uuid;

-- The reference is the derived value, or NULL for a pre-key row.
ALTER TABLE accounts
 ADD CONSTRAINT accounts_data_key_is_derived
 CHECK (data_key_id IS NULL OR data_key_id = identity_account_data_key(id));
ALTER TABLE organizations
 ADD CONSTRAINT organizations_data_key_is_derived
 CHECK (
   data_key_id IS NULL OR data_key_id = identity_organization_data_key(id)
 );

-- One key belongs to one subject: two accounts sharing a data key would make
-- shredding one destroy the other's ciphertext. The CHECK above already makes
-- a shared reference unrepresentable, and the index keeps that guarantee
-- explicit at the storage layer. NULL (pre-key) is outside the index, so any
-- number of rows may have no key.
CREATE UNIQUE INDEX accounts_data_key ON accounts (data_key_id)
 WHERE data_key_id IS NOT NULL;
CREATE UNIQUE INDEX organizations_data_key ON organizations (data_key_id)
 WHERE data_key_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- External identities and the per-organization federation policy
-- ---------------------------------------------------------------------------

-- One OIDC/RP subject per installation, mapped to one account. `issuer` is the
-- provider's `iss` value; `(issuer, subject)` is unique, so the same provider
-- subject cannot map to two accounts. A provider email is stored only as the
-- evidence used at link time; `email_verified` records the provider's claim and
-- the linking rule (identity.rs) refuses to create a row unless the claim is
-- verified and equals the account's verified address.
CREATE TABLE external_identities (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  issuer text NOT NULL CHECK (length(issuer) BETWEEN 1 AND 2048),
  subject text NOT NULL CHECK (length(subject) BETWEEN 1 AND 255),
  email text CHECK (
    email IS NULL OR (
      length(email) BETWEEN 3 AND 320
      AND email = lower(email)
      AND email ~ '^[ -~]+$'
      AND length(email) - length(replace(email, '@', '')) = 1
      AND strpos(email, '@') > 1
      AND length(split_part(email, '@', 1)) BETWEEN 1 AND 64
      AND left(split_part(email, '@', 1), 1) <> '.'
      AND split_part(email, '@', 2) <> ''
      AND strpos(split_part(email, '@', 2), '.') > 1
      AND right(split_part(email, '@', 2), 1) <> '.'
      AND split_part(email, '@', 2) NOT LIKE '%..%'
    )
  ),
  email_verified boolean NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_login_at timestamptz,
  UNIQUE (issuer, subject)
);
CREATE INDEX external_identities_account ON external_identities (account_id);
CREATE INDEX external_identities_email ON external_identities (lower(email))
 WHERE email IS NOT NULL;

-- The per-organization policy for the installation's one issuer set. A missing
-- row is `optional` (offered, not required). `domain` requires an allowed
-- domain; the matching rule is an M11b/SAML decision (ADR 0015 open item 3),
-- so this migration stores the inputs and does not invent the rule.
CREATE TABLE organization_identity_policies (
  organization_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
  policy text NOT NULL DEFAULT 'optional'
    CHECK (policy IN ('optional', 'required', 'domain')),
  allowed_domain text CHECK (
    allowed_domain IS NULL OR (
      length(allowed_domain) BETWEEN 1 AND 253
      AND allowed_domain = lower(allowed_domain)
      AND allowed_domain ~ '^[a-z0-9.-]+$'
      AND strpos(allowed_domain, '..') = 0
      AND left(allowed_domain, 1) <> '.'
      AND right(allowed_domain, 1) <> '.'
    )
  ),
  updated_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT organization_identity_policies_domain_check CHECK (
    (policy = 'domain') = (allowed_domain IS NOT NULL)
  )
);

-- ---------------------------------------------------------------------------
-- Second factors: TOTP seeds, passkeys, recovery codes
-- ---------------------------------------------------------------------------

-- One TOTP authenticator per account. The seed itself is never stored here: it
-- is a `stored_secrets` row (0056), encrypted under the subject's data key, and
-- `secret_id` is the only reference. `last_used_step` lets verification refuse
-- a replayed code once M11a 8B reads it.
CREATE TABLE totp_secrets (
  account_id uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  secret_id uuid NOT NULL UNIQUE REFERENCES stored_secrets(id) ON DELETE CASCADE,
  confirmed_at timestamptz,
  last_used_step bigint CHECK (last_used_step IS NULL OR last_used_step >= 0),
  created_at timestamptz NOT NULL DEFAULT now()
);

-- The seed must be wrapped under the owning account's data key. One level
-- down from `stored_secrets`, the per-secret `secret_data_keys` row carries the
-- owner that wrapped it; without this binding a seed for account A could sit
-- under account B's owner key, and shredding A's key would leave A's factor
-- readable while shredding B's would destroy it. The triggers also fire when
-- either side moves the key, so the invariant cannot be broken after the row
-- exists. Deferred so a transaction may provision the key and the seed in
-- either order; every other row in the transaction is checked at the same final
-- state.
--
-- 0057 (the key lane) owns `owner_kind`/`owner_id`/`owner_key_id` on
-- `secret_data_keys` and always applies first in version order. This migration
-- consumes those columns rather than declaring a second copy; the pairing
-- CHECK, indexes and custody lifecycle are 0057's, and the triggers below only
-- read them.
CREATE FUNCTION identity_totp_key_binding(account_id uuid, secret_id uuid)
RETURNS text
LANGUAGE plpgsql STABLE AS $$
DECLARE
  owner_key uuid;
  owner_kind text;
  owner_id uuid;
  account_key uuid;
BEGIN
  SELECT k.owner_key_id, k.owner_kind, k.owner_id
    INTO owner_key, owner_kind, owner_id
    FROM stored_secrets s
    JOIN secret_data_keys k ON k.id = s.data_key_id
   WHERE s.id = secret_id;
  SELECT data_key_id INTO account_key FROM accounts WHERE id = account_id;
  IF account_key IS NULL THEN
    RETURN format('account %s has no data key', account_id);
  END IF;
  IF owner_key IS NULL OR owner_kind IS DISTINCT FROM 'account'
     OR owner_id IS DISTINCT FROM account_id THEN
    RETURN format(
      'secret %s is not wrapped under a per-secret key owned by account %s',
      secret_id, account_id);
  END IF;
  IF owner_key <> account_key THEN
    RETURN format(
      'secret %s is wrapped under owner key %s, not account %s''s key %s',
      secret_id, owner_key, account_id, account_key);
  END IF;
  RETURN NULL;
END;
$$;

CREATE FUNCTION identity_enforce_totp_key_binding() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  problem text;
BEGIN
  problem := identity_totp_key_binding(NEW.account_id, NEW.secret_id);
  IF problem IS NOT NULL THEN
    RAISE EXCEPTION 'invalid authenticator key binding: %', problem
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER totp_secrets_key_binding
  AFTER INSERT OR UPDATE OF secret_id, account_id ON totp_secrets
  DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION identity_enforce_totp_key_binding();

CREATE FUNCTION identity_enforce_account_key_binding() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  problem text;
  seed record;
BEGIN
  FOR seed IN SELECT secret_id FROM totp_secrets WHERE account_id = NEW.id LOOP
    problem := identity_totp_key_binding(NEW.id, seed.secret_id);
    IF problem IS NOT NULL THEN
      RAISE EXCEPTION 'invalid authenticator key binding: %', problem
        USING ERRCODE = 'check_violation';
    END IF;
  END LOOP;
  RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER accounts_key_binding
  AFTER UPDATE OF data_key_id ON accounts
  DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION identity_enforce_account_key_binding();

-- Moving the per-secret key row out from under the account (or moving a
-- stored secret onto another key row) is refused by re-checking every seed
-- that names it. `secrets::rotate` may rewrite the wrapped bytes, never the
-- owner.
CREATE FUNCTION identity_enforce_secret_key_binding() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  problem text;
  seed record;
BEGIN
  FOR seed IN
    SELECT t.account_id, t.secret_id
      FROM totp_secrets t
      JOIN stored_secrets s ON s.id = t.secret_id
     WHERE s.data_key_id = NEW.id
  LOOP
    problem := identity_totp_key_binding(seed.account_id, seed.secret_id);
    IF problem IS NOT NULL THEN
      RAISE EXCEPTION 'invalid authenticator key binding: %', problem
        USING ERRCODE = 'check_violation';
    END IF;
  END LOOP;
  RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER secret_data_keys_key_binding
  AFTER UPDATE OF owner_key_id, owner_kind, owner_id ON secret_data_keys
  DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION identity_enforce_secret_key_binding();

CREATE FUNCTION identity_enforce_stored_secret_key_binding() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  problem text;
  seed record;
BEGIN
  FOR seed IN
    SELECT account_id, secret_id FROM totp_secrets WHERE secret_id = NEW.id
  LOOP
    problem := identity_totp_key_binding(seed.account_id, seed.secret_id);
    IF problem IS NOT NULL THEN
      RAISE EXCEPTION 'invalid authenticator key binding: %', problem
        USING ERRCODE = 'check_violation';
    END IF;
  END LOOP;
  RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER stored_secrets_key_binding
  AFTER UPDATE OF data_key_id ON stored_secrets
  DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION identity_enforce_stored_secret_key_binding();

-- A WebAuthn credential. The public key is public material; the credential id
-- is unique installation-wide, so one browser credential cannot be enrolled on
-- two accounts. The relying-party id is bound at registration and stored here.
CREATE TABLE webauthn_credentials (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  credential_id bytea NOT NULL UNIQUE
    CHECK (octet_length(credential_id) BETWEEN 1 AND 1023),
  public_key bytea NOT NULL
    CHECK (octet_length(public_key) BETWEEN 1 AND 4096),
  sign_count bigint NOT NULL DEFAULT 0 CHECK (sign_count >= 0),
  rp_id text NOT NULL CHECK (length(rp_id) BETWEEN 1 AND 253),
  transports text[] NOT NULL DEFAULT '{}'::text[],
  name text CHECK (name IS NULL OR length(name) BETWEEN 1 AND 100),
  created_at timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz,
  disabled_at timestamptz
);
CREATE INDEX webauthn_credentials_account ON webauthn_credentials (account_id);

-- Single-use recovery codes are stored as SHA-256 digests only, like every
-- other high-entropy token in the schema. `used_at` is the single-use marker.
--
-- The bare digest is right only for a code with at least 128 bits of
-- randomness: a short human-typed code would be offline-brute-forceable from a
-- database leak. 8B issues the codes and must either generate 128+ random bits
-- per code or lengthen/Argon2 the digest; that is a condition on 8B, recorded
-- in ADR 0015's "as built" section.
CREATE TABLE recovery_codes (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  code_hash bytea NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
  used_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX recovery_codes_account ON recovery_codes (account_id)
 WHERE used_at IS NULL;

-- ---------------------------------------------------------------------------
-- Change of address (ADR 0015: the new address is verified first)
-- ---------------------------------------------------------------------------
--
-- A request records the address the account held when it asked (`previous`)
-- and the address it wants (`new`). Neither replaces `accounts.email` until a
-- token addressed to `new_email` is redeemed. That is what keeps the old
-- address receiving mail until the new one verifies: every notification and
-- recovery path reads `accounts.email`, which is still the old address.
--
-- `token_hash` starts NULL, exactly like `email_verifications` (0054): the
-- plaintext exists only while the outbox consumer composes the message, so no
-- job payload or text column carries it.
CREATE TABLE email_changes (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  previous_email text NOT NULL,
  new_email text NOT NULL,
  token_hash bytea UNIQUE
    CHECK (token_hash IS NULL OR octet_length(token_hash) = 32),
  -- The confirmation mail and the "your address was changed" notice are two
  -- messages about two different facts. They must not share a Message-ID, or a
  -- receiving server that deduplicates on it could drop the notice as a copy
  -- of the confirmation. `id` names the confirmation; this names the notice,
  -- and both are stable across a retry of the same row.
  notice_id uuid NOT NULL DEFAULT gen_random_uuid() UNIQUE,
  requested_by uuid NOT NULL REFERENCES accounts(id),
  expires_at timestamptz NOT NULL,
  sent_at timestamptz,
  used_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT email_changes_distinct CHECK (previous_email <> new_email),
  CONSTRAINT email_changes_previous_format CHECK (
    length(previous_email) BETWEEN 3 AND 320
    AND previous_email = lower(previous_email)
    AND previous_email ~ '^[ -~]+$'
    AND length(previous_email) - length(replace(previous_email, '@', '')) = 1
    AND strpos(previous_email, '@') > 1
    AND length(split_part(previous_email, '@', 1)) BETWEEN 1 AND 64
    AND left(split_part(previous_email, '@', 1), 1) <> '.'
    AND split_part(previous_email, '@', 2) <> ''
    AND strpos(split_part(previous_email, '@', 2), '.') > 1
    AND right(split_part(previous_email, '@', 2), 1) <> '.'
    AND split_part(previous_email, '@', 2) NOT LIKE '%..%'
  ),
  CONSTRAINT email_changes_new_format CHECK (
    length(new_email) BETWEEN 3 AND 320
    AND new_email = lower(new_email)
    AND new_email ~ '^[ -~]+$'
    AND length(new_email) - length(replace(new_email, '@', '')) = 1
    AND strpos(new_email, '@') > 1
    AND length(split_part(new_email, '@', 1)) BETWEEN 1 AND 64
    AND left(split_part(new_email, '@', 1), 1) <> '.'
    AND split_part(new_email, '@', 2) <> ''
    AND strpos(split_part(new_email, '@', 2), '.') > 1
    AND right(split_part(new_email, '@', 2), 1) <> '.'
    AND split_part(new_email, '@', 2) NOT LIKE '%..%'
  )
);
CREATE INDEX email_changes_pending ON email_changes (account_id)
 WHERE used_at IS NULL;
CREATE INDEX email_changes_expiry ON email_changes (expires_at);

-- ---------------------------------------------------------------------------
-- Self-service recovery: reuse M4.1's password_reset_tokens table
-- ---------------------------------------------------------------------------
--
-- ADR 0015 says recovery reuses M4.1's token tables and adds no second scheme.
-- A self-service row differs from an administrator-issued code in three ways,
-- so the table gains the facts it needs:
--
--  * `purpose` keeps the two issuers apart. Existing rows default to
--    'administrator'.
--  * `email` binds the token to the verified address it was mailed to. A token
--    presented with another address, or after the account's address changes,
--    is refused (the wrong-account case in U32).
--  * `sent_at` marks the mail as composed. `token_hash` becomes nullable so
--    the plaintext token lives only in the sender's memory, as 0054 does for
--    email verification; an abandoned row has no hash and grants nothing.
ALTER TABLE password_reset_tokens
 ADD COLUMN purpose text NOT NULL DEFAULT 'administrator'
  CHECK (purpose IN ('administrator', 'self_service')),
 ADD COLUMN email text,
 ADD COLUMN sent_at timestamptz;
ALTER TABLE password_reset_tokens ALTER COLUMN token_hash DROP NOT NULL;
ALTER TABLE password_reset_tokens
 ADD CONSTRAINT password_reset_tokens_hash_length CHECK (
   token_hash IS NULL OR octet_length(token_hash) = 32
 ),
 ADD CONSTRAINT password_reset_tokens_purpose_email CHECK (
   (purpose = 'administrator' AND email IS NULL)
   OR (purpose = 'self_service' AND email IS NOT NULL)
 ),
 ADD CONSTRAINT password_reset_tokens_email_format CHECK (
   email IS NULL OR (
     length(email) BETWEEN 3 AND 320
     AND email = lower(email)
     AND email ~ '^[ -~]+$'
     AND length(email) - length(replace(email, '@', '')) = 1
     AND strpos(email, '@') > 1
     AND length(split_part(email, '@', 1)) BETWEEN 1 AND 64
     AND left(split_part(email, '@', 1), 1) <> '.'
     AND split_part(email, '@', 2) <> ''
     AND strpos(split_part(email, '@', 2), '.') > 1
     AND right(split_part(email, '@', 2), 1) <> '.'
     AND split_part(email, '@', 2) NOT LIKE '%..%'
   )
 );
CREATE INDEX password_reset_tokens_email ON password_reset_tokens (email)
 WHERE purpose = 'self_service';
