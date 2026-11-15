-- Email verification and notification email (M4.1 acceptance cases U08, U23a).
--
-- 0049 added `accounts.email` and `accounts.email_verified_at`; it did not add
-- the `email_verifications` table the plan assigns to M4.1. This migration adds
-- that table plus the two pieces the email delivery path needs: one row per
-- account notification preference, and one durable email intent per
-- notification (so a retry can never send a second copy of a delivered email).
--
-- `email_verifications` has the shape of `password_reset_tokens` in 0044 and
-- adds the two facts a verification link needs:
--
--  * the address it verifies. A link is minted for one address, and redemption
--    requires the account still to hold that same address. Without it a link
--    requested before an address change would verify the new address, which is
--    exactly what U32 forbids. The address is normalised lower case, like
--    `accounts.email`.
--  * who requested it, so the send and redemption audit events have an actor.
--
-- `token_hash` starts NULL and is written when the verification mail is
-- composed, inside the outbox consumer that sends it. That is deliberate: the
-- plaintext token must exist only in the sender's memory, and no job payload or
-- text column may carry it. A row whose hash is still NULL is unusable, so an
-- abandoned request grants nothing. A retry mints a fresh token and overwrites
-- the hash; the previously minted plaintext was either never delivered or is
-- superseded, and either way it can never be redeemed again.

CREATE TABLE email_verifications (
  id uuid PRIMARY KEY,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  email text NOT NULL,
  -- SHA-256 of the token. NULL until the mail is composed; see above.
  token_hash bytea UNIQUE CHECK (token_hash IS NULL OR octet_length(token_hash) = 32),
  requested_by uuid NOT NULL REFERENCES accounts(id),
  expires_at timestamptz NOT NULL,
  sent_at timestamptz,
  used_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  -- The address is only ever compared with `accounts.email`, so it follows the
  -- same rule as that column (0049 `accounts_email_format`).
  CONSTRAINT email_verifications_email_format CHECK (
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
-- Redemption looks up one live token by its digest; a sweep retires the
-- account's earlier unused requests when a new one is made.
CREATE INDEX email_verifications_account ON email_verifications(account_id)
  WHERE used_at IS NULL;
CREATE INDEX email_verifications_expiry ON email_verifications(expires_at);

-- Per-account, per-kind email preference. A missing row means the default
-- (email enabled); the existence of a row with `email_enabled = false` is the
-- only way to mute a kind. The preference is checked before an email intent is
-- written, so a muted kind queues nothing at all.
CREATE TABLE notification_preferences (
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN (
    'pull_request.merged',
    'pull_request.reviewed',
    'pull_request.review_requested'
  )),
  email_enabled boolean NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (account_id, kind)
);

-- One durable intent per notification. The notification handler writes it in
-- the same transaction as the in-app row, then sends from it. The unique
-- `notification_id` is the email deduplication key: the same event, or a second
-- event naming the same source row, can insert at most one intent. `sent_at`
-- is the delivered marker; a retry that finds it set sends nothing.
--
-- `claim_expires_at` serialises two deliveries of the same notification that
-- run at once: only the winner may talk to the SMTP server. The claim expires
-- before the job lease does, so a crashed attempt's retry can claim it again.
CREATE TABLE email_intents (
  id uuid PRIMARY KEY,
  notification_id uuid NOT NULL UNIQUE REFERENCES notifications(id) ON DELETE CASCADE,
  account_id uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  kind text NOT NULL,
  claim_expires_at timestamptz,
  sent_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX email_intents_unsent ON email_intents(account_id)
  WHERE sent_at IS NULL;
