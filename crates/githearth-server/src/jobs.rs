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

pub struct Registry {
    handlers: HashMap<String, Handler>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn register(&mut self, kind: &str, handler: Handler) {
        self.handlers.insert(kind.to_string(), handler);
    }

    fn get(&self, kind: &str) -> Option<&Handler> {
        self.handlers.get(kind)
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
    lease_holder: Uuid,
}

pub async fn enqueue(
    conn: &mut PgConnection,
    organization_id: Option<Uuid>,
    kind: &str,
    payload: serde_json::Value,
    dedupe_key: &str,
    delay: Option<Duration>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, kind, payload, dedupe_key, not_before) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6)) \
         ON CONFLICT (kind, dedupe_key) WHERE state IN ('pending','running') DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id)
    .bind(kind)
    .bind(payload)
    .bind(dedupe_key)
    .bind(delay.unwrap_or_default().as_secs_f64())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn backoff_delay(attempt: i32, after: Option<Duration>) -> Duration {
    let base = Duration::from_secs(1);
    let shift = (attempt.saturating_sub(1).min(9)) as u32;
    let scale: u32 = 1u32 << shift;
    let delay = base * scale;
    let delay = match after {
        Some(a) => delay.max(a),
        None => delay,
    }
    .min(Duration::from_secs(600));
    let jitter_pct: f64 = rand::thread_rng().gen_range(-0.25..0.25);
    let jitter_ms = (delay.as_millis() as f64 * jitter_pct) as i64;
    if jitter_ms < 0 {
        delay.saturating_sub(Duration::from_millis(jitter_ms.unsigned_abs()))
    } else {
        delay + Duration::from_millis(jitter_ms as u64)
    }
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

async fn claim_one(state: &AppState, holder: Uuid, lease_secs: f64) -> Option<RunnerClaim> {
    let row = sqlx::query(
        "UPDATE jobs SET state = 'running', attempt = attempt + 1, lease_holder = $1, \
         lease_expires_at = now() + $2 * interval '1 second', updated_at = now() \
         WHERE id = (SELECT id FROM jobs WHERE state = 'pending' AND not_before <= now() \
         ORDER BY not_before FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING id, organization_id, kind, payload, attempt, max_attempts, lease_holder",
    )
    .bind(holder)
    .bind(lease_secs)
    .fetch_optional(&state.pool)
    .await;

    match row {
        Ok(Some(row)) => {
            let payload: serde_json::Value = row.get("payload");
            Some(RunnerClaim {
                id: row.get("id"),
                organization_id: row.get("organization_id"),
                kind: row.get("kind"),
                payload,
                attempt: row.get("attempt"),
                max_attempts: row.get("max_attempts"),
                lease_holder: row.get("lease_holder"),
            })
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "job claim query failed");
            None
        }
    }
}

async fn apply_outcome(state: &AppState, claim: &RunnerClaim, outcome: &JobOutcome) {
    let (new_state, not_before, last_error): (&str, Option<Duration>, Option<String>) =
        match outcome {
            JobOutcome::Done => ("done", None, None),
            JobOutcome::Retry { after, reason } if claim.attempt < claim.max_attempts => {
                let d = backoff_delay(claim.attempt, Some(*after));
                ("pending", Some(d), Some(reason.clone()))
            }
            JobOutcome::Retry { reason, .. } => {
                let d = backoff_delay(claim.attempt, None);
                (
                    "dead",
                    Some(d),
                    Some(format!("retry limit exceeded: {reason}")),
                )
            }
            JobOutcome::Failed { reason } => ("dead", None, Some(reason.clone())),
        };

    let fenced = if let Some(not_before) = not_before {
        let delay_ms = not_before.as_millis() as i64;
        sqlx::query(
            "UPDATE jobs SET state = $1, not_before = now() + make_interval(millis => $2), \
             last_error = $3, updated_at = now() \
             WHERE id = $4 AND state = 'running' AND lease_holder = $5 AND attempt = $6",
        )
        .bind(new_state)
        .bind(delay_ms)
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

async fn run_job(state: AppState, registry: Arc<Registry>, claim: RunnerClaim, timeout: Duration) {
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
        let timeout = Duration::from_secs(state.config.job_timeout_seconds);
        let lease = (timeout * 2).as_secs_f64();
        let slots = Arc::new(tokio::sync::Semaphore::new(state.config.job_concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        let mut last_retention: Option<Instant> = None;

        while !shutdown.is_cancelled() {
            recover_leases(&state).await;
            let mut claimed = false;
            while let Ok(permit) = slots.clone().try_acquire_owned() {
                let Some(claim) = claim_one(&state, holder, lease).await else {
                    break;
                };
                claimed = true;
                let (state, registry) = (state.clone(), registry.clone());
                tasks.spawn(async move {
                    run_job(state, registry, claim, timeout).await;
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

        // Handlers run under `timeout`, so in-flight work ends by then.
        let _ = tokio::time::timeout(timeout + Duration::from_secs(1), async {
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
    })
}
