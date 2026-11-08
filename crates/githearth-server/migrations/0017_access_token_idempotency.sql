ALTER TABLE access_tokens ADD COLUMN idempotency_key text;
CREATE UNIQUE INDEX access_tokens_idempotency ON access_tokens (account_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
