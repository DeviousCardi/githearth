-- Outbox fan-out (M4.1, acceptance case U12; ADR 0006 "Revision for M4.1").
--
-- Migration number 0050: `0048` and `0049` belong to the teams and invitations
-- slice, which merges first. This migration and `0051_job_backoff_cap.sql` are
-- renumbered to sit after it; they are otherwise unchanged.
--
-- A relay reads committed `outbox_events` rows and enqueues one durable job per
-- registered consumer, deduplicated on (event id, consumer). The event row is
-- never claimed or updated by the relay: the `jobs` table keeps the retry,
-- lease and dead-letter columns, which is why ADR 0006 refused to claim outbox
-- rows directly and that reasoning still stands.
--
-- Pieces:
--  * the audit chain's order copied onto the outbox row: (chain, seq), unique;
--  * one cutover watermark per chain, written here, so nothing from before this
--    upgrade is ever delivered to anybody;
--  * `outbox_consumers`: each consumer's registration position per chain and
--    its event filter (NULL = every kind);
--  * `outbox_deliveries`: one row per (event, consumer) with the per-consumer
--    attempt budget, next attempt time and state (pending/delivered/dead).

-- 1. (chain, seq) on the outbox row, copied from the audit chain.
--
-- (chain, seq) is the total order the audit chain already assigns each audit
-- row. It is copied, not re-derived: the chain stays the one source of order.
-- The outbox insert is folded into the audit trigger rather than left to each
-- writer, so no writer can create an event without its order (a writer that
-- inserted only the outbox row would otherwise read as a pre-cutover event
-- forever). The trigger only fills columns the writer left NULL, so the two
-- writers that insert their outbox row explicitly (`pushes.rs`,
-- `merge_intents.rs`) keep working unchanged.
ALTER TABLE outbox_events
 ADD COLUMN chain uuid,
 ADD COLUMN seq bigint;

UPDATE outbox_events o
 SET chain = COALESCE(a.organization_id, '00000000-0000-0000-0000-000000000000'),
     seq = a.seq
 FROM audit_events a
 WHERE a.id = o.id;

-- The chain row is locked (FOR UPDATE) by `audit_assign`, so the transaction
-- that inserted the audit row holds the same lock until it commits. A writer
-- that inserts its audit and outbox rows together (every writer, `event_with`
-- and the two explicit ones) takes that lock first, so this fails only if an
-- audit row with no outbox row was ever written; then the migration must be
-- fixed, not silently backfilled later. (Every audited write writes both — the
-- outbox-failure rollback test relies on it.)
ALTER TABLE outbox_events
 ALTER COLUMN chain SET NOT NULL,
 ALTER COLUMN seq SET NOT NULL;

ALTER TABLE outbox_events
 ADD CONSTRAINT outbox_events_chain UNIQUE (chain, seq);

CREATE FUNCTION outbox_assign() RETURNS trigger AS $$
DECLARE
 a audit_events;
BEGIN
 SELECT * INTO a FROM audit_events WHERE id = NEW.id;
 IF FOUND THEN
  NEW.chain := COALESCE(NEW.chain, COALESCE(a.organization_id,
                                             '00000000-0000-0000-0000-000000000000'));
  NEW.seq := COALESCE(NEW.seq, a.seq);
 END IF;
 RETURN NEW;
END $$ LANGUAGE plpgsql;

CREATE TRIGGER outbox_events_assign BEFORE INSERT ON outbox_events
 FOR EACH ROW EXECUTE FUNCTION outbox_assign();

-- Undelivered scan: (chain, seq) order over rows no consumer has fanned out
-- yet. `delivered_at` keeps its ADR 0006 meaning, "the fan-out committed", not
-- "every consumer succeeded".
CREATE INDEX outbox_events_undelivered
 ON outbox_events (chain, seq) WHERE delivered_at IS NULL;

-- 2. Consumer registration.
CREATE TABLE outbox_consumers (
 id uuid PRIMARY KEY,
 -- The handler key in `outbox_deliveries.kind`, e.g. `notification.outbox`.
 name text NOT NULL UNIQUE CHECK (length(name) BETWEEN 1 AND 128),
 -- NULL (or JSON null) means every event kind; an array lists the kinds.
 event_filter jsonb NOT NULL DEFAULT 'null'::jsonb
    CHECK (event_filter IS NULL OR event_filter = 'null'::jsonb
           OR jsonb_typeof(event_filter) = 'array'),
 -- The job kind the delivery is enqueued as. The column admits NULL so a later
 -- schema can add an audit-only consumer, but this release enforces a handler:
 -- `outbox::register` refuses a consumer without one (ADR 0006, revised for
 -- M4.1), so a delivery always has a job that can run.
 job_kind text CHECK (job_kind IS NULL OR length(job_kind) BETWEEN 1 AND 128),
 -- Per-consumer retry budget. 30 attempts at a 1 s base doubling to a 10 min
 -- cap spans more than three hours (see `outbox::retry_span_seconds`).
 max_attempts integer NOT NULL DEFAULT 30 CHECK (max_attempts BETWEEN 1 AND 1000),
 backoff_cap_seconds integer NOT NULL DEFAULT 600
    CHECK (backoff_cap_seconds BETWEEN 1 AND 86400),
 registered_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);

-- Each consumer's registration position, one row per audit chain. A consumer
-- receives events from this (chain, seq) forward and nothing older. A consumer
-- registered after the cutover gets a row here at registration time. The
-- watermark below is written only for the consumers that exist when this
-- migration runs (the insert names `notification.outbox`); a consumer that was
-- registered before the cutover without being watermarked would receive the
-- whole backlog from zero. No other consumer can exist with this schema: a
-- consumer needs a handler, and there is no earlier outbox writer.
CREATE TABLE outbox_consumer_positions (
 consumer_id uuid NOT NULL REFERENCES outbox_consumers(id) ON DELETE CASCADE,
 chain uuid NOT NULL,
 from_seq bigint NOT NULL CHECK (from_seq >= 0),
 PRIMARY KEY (consumer_id, chain)
);

-- One delivery row per (event, consumer). Nothing cascades from an event: the
-- audit log is append-only and never deletes a row, and a delivery is the
-- record of what a consumer was owed. (A cascade here would also be the one
-- path that could delete outbox rows, which ADR 0006 keeps out of reach.)
CREATE TABLE outbox_deliveries (
 id uuid PRIMARY KEY,
 event_id uuid NOT NULL REFERENCES outbox_events(id),
 consumer_id uuid NOT NULL REFERENCES outbox_consumers(id) ON DELETE CASCADE,
 -- Copied from the event when the delivery is created: (chain, seq) order for
 -- the fan-out scan and for an operator reading the backlog.
 chain uuid NOT NULL,
 seq bigint NOT NULL,
 kind text NOT NULL,
 attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
 -- The budget this delivery was enqueued with. Carried on the row so the job
 -- and the row cannot disagree, and so a requeue reuses what the consumer had.
 max_attempts integer NOT NULL CHECK (max_attempts BETWEEN 1 AND 1000),
 next_attempt_at timestamptz NOT NULL DEFAULT now(),
 -- `pending` = waiting for its next attempt, `delivering` = the job is running,
 -- `delivered` = the handler finished, `dead` = the budget is exhausted.
 state text NOT NULL DEFAULT 'pending'
    CHECK (state IN ('pending', 'delivering', 'delivered', 'dead')),
 -- The job currently carrying the delivery. A requeue writes a new job id and
 -- a new job key, so the previous job's key can never absorb the requeue.
 job_id uuid,
 job_key text NOT NULL,
 last_error text,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 -- The dedup key of the fan-out: exactly one delivery per (event, consumer),
 -- however often the relay re-reads the event.
 UNIQUE (event_id, consumer_id)
);

CREATE INDEX outbox_deliveries_due
 ON outbox_deliveries (next_attempt_at) WHERE state = 'pending';
CREATE INDEX outbox_deliveries_order ON outbox_deliveries (chain, seq);

-- The consumers that ship with this release. The notification consumer takes
-- over the notification jobs that used to be enqueued directly beside the
-- outbox writes; `codeowners.request_reviews` stays a direct enqueue and is not
-- a consumer.
INSERT INTO outbox_consumers (id, name, event_filter, job_kind, max_attempts,
                              backoff_cap_seconds)
VALUES
 ('8f0c1a52-9d4e-4c43-9a51-6d6f75747062', 'notification.outbox',
  '["pull_request.merged", "pull_request.reviewed", "pull_request.approved",
    "pull_request.changes_requested", "pull_request.review_requested"]'::jsonb,
  'notification.outbox', 30, 600);

-- 4. The cutover watermark.
--
-- The installed consumers receive nothing from before this upgrade. Their
-- registration position is the head of each chain at migration time
-- (`audit_chain.last_seq`), and a consumer only receives (chain, seq) with
-- seq greater than its position for that chain. `audit_chain.last_seq` is
-- advanced only by `audit_assign` under the chain row's lock, and only for a
-- row whose transaction commits, so a row committed after the migration gets a
-- seq above the watermark and a row from before keeps a seq at or below it.
-- The site chain is the nil UUID, like `audit_assign`. A chain created after
-- the migration has no row in `audit_chain` yet, so it has no watermark either;
-- its first row is therefore its first event, which is what a chain created
-- after the cutover should deliver.
--
-- The read below locks `audit_chain` FOR SHARE. `audit_assign` locks the chain
-- row FOR UPDATE (conflicting with this share lock) before it assigns a seq,
-- and this transaction holds the lock until it commits. No audit or outbox row
-- can take a new seq between the read of each head and the cutover, so the
-- watermark is exactly the last row of the old backlog and everything the
-- relay later sees above it was committed after the upgrade.
INSERT INTO outbox_consumer_positions (consumer_id, chain, from_seq)
SELECT c.id, a.chain, a.last_seq
  FROM outbox_consumers c CROSS JOIN (SELECT chain, last_seq FROM audit_chain
                                      FOR SHARE) a
 WHERE c.name = 'notification.outbox'
ON CONFLICT DO NOTHING;
