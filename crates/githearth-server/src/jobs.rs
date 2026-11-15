//! Durable job runner (ADR 0006, B side).
//!
//! Claims jobs from the `jobs` table, runs handlers with leasing and fencing,
//! and retries or marks dead on failure.

use crate::AppState;
use rand::Rng;
use sqlx::{PgConnection, Row};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type Handler = Arc<dyn Fn(AppState, Job) -> BoxFuture<'static, JobOutcome> + Send + Sync>;

pub struct Job {
    pub id: Uuid,
    pub organization_id: Option<Uuid>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub attempt: i32,
    pub max_attempts: i32,
}

pub enum JobOutcome {
    Done,
    Retry { after: Duration, reason: String },
    Failed { reason: String },
}

struct Registered {
    handler: Handler,
    /// Outbox delivery kinds are not counted by the late-pending doctor check:
    /// a delivery waiting out a retry backoff, a consumer with no handler yet,
    /// or a backlog above the registration watermark is due by design, not a
    /// stuck runner (ADR 0006, revised for M4.1).
    outbox_delivery: bool,
    /// How long this kind may run. A kind whose work is several bounded steps
    /// (housekeeping) is longer than the generic `job_timeout_seconds`; without
    /// its own budget the runner would time it out mid-step and retry forever.
    budget: Option<Duration>,
}

pub struct Registry {
    handlers: HashMap<&'static str, Registered>,
    /// Test hook: cap every retry delay, so a test can reach the attempt cap
    /// without waiting for real backoffs. Never set by `main`.
    pub backoff_override: Option<Duration>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            backoff_override: None,
        }
    }

    /// Registers a handler under the generic `job_timeout_seconds`. The kind is
    /// a `&'static str` (every caller passes a literal) because `/metrics`
    /// publishes one series per registered kind: the set must be fixed at build
    /// time, never by a value read from the database.
    pub fn register(&mut self, kind: &'static str, handler: Handler) {
        self.handlers.insert(
            kind,
            Registered {
                handler,
                outbox_delivery: false,
                budget: None,
            },
        );
    }

    /// Registers a handler with its own run budget. The claim lease is twice
    /// the budget, so a lease can never expire while its handler is running.
    pub fn register_with_budget(&mut self, kind: &'static str, handler: Handler, budget: Duration) {
        self.handlers.insert(
            kind,
            Registered {
                handler,
                outbox_delivery: false,
                budget: Some(budget),
            },
        );
    }

    /// The registered kinds, sorted, for `githearth_jobs_by_kind`. The label set
    /// is what this binary registers plus `other`, never what the database
    /// happens to hold.
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = self.handlers.keys().copied().collect();
        kinds.sort_unstable();
        kinds
    }

    /// Marks a kind as an outbox delivery: its jobs are queued by the relay
    /// behind each consumer's registration position and retry budget.
    pub fn mark_outbox_delivery(&mut self, kind: &str) {
        if let Some(registered) = self.handlers.get_mut(kind) {
            registered.outbox_delivery = true;
        }
    }

    fn outbox_delivery_kinds(&self) -> Vec<String> {
        self.handlers
            .iter()
            .filter(|(_, registered)| registered.outbox_delivery)
            .map(|(kind, _)| (*kind).to_string())
            .collect()
    }

    fn get(&self, kind: &str) -> Option<&Handler> {
        self.handlers
            .get(kind)
            .map(|registered| &registered.handler)
    }

    fn budget(&self, kind: &str) -> Option<Duration> {
        self.handlers
            .get(kind)
            .and_then(|registered| registered.budget)
    }

    /// Per-kind claim lease in seconds. A lease is twice the budget, so
    /// `recover_leases` never reclaims a job whose handler is still inside a
    /// bounded step.
    fn leases(&self) -> HashMap<&'static str, f64> {
        self.handlers
            .iter()
            .filter_map(|(kind, registered)| {
                registered
                    .budget
                    .map(|budget| (*kind, (budget * 2).as_secs_f64()))
            })
            .collect()
    }

    fn longest_budget(&self) -> Option<Duration> {
        self.handlers
            .values()
            .filter_map(|registered| registered.budget)
            .max()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

struct RunnerClaim {
    id: Uuid,
    organization_id: Option<Uuid>,
    kind: String,
    payload: serde_json::Value,
    attempt: i32,
    max_attempts: i32,
    backoff_cap_seconds: i32,
    backoff_override: Duration,
    lease_holder: Uuid,
}

/// The base of the exponential backoff (ADR 0006: base 1 s, doubling).
pub const BACKOFF_BASE_SECONDS: u32 = 1;

/// How a job is enqueued when its kind needs more than the generic defaults.
#[derive(Clone, Copy)]
pub struct EnqueueOptions {
    /// The job's attempt budget. `None` keeps the table default (5).
    pub max_attempts: Option<i32>,
    /// Hard ceiling on the exponential backoff.
    pub backoff_cap: Duration,
}

impl Default for EnqueueOptions {
    fn default() -> Self {
        Self {
            max_attempts: None,
            backoff_cap: Duration::from_secs(DEFAULT_BACKOFF_CAP_SECONDS),
        }
    }
}

pub const DEFAULT_BACKOFF_CAP_SECONDS: u64 = 600;

/// Makes every queued job of `kinds` due now: the operator's "the dependency
/// is back" without waiting for each consumer's backoff. Tests walk an outage
/// with it instead of sleeping for a 10-minute cap.
pub async fn mark_due_outbox_jobs(pool: &sqlx::PgPool, kinds: &[String]) {
    let _ = sqlx::query(
        "UPDATE jobs SET not_before = now() \
         WHERE state = 'pending' AND kind = ANY($1) AND not_before > now()",
    )
    .bind(kinds)
    .execute(pool)
    .await;
}

impl Registry {
    /// Makes every queued job of this registry's outbox delivery kinds due
    /// now. Tests that walk an outage hour by hour call it instead of waiting
    /// for a 10-minute backoff.
    pub async fn make_outbox_due(&self, pool: &sqlx::PgPool) {
        mark_due_outbox_jobs(pool, &self.outbox_delivery_kinds()).await;
    }
}

pub async fn enqueue(
    conn: &mut PgConnection,
    organization_id: Option<Uuid>,
    kind: &str,
    payload: serde_json::Value,
    dedupe_key: &str,
    delay: Option<Duration>,
) -> Result<(), sqlx::Error> {
    enqueue_with(
        conn,
        organization_id,
        kind,
        payload,
        dedupe_key,
        delay,
        EnqueueOptions::default(),
    )
    .await
}

/// `enqueue` with an explicit attempt budget and backoff cap. The outbox relay
/// enqueues its per-consumer jobs this way: a budget that spans hours must be
/// what the job retries with, and the cap is the consumer's, not a global one.
///
/// The budget is clamped to `>= 1` rather than refused: a job that can never be
/// attempted has no useful outcome, and the delivery row's own budget is the
/// one the operator can requeue with.
pub async fn enqueue_with(
    conn: &mut PgConnection,
    organization_id: Option<Uuid>,
    kind: &str,
    payload: serde_json::Value,
    dedupe_key: &str,
    delay: Option<Duration>,
    options: EnqueueOptions,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, kind, payload, dedupe_key, not_before, \
                           max_attempts, backoff_cap_seconds) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6), \
                 COALESCE($7, 5), $8) \
         ON CONFLICT (kind, dedupe_key) WHERE state IN ('pending','running') DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id)
    .bind(kind)
    .bind(payload)
    .bind(dedupe_key)
    .bind(delay.unwrap_or_default().as_secs_f64())
    .bind(options.max_attempts.map(|n| n.max(1)))
    .bind(options.backoff_cap.as_secs().clamp(1, 86_400) as i32)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `base × 2^(attempt−1)`, capped, with ±25% jitter (ADR 0006).
fn backoff_delay(attempt: i32, after: Option<Duration>, cap: Duration) -> Duration {
    let base = Duration::from_secs(BACKOFF_BASE_SECONDS as u64);
    let shift = (attempt.saturating_sub(1).min(20)) as u32;
    let scale: u64 = 1u64 << shift;
    let delay = base * (scale as u32);
    let delay = match after {
        Some(a) => delay.max(a),
        None => delay,
    }
    .min(cap);
    let jitter_pct: f64 = rand::thread_rng().gen_range(-0.25..0.25);
    let jitter_ms = (delay.as_millis() as f64 * jitter_pct) as i64;
    let jittered = if jitter_ms < 0 {
        delay.saturating_sub(Duration::from_millis(jitter_ms.unsigned_abs()))
    } else {
        delay + Duration::from_millis(jitter_ms as u64)
    };
    // Jitter must never push a retry past the cap the kind asked for.
    jittered.min(cap)
}

async fn recover_leases(state: &AppState) {
    let _ = sqlx::query(
        "UPDATE jobs SET state = 'pending', updated_at = now() \
         WHERE state = 'running' AND lease_expires_at < now() AND attempt < max_attempts",
    )
    .execute(&state.pool)
    .await;
    let _ = sqlx::query(
        "UPDATE jobs SET state = 'dead', last_error = 'lease expired', updated_at = now() \
         WHERE state = 'running' AND lease_expires_at < now() AND attempt >= max_attempts",
    )
    .execute(&state.pool)
    .await;
}

/// Claims one due job and leases it for the job kind's own budget (twice the
/// budget), falling back to the generic `default_lease_secs`. Selecting the
/// row and updating it happen in one transaction, so the kind is known before
/// the lease is chosen without opening a window another worker could claim in.
/// `backoff_override`, when set, caps every retry delay of the claimed job's
/// kind. Only tests use it: the production cap is the job's own
/// `backoff_cap_seconds`, and a test that must observe a 30-minute outage
/// through `next_attempt_at` cannot wait for real backoffs of minutes.
async fn claim_one(
    state: &AppState,
    holder: Uuid,
    default_lease_secs: f64,
    leases: &HashMap<&'static str, f64>,
    backoff_override: Option<Duration>,
) -> Option<RunnerClaim> {
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(error = %e, "job claim transaction failed");
            return None;
        }
    };
    let selected = sqlx::query(
        "SELECT id, kind FROM jobs WHERE state = 'pending' AND not_before <= now() \
         ORDER BY not_before FOR UPDATE SKIP LOCKED LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await;
    let (id, kind): (Uuid, String) = match selected {
        Ok(Some(row)) => (row.get("id"), row.get("kind")),
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "job claim query failed");
            return None;
        }
    };
    let lease_secs = leases
        .get(kind.as_str())
        .copied()
        .unwrap_or(default_lease_secs);
    let row = sqlx::query(
        "UPDATE jobs SET state = 'running', attempt = attempt + 1, lease_holder = $1, \
         lease_expires_at = now() + $2 * interval '1 second', updated_at = now() \
         WHERE id = $3 \
         RETURNING id, organization_id, kind, payload, attempt, max_attempts, \
                   backoff_cap_seconds, lease_holder",
    )
    .bind(holder)
    .bind(lease_secs)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await;

    let claim = match row {
        Ok(Some(row)) => {
            let payload: serde_json::Value = row.get("payload");
            RunnerClaim {
                id: row.get("id"),
                organization_id: row.get("organization_id"),
                kind: row.get("kind"),
                payload,
                attempt: row.get("attempt"),
                max_attempts: row.get("max_attempts"),
                backoff_cap_seconds: row.get("backoff_cap_seconds"),
                backoff_override: backoff_override.unwrap_or(Duration::from_secs(86_400)),
                lease_holder: row.get("lease_holder"),
            }
        }
        Ok(None) => {
            let _ = tx.rollback().await;
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "job claim update failed");
            let _ = tx.rollback().await;
            return None;
        }
    };
    if let Err(e) = tx.commit().await {
        tracing::warn!(error = %e, "job claim commit failed");
        return None;
    }
    Some(claim)
}

async fn apply_outcome(state: &AppState, claim: &RunnerClaim, outcome: &JobOutcome) {
    let cap = Duration::from_secs(claim.backoff_cap_seconds.clamp(1, 86_400) as u64);
    let cap = cap.min(claim.backoff_override);
    let (new_state, not_before, last_error): (&str, Option<Duration>, Option<String>) =
        match outcome {
            JobOutcome::Done => ("done", None, None),
            JobOutcome::Retry { after, reason } if claim.attempt < claim.max_attempts => {
                let d = backoff_delay(claim.attempt, Some(*after), cap);
                ("pending", Some(d), Some(reason.clone()))
            }
            JobOutcome::Retry { reason, .. } => {
                let d = backoff_delay(claim.attempt, None, cap);
                (
                    "dead",
                    Some(d),
                    Some(format!("retry limit exceeded: {reason}")),
                )
            }
            JobOutcome::Failed { reason } => ("dead", None, Some(reason.clone())),
        };

    let fenced = if let Some(not_before) = not_before {
        // `make_interval` has no `millis` parameter (only `secs`, a double);
        // passing milliseconds to it failed every retry before this fix.
        let delay_secs = not_before.as_secs_f64();
        sqlx::query(
            "UPDATE jobs SET state = $1, not_before = now() + make_interval(secs => $2), \
             last_error = $3, updated_at = now() \
             WHERE id = $4 AND state = 'running' AND lease_holder = $5 AND attempt = $6",
        )
        .bind(new_state)
        .bind(delay_secs)
        .bind(&last_error)
        .bind(claim.id)
        .bind(claim.lease_holder)
        .bind(claim.attempt)
        .execute(&state.pool)
        .await
    } else {
        sqlx::query(
            "UPDATE jobs SET state = $1, last_error = $2, updated_at = now() \
             WHERE id = $3 AND state = 'running' AND lease_holder = $4 AND attempt = $5",
        )
        .bind(new_state)
        .bind(&last_error)
        .bind(claim.id)
        .bind(claim.lease_holder)
        .bind(claim.attempt)
        .execute(&state.pool)
        .await
    };

    if let Err(e) = fenced {
        tracing::warn!(job_id = %claim.id, error = %e, "fenced outcome update failed");
    }
}

async fn run_job(
    state: AppState,
    registry: Arc<Registry>,
    claim: RunnerClaim,
    default_timeout: Duration,
) {
    // The delivery row, when this job carries one, is `delivering` while the
    // handler runs. A job with no delivery row (every non-outbox kind) makes
    // this a no-op.
    let _ = sqlx::query(
        "UPDATE outbox_deliveries SET state = 'delivering', updated_at = now() \
         WHERE job_id = $1 AND state IN ('pending', 'delivering')",
    )
    .bind(claim.id)
    .execute(&state.pool)
    .await;

    // A kind with its own budget (housekeeping: every bounded phase of a run)
    // must not be cut off by the generic `job_timeout_seconds`.
    let timeout = registry.budget(&claim.kind).unwrap_or(default_timeout);
    let handler = match registry.get(&claim.kind) {
        Some(h) => h.clone(),
        None => {
            apply_outcome(
                &state,
                &claim,
                &JobOutcome::Failed {
                    reason: format!("no handler for kind: {}", claim.kind),
                },
            )
            .await;
            sync_delivery(&state, claim.id).await;
            return;
        }
    };

    let job = Job {
        id: claim.id,
        organization_id: claim.organization_id,
        kind: claim.kind.clone(),
        payload: claim.payload.clone(),
        attempt: claim.attempt,
        max_attempts: claim.max_attempts,
    };

    // Run handler under timeout in a spawned task to catch panics.
    let state_for_handler = state.clone();
    let handle = tokio::spawn(async move {
        tokio::time::timeout(timeout, handler(state_for_handler, job)).await
    });

    let result = match handle.await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_elapsed)) => JobOutcome::Retry {
            after: Duration::ZERO,
            reason: "timeout".into(),
        },
        Err(e) => JobOutcome::Retry {
            after: Duration::ZERO,
            reason: if e.is_panic() { "panic" } else { "cancelled" }.into(),
        },
    };

    apply_outcome(&state, &claim, &result).await;
    sync_delivery(&state, claim.id).await;
}

/// Mirrors a job's outcome onto the delivery row that carries it. Best effort:
/// a failure here is logged and leaves the delivery row as it was, so the next
/// redelivery of the same event is what repairs it (at least once).
///
/// `apply_outcome` is fenced on the claim (id, holder, attempt), so when it
/// returns the job's own state is settled; a stale claim changed nothing. This
/// reads the job back rather than trusting the local outcome, so it can never
/// disagree with what the runner recorded. `done` marks the delivery
/// delivered, `dead` marks it dead (doctor warns, `jobs requeue` recovers it),
/// `pending` is a retry waiting for its next attempt time, and `running`
/// (a claim that has not recorded an outcome yet) is `delivering`.
async fn sync_delivery(state: &AppState, job_id: Uuid) {
    if let Err(e) = sqlx::query(
        "UPDATE outbox_deliveries d SET \
             state = CASE j.state WHEN 'done' THEN 'delivered' \
                                  WHEN 'dead' THEN 'dead' \
                                  WHEN 'pending' THEN 'pending' \
                                  WHEN 'running' THEN 'delivering' \
                                  ELSE d.state END, \
             attempts = j.attempt, \
             next_attempt_at = CASE j.state WHEN 'pending' THEN j.not_before \
                                            ELSE d.next_attempt_at END, \
             last_error = j.last_error, \
             updated_at = now() \
         FROM jobs j \
         WHERE d.job_id = j.id AND j.id = $1 \
           AND d.state NOT IN ('delivered', 'dead') \
           AND j.state IN ('pending', 'running', 'done', 'dead')",
    )
    .bind(job_id)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%job_id, error = %e, "delivery row update failed");
    }
}

async fn retention_sweep(state: &AppState, retention_days: u64) {
    let _ = sqlx::query(
        "DELETE FROM jobs WHERE state = 'done' AND updated_at < now() - make_interval(days => $1)",
    )
    .bind(retention_days as i32)
    .execute(&state.pool)
    .await;
}

/// Starts the runner. `job_concurrency` slots run handlers independently; the
/// loop claims again at once after a claim and sleeps `job_poll_interval_ms`
/// otherwise. On `shutdown` it stops claiming, waits for in-flight handlers up to
/// their timeout, then returns any job it still holds to `pending` (ADR 0006).
pub fn spawn(
    state: AppState,
    registry: Registry,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let registry = Arc::new(registry);
        let holder = Uuid::new_v4();
        let poll = Duration::from_millis(state.config.job_poll_interval_ms);
        let default_timeout = Duration::from_secs(state.config.job_timeout_seconds);
        let default_lease = (default_timeout * 2).as_secs_f64();
        let leases = registry.leases();
        // A shutdown waits for the longest budget any handler may use, not
        // just the generic one, before returning its jobs to `pending`.
        let backoff_override = registry.backoff_override;
        let shutdown_wait = registry
            .longest_budget()
            .unwrap_or(default_timeout)
            .max(default_timeout)
            + Duration::from_secs(1);
        let slots = Arc::new(tokio::sync::Semaphore::new(state.config.job_concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        let mut last_retention: Option<Instant> = None;

        while !shutdown.is_cancelled() {
            recover_leases(&state).await;
            let mut claimed = false;
            while let Ok(permit) = slots.clone().try_acquire_owned() {
                let Some(claim) =
                    claim_one(&state, holder, default_lease, &leases, backoff_override).await
                else {
                    break;
                };
                claimed = true;
                let (state, registry) = (state.clone(), registry.clone());
                tasks.spawn(async move {
                    run_job(state, registry, claim, default_timeout).await;
                    drop(permit);
                });
            }
            while tasks.try_join_next().is_some() {}
            if last_retention.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
                retention_sweep(&state, state.config.job_retention_days).await;
                last_retention = Some(Instant::now());
            }
            if !claimed {
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        }

        // Handlers run under their kind's budget, so in-flight work ends by
        // the longest of them.
        let _ = tokio::time::timeout(shutdown_wait, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        let _ = sqlx::query(
            "UPDATE jobs SET state = 'pending', lease_holder = NULL, lease_expires_at = NULL, \
             updated_at = now() WHERE state = 'running' AND lease_holder = $1",
        )
        .bind(holder)
        .execute(&state.pool)
        .await;
        // The delivery follows the job it was mirrored from: a job returned to
        // `pending` at shutdown is no longer `delivering`.
        let _ = sqlx::query(
            "UPDATE outbox_deliveries d SET state = 'pending', updated_at = now() \
             FROM jobs j WHERE d.job_id = j.id AND d.state = 'delivering' \
               AND j.state = 'pending'",
        )
        .execute(&state.pool)
        .await;
    })
}
