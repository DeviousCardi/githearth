-- Per-consumer backoff cap for the durable job runner (ADR 0006, revised for
-- M4.1).
--
-- The retry budget of an outbox delivery is per consumer: 30 attempts, base 1 s
-- doubling, capped at 10 minutes, which spans more than three hours. The cap
-- therefore travels with the job instead of being a constant in the runner: an
-- outbox delivery is enqueued with its consumer's `max_attempts` and
-- `backoff_cap_seconds`, and the runner reads the cap back from the row.
--
-- The default keeps every kind at 600 s, which is what the runner used before
-- this column existed (`backoff_delay` capped at 600 s, ADR 0006's delay rule).
-- A job that was retrying over the upgrade keeps the same cap.
--
-- This is a separate migration from `0050` only because `0048` and `0049`
-- belong to the teams and invitations slice, which merges first; the two
-- outbox migrations ship together.
ALTER TABLE jobs ADD COLUMN backoff_cap_seconds integer NOT NULL DEFAULT 600
    CHECK (backoff_cap_seconds BETWEEN 1 AND 86400);
