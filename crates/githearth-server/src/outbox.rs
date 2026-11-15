//! Outbox fan-out (M4.1, acceptance case U12; ADR 0006 "Revision for M4.1").
//!
//! The relay reads committed `outbox_events` rows in `(chain, seq)` order and
//! hands each one to every registered consumer whose registration position and
//! kind filter include it. Each hand-off is a durable job: retries, leases and
//! dead letters live in `jobs`, which is the reason ADR 0006 gave against
//! claiming outbox rows directly, and that reasoning still holds.
//!
//! # Registration positions
//!
//! A consumer has one position per audit chain (`outbox_consumer_positions`):
//! the chain's head when the consumer was **first** registered, or the cutover
//! watermark migration `0050` wrote for the consumer that shipped with that
//! upgrade. A consumer receives `(chain, seq)` only while `seq > position`. The
//! position is written once and never moved, so it survives a restart. A chain
//! that did not exist at registration has no position row; the fan-out treats
//! that as position zero, so it is delivered from its own first event even
//! though later `register` calls (the relay re-registers each poll) see it.
//!
//! # Fan-out, dedupe and at-least-once
//!
//! One job is enqueued per `(event, consumer)`. The job key is deterministic,
//! `outbox-deliver:<event id>:<consumer id>`, so the same job is re-enqueued by
//! any later pass and the runner's dedupe index (unique per kind while pending
//! or running) makes a second live copy impossible even when two consumers
//! share a job kind. The pair is never re-inserted: the unique
//! `(event_id, consumer_id)` row and the event's `delivered_at` marker both
//! stop it, so a consumer handler runs once per delivery, not repeatedly until
//! it succeeds. Idempotence is still the handler's own business, because the
//! runner can retry a job whose attempt failed. The event's `delivered_at` is
//! set once every due consumer has its delivery row: it keeps the ADR 0006
//! meaning, "the fan-out committed", and says nothing about whether each
//! consumer's handler succeeded. Every consumer has a handler: [`register`]
//! refuses one with no job kind.
//!
//! A delivery that exhausts its budget is mirrored to `dead` by the runner
//! (`jobs::sync_delivery`), `doctor` reports it, and [`requeue`] recovers it
//! under a key that no finished job can absorb.

use crate::{AppState, jobs};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::time::Duration;
use uuid::Uuid;

/// The job key prefix; with the event id it makes the fan-out key.
const JOB_KEY_PREFIX: &str = "outbox-deliver:";

pub const DEFAULT_MAX_ATTEMPTS: i32 = 30;
pub const MAX_ATTEMPTS_LIMIT: i32 = 1000;
pub const DEFAULT_BACKOFF_CAP_SECONDS: u64 = 600;

/// A registered consumer of the outbox.
#[derive(Clone, Debug)]
pub struct Consumer {
    pub id: Uuid,
    pub name: String,
    /// `None` means every event kind.
    pub kinds: Option<Vec<String>>,
    /// The job kind a delivery is enqueued as. Required: a consumer with no
    /// handler is refused by [`register`] (ADR 0006, revised for M4.1).
    pub job_kind: Option<String>,
    pub max_attempts: i32,
    pub backoff_cap: Duration,
}

impl Consumer {
    pub fn new(name: &str, job_kind: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            kinds: None,
            job_kind: Some(job_kind.to_string()),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_cap: Duration::from_secs(DEFAULT_BACKOFF_CAP_SECONDS),
        }
    }

    pub fn with_kinds(mut self, kinds: &[&str]) -> Self {
        self.kinds = Some(kinds.iter().map(|k| k.to_string()).collect());
        self
    }

    fn filter(&self) -> Value {
        match &self.kinds {
            Some(kinds) => Value::Array(kinds.iter().map(|k| Value::String(k.clone())).collect()),
            None => Value::Null,
        }
    }
}

/// The fan-out key of one `(event, consumer)` delivery. The event id makes it
/// deterministic across relay passes; the consumer id keeps two consumers with
/// the same job kind from sharing one key.
pub fn fanout_key(event_id: Uuid, consumer_id: Uuid) -> String {
    format!("{JOB_KEY_PREFIX}{event_id}:{consumer_id}")
}

/// The key of the `n`th requeue of a delivery row.
pub fn requeue_key(base: &str, n: i64) -> String {
    format!("{base}:r{n}")
}

/// Registers (or updates) a consumer. A position on a chain is written only
/// the first time the consumer is seen; later calls update the filter, job kind
/// and budget and leave every position alone.
///
/// A consumer must have a handler: `job_kind` is required, and a consumer
/// without one is refused (ADR 0006, revised for M4.1). Its deliveries would be
/// created by the fan-out and never enqueued, so they would stay `pending`
/// forever while the event waited for a consumer that can never deliver it.
pub async fn register(pool: &PgPool, consumer: &Consumer) -> Result<(), sqlx::Error> {
    if consumer.job_kind.is_none() {
        return Err(sqlx::Error::Protocol(
            "an outbox consumer needs a job kind; register it with its handler".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    // The chain rows first, under the lock every audit writer takes, so no
    // writer commits a new seq between the head read below and this commit.
    sqlx::query("SELECT chain FROM audit_chain FOR SHARE")
        .execute(&mut *tx)
        .await?;
    // `xmax = 0` is true only for a row this statement inserted. Positions are
    // drawn for a *new* consumer only: a chain created after it registered has
    // no position row, which the fan-out reads as zero so the chain is
    // delivered from its own first event. Re-registering on every relay pass
    // must not give that chain a late head and skip the backlog.
    let (row_id, inserted): (Uuid, bool) = sqlx::query_as(
        "INSERT INTO outbox_consumers \
             (id, name, event_filter, job_kind, max_attempts, backoff_cap_seconds) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (name) DO UPDATE SET \
             event_filter = EXCLUDED.event_filter, \
             job_kind = EXCLUDED.job_kind, \
             max_attempts = EXCLUDED.max_attempts, \
             backoff_cap_seconds = EXCLUDED.backoff_cap_seconds, \
             updated_at = now() \
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(consumer.id)
    .bind(&consumer.name)
    .bind(consumer.filter())
    .bind(&consumer.job_kind)
    .bind(consumer.max_attempts.clamp(1, MAX_ATTEMPTS_LIMIT))
    .bind(consumer.backoff_cap.as_secs().clamp(1, 86_400) as i32)
    .fetch_one(&mut *tx)
    .await?;
    if inserted {
        let chains: Vec<(Uuid, i64)> = sqlx::query_as("SELECT chain, last_seq FROM audit_chain")
            .fetch_all(&mut *tx)
            .await?;
        for (chain, last_seq) in chains {
            // The first position a consumer has on a chain is its registration
            // position; the primary key makes every later call a no-op.
            sqlx::query(
                "INSERT INTO outbox_consumer_positions (consumer_id, chain, from_seq) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(row_id)
            .bind(chain)
            .bind(last_seq)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// The consumer this release ships, registered at `serve` start-up (and by
/// migration `0050`, which gives it a position at the cutover).
pub async fn register_builtin(state: &AppState) -> Result<(), sqlx::Error> {
    register(&state.pool, &crate::notifications::outbox_consumer()).await
}

/// One event and the consumers due it.
#[derive(sqlx::FromRow)]
struct Pair {
    event_id: Uuid,
    chain: Uuid,
    seq: i64,
    kind: String,
    organization_id: Option<Uuid>,
    event_payload: Value,
    consumer_id: Uuid,
    job_kind: Option<String>,
    max_attempts: i32,
    backoff_cap_seconds: i32,
}

const PAIRS: &str = "SELECT e.id AS event_id, e.chain, e.seq, e.kind, e.organization_id, \
            e.payload AS event_payload, c.id AS consumer_id, c.job_kind, c.max_attempts, \
            c.backoff_cap_seconds \
     FROM outbox_events e \
     CROSS JOIN outbox_consumers c \
     LEFT JOIN outbox_consumer_positions p \
            ON p.consumer_id = c.id AND p.chain = e.chain \
     WHERE e.delivered_at IS NULL \
       AND e.seq > COALESCE(p.from_seq, 0) \
       AND (c.event_filter IS NULL OR c.event_filter = 'null'::jsonb OR c.event_filter ? e.kind) \
     ORDER BY e.chain, e.seq, c.name \
      LIMIT $1";

/// Fans out one batch of undelivered events. Returns how many `(event,
/// consumer)` deliveries it created.
pub async fn fan_out_batch(state: &AppState, limit: i64) -> Result<u64, sqlx::Error> {
    let pairs: Vec<Pair> = sqlx::query_as(PAIRS)
        .bind(limit)
        .fetch_all(&state.pool)
        .await?;
    let mut created = 0u64;
    for pair in &pairs {
        let job_key = fanout_key(pair.event_id, pair.consumer_id);
        let mut tx = state.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO outbox_deliveries \
                 (id, event_id, consumer_id, chain, seq, kind, max_attempts, job_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (event_id, consumer_id) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(pair.event_id)
        .bind(pair.consumer_id)
        .bind(pair.chain)
        .bind(pair.seq)
        .bind(&pair.kind)
        .bind(pair.max_attempts.clamp(1, MAX_ATTEMPTS_LIMIT))
        .bind(&job_key)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            // Another pass (or a crash-and-refan) already created it.
            tx.rollback().await?;
            continue;
        }
        if let Some(job_kind) = &pair.job_kind {
            jobs::enqueue_with(
                &mut tx,
                pair.organization_id,
                job_kind,
                serde_json::json!({
                    "event_id": pair.event_id,
                    "consumer_id": pair.consumer_id,
                    "event_kind": pair.kind,
                    "event": pair.event_payload,
                }),
                &job_key,
                None,
                jobs::EnqueueOptions {
                    max_attempts: Some(pair.max_attempts),
                    backoff_cap: Duration::from_secs(
                        pair.backoff_cap_seconds.clamp(1, 86_400) as u64
                    ),
                },
            )
            .await?;
            sqlx::query(
                "UPDATE outbox_deliveries SET job_id = \
                     (SELECT id FROM jobs WHERE kind = $2 AND dedupe_key = $3), updated_at = now() \
                 WHERE event_id = $1 AND consumer_id = $4",
            )
            .bind(pair.event_id)
            .bind(job_kind)
            .bind(&job_key)
            .bind(pair.consumer_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        created += 1;
    }

    // "Delivered" means the fan-out committed for every consumer due the
    // event. Every consumer has a handler (`register` refuses one without a
    // job kind), so each owed delivery is created here.
    sqlx::query(
        "UPDATE outbox_events e SET delivered_at = now() \
         WHERE e.delivered_at IS NULL AND NOT EXISTS ( \
             SELECT 1 FROM outbox_consumers c \
             LEFT JOIN outbox_consumer_positions p ON p.consumer_id = c.id AND p.chain = e.chain \
             LEFT JOIN outbox_deliveries d ON d.event_id = e.id AND d.consumer_id = c.id \
             WHERE e.seq > COALESCE(p.from_seq, 0) \
               AND (c.event_filter IS NULL OR c.event_filter = 'null'::jsonb OR c.event_filter ? e.kind) \
               AND d.id IS NULL)",
    )
    .execute(&state.pool)
    .await?;
    Ok(created)
}

/// Runs the relay until `shutdown` is cancelled. Each pass re-registers the
/// built-in consumers (a chain created since the last pass gets its head before
/// the scan reads it) and fans out everything it finds, polling again
/// immediately while it keeps finding work. Polling, not LISTEN/NOTIFY (ADR
/// 0006): no catch-up logic after a reconnect, one indexed query per interval.
pub async fn run(state: &AppState, shutdown: tokio_util::sync::CancellationToken) {
    let poll = Duration::from_millis(state.config.outbox_poll_interval_ms);
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        if let Err(e) = register_builtin(state).await {
            tracing::warn!(error = %e, "outbox consumer registration failed; will retry");
        }
        let mut busy = false;
        loop {
            match fan_out_batch(state, 200).await {
                Ok(0) => break,
                Ok(_) => busy = true,
                Err(e) => {
                    tracing::warn!(error = %e, "outbox fan-out failed; will retry next poll");
                    break;
                }
            }
            if shutdown.is_cancelled() {
                return;
            }
        }
        if !busy {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }
}

/// Spawns [`run`].
pub fn spawn(
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run(&state, shutdown).await })
}

/// The delay before retry `retry` (1-based): `base × 2^(retry−1)`, capped.
fn retry_delay_seconds(retry: u32, cap_seconds: u64) -> u64 {
    (1u64 << (retry - 1).min(40)).min(cap_seconds.max(1))
}

/// The span a budget reaches: the sum of every gap between attempts, from the
/// first failure to the last retry. The default, 30 attempts with a 1 s base
/// doubling to a 10-minute cap, is
///
/// ```text
/// retries  1..8:  1+2+4+8+16+32+64+128 = 255 s
/// retry   9:      256 s
/// retry  10:      512 s
/// retries 11..29: 19 × 600 = 11,400 s
///                   total = 12,423 s = 3 h 27 min
/// ```
///
/// so an ordinary 30-minute mail outage is absorbed with attempts to spare.
pub fn retry_span_seconds(max_attempts: i32, cap_seconds: u64) -> u64 {
    (1..max_attempts.max(1) as u32)
        .map(|retry| retry_delay_seconds(retry, cap_seconds))
        .sum()
}

/// Pending / delivered / dead counts, for the operator command and `doctor`.
pub async fn counts(pool: &PgPool) -> Result<(i64, i64, i64), sqlx::Error> {
    let (pending, delivered, dead): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'pending'), \
                count(*) FILTER (WHERE state = 'delivered'), \
                count(*) FILTER (WHERE state = 'dead') \
         FROM outbox_deliveries",
    )
    .fetch_one(pool)
    .await?;
    Ok((pending, delivered, dead))
}

/// What a requeue did to one delivery.
#[derive(Debug, PartialEq, Eq)]
pub enum RequeueOutcome {
    Requeued { job_id: Uuid, job_key: String },
    Skipped(String),
}

/// Re-enqueues dead deliveries from their rows (`jobs requeue`).
///
/// `consumer` and `job_key` narrow the selection. For each row, in its own
/// transaction and under a row lock (a concurrent redelivery or a second
/// requeue cannot double it):
///  * the job key becomes `<base>:r<n>`, `n` one more than the requeues already
///    recorded for the row. `base` is the delivery's fan-out key
///    (`outbox-deliver:<event>:<consumer>`), so the done or dead job that used
///    it can never absorb the new key;
///  * the job gets the delivery row's own `max_attempts` (an operator who
///    requeues has fixed the cause and grants the budget again) and the
///    consumer's cap, with a next attempt of now;
///  * the delivery goes back to `pending` with `attempts = 0`.
///
/// If the newest requeued job for the row is still pending or running, the row
/// is skipped: it is not lost, it is already being delivered.
///
/// `dry_run` reports what would happen and writes nothing.
pub async fn requeue(
    pool: &PgPool,
    consumer: Option<&str>,
    job_key: Option<&str>,
    dry_run: bool,
) -> Result<Vec<(Uuid, Uuid, RequeueOutcome)>, sqlx::Error> {
    struct Dead {
        delivery_id: Uuid,
        event_id: Uuid,
        kind: String,
        job_kind: Option<String>,
        max_attempts: i32,
        cap_seconds: i32,
        consumer_id: Uuid,
        job_key: String,
    }
    let dead: Vec<Dead> = sqlx::query(
        "SELECT d.id AS delivery_id, d.event_id, d.kind, d.max_attempts, d.job_key, \
                c.id AS consumer_id, c.job_kind, c.backoff_cap_seconds \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE d.state = 'dead' \
           AND ($1::text IS NULL OR c.name = $1) \
           AND ($2::text IS NULL OR d.job_key = $2 OR d.job_key LIKE $2 || ':r%') \
         ORDER BY d.chain, d.seq, c.name",
    )
    .bind(consumer)
    .bind(job_key)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| Dead {
        delivery_id: row.get("delivery_id"),
        event_id: row.get("event_id"),
        kind: row.get("kind"),
        job_kind: row.get("job_kind"),
        max_attempts: row.get("max_attempts"),
        cap_seconds: row.get("backoff_cap_seconds"),
        consumer_id: row.get("consumer_id"),
        job_key: row.get("job_key"),
    })
    .collect();

    let mut results = Vec::with_capacity(dead.len());
    for row in dead {
        let Some(job_kind) = row.job_kind.clone() else {
            results.push((
                row.delivery_id,
                row.event_id,
                RequeueOutcome::Skipped("consumer has no handler".into()),
            ));
            continue;
        };
        let mut tx = pool.begin().await?;
        let current: Option<String> = sqlx::query_scalar(
            "SELECT job_key FROM outbox_deliveries WHERE id = $1 AND state = 'dead' FOR UPDATE",
        )
        .bind(row.delivery_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            tx.rollback().await?;
            results.push((
                row.delivery_id,
                row.event_id,
                RequeueOutcome::Skipped("no longer dead".into()),
            ));
            continue;
        };
        debug_assert_eq!(current, row.job_key);
        // The row's own base, never the current (possibly already requeued)
        // key: requeue 2 must not be able to extend requeue 1's suffix.
        let base = fanout_key(row.event_id, row.consumer_id);
        let made: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE dedupe_key LIKE $1 || ':r%'")
                .bind(&base)
                .fetch_one(&mut *tx)
                .await?;
        let new_key = requeue_key(&base, made + 1);
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = $1 AND dedupe_key LIKE $2 || ':r%' \
             AND state IN ('pending', 'running')",
        )
        .bind(&job_kind)
        .bind(&base)
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            tx.rollback().await?;
            results.push((
                row.delivery_id,
                row.event_id,
                RequeueOutcome::Skipped("a requeued job is still active".into()),
            ));
            continue;
        }
        if dry_run {
            tx.rollback().await?;
            results.push((
                row.delivery_id,
                row.event_id,
                RequeueOutcome::Requeued {
                    job_id: Uuid::nil(),
                    job_key: new_key,
                },
            ));
            continue;
        }
        let organization_id: Option<Uuid> =
            sqlx::query_scalar("SELECT organization_id FROM outbox_events WHERE id = $1")
                .bind(row.event_id)
                .fetch_one(&mut *tx)
                .await?;
        jobs::enqueue_with(
            &mut tx,
            organization_id,
            &job_kind,
            serde_json::json!({
                "event_id": row.event_id,
                "consumer_id": row.consumer_id,
                "event_kind": row.kind,
            }),
            &new_key,
            None,
            jobs::EnqueueOptions {
                max_attempts: Some(row.max_attempts),
                backoff_cap: Duration::from_secs(row.cap_seconds.clamp(1, 86_400) as u64),
            },
        )
        .await?;
        let job_id: Uuid =
            sqlx::query_scalar("SELECT id FROM jobs WHERE kind = $1 AND dedupe_key = $2")
                .bind(&job_kind)
                .bind(&new_key)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "UPDATE outbox_deliveries SET state = 'pending', attempts = 0, job_id = $2, \
                 job_key = $3, next_attempt_at = now(), updated_at = now() WHERE id = $1",
        )
        .bind(row.delivery_id)
        .bind(job_id)
        .bind(&new_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        results.push((
            row.delivery_id,
            row.event_id,
            RequeueOutcome::Requeued {
                job_id,
                job_key: new_key,
            },
        ));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::{requeue_key, retry_span_seconds};

    #[test]
    fn the_default_budget_spans_more_than_three_hours() {
        // 255 + 256 + 512 + 19 × 600 = 12,423 s for 30 attempts.
        assert_eq!(retry_span_seconds(30, 600), 12_423);
        assert!(retry_span_seconds(30, 600) > 3 * 3600);
        // A 30-minute outage is absorbed with attempts to spare.
        assert!(retry_span_seconds(30, 600) > 1800);
    }

    #[test]
    fn one_attempt_has_no_retry_and_the_cap_bounds_each_gap() {
        assert_eq!(retry_span_seconds(1, 600), 0);
        assert_eq!(retry_span_seconds(2, 600), 1);
        assert_eq!(
            retry_span_seconds(11, 600),
            1 + 2 + 4 + 8 + 16 + 32 + 64 + 128 + 256 + 512
        );
    }

    #[test]
    fn a_requeue_key_is_the_fanout_key_plus_its_number() {
        let base = "outbox-deliver:6f9619ff-8b86-d011-b42d-00c04fc964ff";
        assert_eq!(requeue_key(base, 1), format!("{base}:r1"));
        assert_ne!(requeue_key(base, 1), requeue_key(base, 2));
    }
}
