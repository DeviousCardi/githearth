-- Atomic idempotency (Task atomic idempotency).
-- The operation records the created object's operation_id in its own claim so a
-- retry of an abandoned create can find the half-created row instead of recreating.
ALTER TABLE operations ADD COLUMN operation_id uuid;
