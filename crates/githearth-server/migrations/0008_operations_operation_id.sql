-- Atomic idempotency (Task atomic idempotency).
-- The operation records the created object's operation_id in its own claim so a
-- retry of an abandoned create can find the half-created row instead of recreating.
-- claim_id is the ownership generation: every claim (fresh or re-claimed) gets a
-- new one, so a stale handler's store/remove can never touch a re-claimed claim.
ALTER TABLE operations ADD COLUMN operation_id uuid;
ALTER TABLE operations ADD COLUMN claim_id uuid NOT NULL DEFAULT gen_random_uuid();
