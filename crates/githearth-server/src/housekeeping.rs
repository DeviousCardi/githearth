//! Repository housekeeping: scheduled repack and grace-bounded pruning
//! (ADR 0013), as a durable job (ADR 0006).
//!
//! One run maintains one repository:
//!
//! 1. Skip it when it is not `active`, is archived, or housekeeping is
//!    disabled by configuration.
//! 2. Take the per-repository lock **before any pool connection** (the
//!    lock-ordering rule in `reconcile.rs`), so a push or merge on the same
//!    repository either finishes first or waits for this run.
//! 3. Under the lock, replay the push journal. A journal that cannot be
//!    reconciled may still need objects, so the run defers instead of pruning.
//! 4. Collect the keep points: every merge candidate whose intent has not
//!    reached its ref update yet.
//! 5. Repack, delete unreachable objects past the grace period, verify (in a
//!    blocking task that owns the lock guard).
//! 6. Record the sizes and one audit event in the same transaction.
//!
//! Two runs for one repository never overlap: the job's `dedupe_key` is the
//! repository id, so a second enqueue is a no-op while the first is pending or
//! running, and the per-repository lock serializes a hand-started run with a
//! scheduled one.

use crate::AppState;
use crate::jobs::{self, Job, JobOutcome};
use githearth_core::RepositoryStatus;
use githearth_git::maintenance::{self, Deferred, Faults, KeepPoints, Plan, Report};
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// The job kind and the audit event kind share this name.
pub const JOB_KIND: &str = "repository.housekeeping";

/// One-shot interruption points, keyed by repository id. Tests set one for
/// their own repository; reading it clears it, so the run that follows an
/// injected failure completes normally: exactly the "a restart finishes the
/// work" property ADR 0013 claims. Production never sets an entry.
///
/// `slow` is a test-only delay inside the run's blocking work. It lets the
/// durable-job test prove that a run longer than `job_timeout_seconds` still
/// completes under the job kind's own budget.
#[derive(Clone, Copy, Default)]
struct Interruption {
    crash_at: Option<maintenance::FaultPoint>,
    slow: Option<Duration>,
    scan_budget: Option<Duration>,
}

fn faults() -> &'static Mutex<HashMap<Uuid, Interruption>> {
    static FAULTS: OnceLock<Mutex<HashMap<Uuid, Interruption>>> = OnceLock::new();
    FAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Interrupt the next run of `repository_id` at `point`.
pub fn set_fault(repository_id: Uuid, point: maintenance::FaultPoint) {
    faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(repository_id)
        .or_default()
        .crash_at = Some(point);
}

/// Make the next run of `repository_id` take at least `delay` before it does
/// its Git steps. Test-only, like [`set_fault`].
pub fn set_slow(repository_id: Uuid, delay: Duration) {
    faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(repository_id)
        .or_default()
        .slow = Some(delay);
}

/// Give the next run of `repository_id` a scan/deletion deadline of `budget`
/// instead of one step timeout. Test-only; a zero budget makes the scan stop
/// immediately so a test can assert the run does not claim a prune.
pub fn set_scan_budget(repository_id: Uuid, budget: Duration) {
    faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(repository_id)
        .or_default()
        .scan_budget = Some(budget);
}

fn take_interruption(repository_id: Uuid) -> Interruption {
    faults()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&repository_id)
        .unwrap_or_default()
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome {
    /// The repository was maintained.
    Completed(Report),
    /// The repository is not eligible (not active, archived, disabled, gone).
    Skipped(&'static str),
    /// The run started but must not prune yet (quarantine, unreconciled
    /// journal). Recorded for the operator; the next scheduled run retries.
    Deferred(Deferred),
}

/// Registers the job kind with its own budget: every bounded phase of a run
/// plus slack. Without it the runner's generic `job_timeout_seconds` (60 s by
/// default) would cut a real run off mid-step, record nothing and retry
/// forever.
pub fn register(registry: &mut jobs::Registry, config: &crate::config::Config) {
    registry.register_with_budget(
        JOB_KIND,
        handler(),
        Duration::from_secs(config.housekeeping_job_timeout_seconds()),
    );
}

fn handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: Job| {
        Box::pin(async move { run_job(state, job).await })
    })
}

async fn run_job(state: AppState, job: Job) -> JobOutcome {
    let Some(repository_id) = payload_repository(&job.payload) else {
        return JobOutcome::Failed {
            reason: "missing or invalid `repository_id` in payload".into(),
        };
    };
    if !state.config.housekeeping_enabled {
        return JobOutcome::Done;
    }
    // A direct enqueue (unarchive, an operator follow-up) can bypass the
    // scheduler's `housekeeping_concurrency` allowance. This semaphore is the
    // run-side bound the job budget assumes, so the number of simultaneous
    // Git runs never exceeds the configured concurrency.
    let permit = state.housekeeping_slots.clone().acquire_owned().await;
    let Ok(_permit) = permit else {
        return JobOutcome::Retry {
            after: Duration::from_secs(5),
            reason: "housekeeping concurrency gate closed".into(),
        };
    };
    match run_repository(&state, repository_id).await {
        Ok(Outcome::Completed(report)) => {
            tracing::info!(
                repository_id = %repository_id,
                bytes_before = report.bytes_before,
                bytes_after = report.bytes_after,
                expired_objects = report.expired,
                "repository housekeeping completed"
            );
            JobOutcome::Done
        }
        Ok(Outcome::Skipped(reason)) => {
            tracing::debug!(repository_id = %repository_id, reason, "repository housekeeping skipped");
            JobOutcome::Done
        }
        Ok(Outcome::Deferred(deferred)) => {
            tracing::info!(
                repository_id = %repository_id,
                reason = %deferred,
                "repository housekeeping deferred"
            );
            JobOutcome::Done
        }
        Err(error) => {
            // The error was already recorded on the repository's row without
            // anything a retry could not fix.
            tracing::warn!(
                repository_id = %repository_id,
                error = %error,
                "repository housekeeping failed; will retry"
            );
            JobOutcome::Retry {
                // Short: a failed run is safe to repeat, and the runner's own
                // exponential backoff bounds a persistent failure.
                after: Duration::from_secs(5),
                reason: "housekeeping failed".into(),
            }
        }
    }
}

fn payload_repository(payload: &Value) -> Option<Uuid> {
    payload
        .get("repository_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Run maintenance for one repository. Safe to call directly (tests, the job
/// handler); the job handler is a thin wrapper. Honours
/// `housekeeping_enabled`.
pub async fn run_repository(
    state: &AppState,
    repository_id: Uuid,
) -> Result<Outcome, HousekeepingError> {
    run(state, repository_id, false).await
}

/// Run maintenance for one repository now, even when the scheduler is
/// disabled. The explicit operator command uses this: disabling the schedule
/// stops automatic work, not a deliberate one-off run.
pub async fn run_repository_now(
    state: &AppState,
    repository_id: Uuid,
) -> Result<Outcome, HousekeepingError> {
    run(state, repository_id, true).await
}

async fn run(
    state: &AppState,
    repository_id: Uuid,
    explicit: bool,
) -> Result<Outcome, HousekeepingError> {
    if !explicit && !state.config.housekeeping_enabled {
        return Ok(Outcome::Skipped("housekeeping is disabled"));
    }

    // Lock BEFORE any pool connection. The guard moves into the blocking task,
    // so a cancelled request cannot release it while Git still runs.
    let guard = state
        .storage
        .repository_lock(repository_id)
        .lock_owned()
        .await;

    // Read the facts under the lock: a delete or archive that commits while
    // the lock is pending must not be overtaken by this run.
    let Some(row) = repository_row(state, repository_id).await? else {
        return Ok(Outcome::Skipped("repository not found"));
    };
    if row.status != RepositoryStatus::Active {
        return Ok(Outcome::Skipped("repository is not active"));
    }
    if row.archived {
        return Ok(Outcome::Skipped("repository is archived"));
    }

    // The journal must be reconciled before anything is deleted: its records
    // may still name objects that no ref reaches yet.
    if let Err(error) = crate::pushes::replay(state, repository_id).await {
        let reason = error.to_string();
        record_outcome(
            state,
            repository_id,
            row.organization_id,
            row.created_by,
            OutcomeRow::deferred(&reason),
        )
        .await?;
        return Ok(Outcome::Deferred(Deferred::JournalNotReplayed { reason }));
    }

    // A pull request may have been force-pushed, or opened before pins existed;
    // refresh the pin for every open pull request whose head lives here, while
    // the repository lock excludes its writers.
    refresh_open_pins(state, repository_id).await?;

    let mut conn = state.pool.acquire().await?;
    let keep = keep_points(&mut conn, repository_id).await?;
    drop(conn);

    let timeout = Duration::from_secs(state.config.housekeeping_timeout_seconds);
    let repo_path = state.storage.repository_path(repository_id);
    let interruption = take_interruption(repository_id);
    // A push cannot outlive `git_timeout_seconds`, so twice that is safely
    // older than any live quarantine; anything older is a killed process's
    // leftover and is removed instead of deferring the run forever.
    let quarantine_max_age =
        Duration::from_secs(state.config.git_timeout_seconds.saturating_mul(2));
    let plan = Plan::new(Duration::from_secs(state.config.housekeeping_grace_seconds))
        .with_timeout(timeout)
        .with_keep(keep)
        .with_quarantine_max_age(quarantine_max_age)
        .with_max_live_output(state.config.housekeeping_max_live_set_bytes)
        .with_faults(Faults {
            crash_at: interruption.crash_at,
            slow: interruption.slow,
            slow_repack: None,
            scan_budget: interruption.scan_budget,
        });

    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        maintenance::run(&repo_path, &plan)
    })
    .await
    .map_err(|e| HousekeepingError::Join(e.to_string()))?;

    match result {
        Ok(report) => {
            let bytes_before = i64::try_from(report.bytes_before).unwrap_or(i64::MAX);
            let bytes_after = i64::try_from(report.bytes_after).unwrap_or(i64::MAX);
            let expired = i32::try_from(report.expired).unwrap_or(i32::MAX);
            record_outcome(
                state,
                repository_id,
                row.organization_id,
                row.created_by,
                OutcomeRow::completed(bytes_before, bytes_after, expired),
            )
            .await?;
            Ok(Outcome::Completed(report))
        }
        Err(githearth_git::StorageError::Deferred(deferred)) => {
            record_outcome(
                state,
                repository_id,
                row.organization_id,
                row.created_by,
                OutcomeRow::deferred(&deferred.to_string()),
            )
            .await?;
            Ok(Outcome::Deferred(deferred))
        }
        Err(error) => {
            let reason = error.to_string();
            record_outcome(
                state,
                repository_id,
                row.organization_id,
                row.created_by,
                OutcomeRow::failed(&reason),
            )
            .await?;
            Err(HousekeepingError::Storage(reason))
        }
    }
}

#[derive(Debug)]
pub enum HousekeepingError {
    Database(sqlx::Error),
    Join(String),
    Storage(String),
}

impl std::fmt::Display for HousekeepingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "database error: {e}"),
            Self::Join(e) => write!(f, "blocking task failed: {e}"),
            Self::Storage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for HousekeepingError {}

impl From<sqlx::Error> for HousekeepingError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}

struct RepositoryFacts {
    status: RepositoryStatus,
    archived: bool,
    created_by: Uuid,
    organization_id: Uuid,
}

async fn repository_row(
    state: &AppState,
    repository_id: Uuid,
) -> Result<Option<RepositoryFacts>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, bool, Uuid, Uuid)>(
        "SELECT status, archived, created_by, organization_id FROM repositories WHERE id = $1",
    )
    .bind(repository_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(
        |(status, archived, created_by, organization_id)| RepositoryFacts {
            status: status.parse().unwrap_or(RepositoryStatus::Failed),
            archived,
            created_by,
            organization_id,
        },
    ))
}

/// Every active merge candidate and every pinned pull-request head: work that
/// is real but not yet reachable from a ref (ADR 0013 "What is live", 3 and 7).
async fn keep_points(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<KeepPoints, sqlx::Error> {
    // A finished intent needs no pin: its result is reachable from the ref it
    // moved, and pinning a `merged` result that a later force push makes
    // unreachable would keep it forever. `candidate_oid` is written by
    // `merge::execute_intent` under the same lock, before the ref update.
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT candidate_oid, result_oid FROM merge_intents \
         WHERE repository_id = $1 AND state IN ('validating', 'executing')",
    )
    .bind(repository_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut commits = Vec::new();
    for (candidate, result) in rows {
        for oid in [candidate, result].into_iter().flatten() {
            if crate::pushes::valid_oid(&oid) {
                commits.push(oid);
            }
        }
    }
    // Pins are not Git refs and are never advertised; they exist only so the
    // live-set walk can start from a closed pull request's last head.
    commits.extend(crate::pins::keep_commits(&mut *conn, repository_id).await?);
    commits.sort();
    commits.dedup();
    Ok(KeepPoints { commits })
}

/// Backfills a base pin for every open or recently closed pull request that
/// has none, at server start and before the scheduler can enqueue a run.
///
/// A pull request row does not record the commit its target branch pointed at
/// when it was opened, and 0046 cannot read Git, so this is the one pin the
/// migration cannot write. It reads each repository's branch tips once and
/// pins the target branch's current tip for every such request: for a request
/// whose target branch has not moved, that *is* the base; for one that has
/// moved, it is a later commit that still reaches the original base (a branch
/// tip reaches all of its ancestors). Requests opened after this version pin
/// their exact base in their own create transaction.
///
/// Runs over every active repository once, best effort per repository: a
/// repository whose storage is unreadable is reported and skipped, and the
/// next start retries it. Returns how many pins it wrote.
pub async fn backfill_bases(state: &AppState) -> Result<usize, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT pr.id, pr.repository_id, pr.target_branch FROM pull_requests pr \
         WHERE (pr.state = 'open' \
                OR (pr.state = 'closed' AND pr.closed_at > now() - make_interval(days => $1))) \
           AND NOT EXISTS (SELECT 1 FROM pinned_commits p WHERE p.base_of_pull_request_id = pr.id)",
    )
    .bind(days(state.config.housekeeping_pin_retention_days))
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let timeout = Duration::from_secs(
        state
            .config
            .git_timeout_seconds
            .min(state.config.housekeeping_timeout_seconds),
    );
    let mut tips: HashMap<Uuid, HashMap<String, String>> = HashMap::new();
    for (_, repository_id, _) in &rows {
        if tips.contains_key(repository_id) {
            continue;
        }
        let path = state.storage.repository_path(*repository_id);
        match tokio::task::spawn_blocking(move || {
            githearth_git::browse::branch_tips(&path, timeout)
        })
        .await
        {
            Ok(Ok(read)) => {
                tips.insert(*repository_id, read);
            }
            Ok(Err(error)) => {
                tracing::warn!(%repository_id, error = %error, "base pin backfill skipped a repository");
                tips.insert(*repository_id, HashMap::new());
            }
            Err(error) => {
                tracing::warn!(%repository_id, error = %error, "base pin backfill skipped a repository");
                tips.insert(*repository_id, HashMap::new());
            }
        }
    }
    let mut conn = state.pool.acquire().await?;
    let mut written = 0;
    for (pull_id, repository_id, branch) in rows {
        let Some(oid) = tips
            .get(&repository_id)
            .and_then(|read| read.get(&branch))
            .cloned()
        else {
            continue;
        };
        crate::pins::pin_base(
            &mut conn,
            repository_id,
            pull_id,
            &oid,
            "open",
            state.config.housekeeping_pin_retention_days,
        )
        .await?;
        written += 1;
    }
    if written > 0 {
        tracing::info!(written, "pull-request base pins backfilled");
    }
    Ok(written)
}

fn days(retention_days: u64) -> i32 {
    i32::try_from(retention_days).unwrap_or(i32::MAX)
}

/// Extend the pins of every open pull request this repository is part of. A
/// branch that has been deleted keeps its last pin until the retention
/// timestamp; a force push gets the new head pinned immediately.
///
/// A run on the **source** repository pins the head, and, for a fork pull
/// request whose head one read or merge already copied into the target, the
/// target-side copy as well (charged to the target organization). A run on the
/// **target** repository pins the base of every open request into it. Each
/// statement is a no-op when the repository holds no such pin, so both runs
/// can call it.
///
/// The whole refresh is bounded by one step timeout. Branch tips are read in
/// one `for-each-ref` call rather than one `rev-parse` per request, so the
/// repository lock and the pool connection are held for a bounded, small
/// number of operations. A refresh that runs out of time leaves the previous
/// pins in place; they are still live, so skipping the rest only delays
/// following a force push, it never unprotects a head.
async fn refresh_open_pins(state: &AppState, repository_id: Uuid) -> Result<(), HousekeepingError> {
    let retention_days = state.config.housekeeping_pin_retention_days;
    let mut conn = state.pool.acquire().await?;
    // The base pins of every open request into this repository (its target
    // side): an extended window from a close that was later reopened goes back
    // to living with the request. A no-op when this repository is no open
    // request's target.
    sqlx::query(
        "UPDATE pinned_commits SET retain_until = NULL \
         FROM pull_requests pr \
         WHERE pr.id = pinned_commits.base_of_pull_request_id \
           AND pr.state = 'open' AND pr.repository_id = $1 \
           AND pinned_commits.retain_until IS NOT NULL",
    )
    .bind(repository_id)
    .execute(&mut *conn)
    .await?;
    let rows: Vec<(Uuid, String, Uuid)> = sqlx::query_as(
        "SELECT pr.id, pr.source_branch, pr.repository_id FROM pull_requests pr \
         WHERE pr.state = 'open' AND pr.source_repository_id = $1",
    )
    .bind(repository_id)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let timeout = Duration::from_secs(
        state
            .config
            .git_timeout_seconds
            .min(state.config.housekeeping_timeout_seconds),
    );
    let path = state.storage.repository_path(repository_id);
    let deadline = tokio::time::Instant::now() + timeout;
    let tips =
        tokio::task::spawn_blocking(move || githearth_git::browse::branch_tips(&path, timeout))
            .await
            .map_err(|e| HousekeepingError::Join(e.to_string()))?
            .map_err(|e| HousekeepingError::Storage(e.to_string()))?;
    for (pull_request_id, branch, target_repository_id) in rows {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                repository_id = %repository_id,
                "pin refresh ran out of time; the remaining open requests keep their previous pins"
            );
            break;
        }
        if let Some(oid) = tips.get(&branch).cloned() {
            crate::pins::pin_head(
                &mut conn,
                repository_id,
                pull_request_id,
                &oid,
                retention_days,
            )
            .await?;
            if target_repository_id != repository_id {
                // The target keeps a copy of the fork's head (a read or merge
                // copied it). Pin that copy where it lives; the INSERT selects
                // nothing when the target does not hold the object.
                let target_path = state.storage.repository_path(target_repository_id);
                let probe = oid.clone();
                let has_copy =
                    tokio::task::spawn_blocking(move || state_copy_probe(&target_path, &probe))
                        .await
                        .map_err(|e| HousekeepingError::Join(e.to_string()))?;
                if has_copy {
                    crate::pins::pin_target_copy(
                        &mut conn,
                        target_repository_id,
                        pull_request_id,
                        &oid,
                        retention_days,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

/// Whether a bare repository stores object `oid`. A missing directory reads as
/// `false`: a target that is gone has nothing to pin.
fn state_copy_probe(path: &std::path::Path, oid: &str) -> bool {
    std::process::Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("--git-dir")
        .arg(path)
        .args(["cat-file", "-e", oid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The row write for one run: `repository_housekeeping` plus the audit/outbox
/// record, in one transaction, as every mutation does.
struct OutcomeRow {
    /// `(bytes_before, bytes_after, expired)` from a completed run. A failure
    /// or deferral carries `None`, so the last measured sizes survive instead
    /// of being zeroed.
    sizes: Option<(i64, i64, i32)>,
    outcome: &'static str,
    error: Option<String>,
}

impl OutcomeRow {
    fn completed(bytes_before: i64, bytes_after: i64, expired: i32) -> Self {
        Self {
            sizes: Some((bytes_before, bytes_after, expired)),
            outcome: "completed",
            error: None,
        }
    }

    fn deferred(reason: &str) -> Self {
        Self {
            sizes: None,
            outcome: "deferred",
            error: Some(reason.to_owned()),
        }
    }

    fn failed(reason: &str) -> Self {
        Self {
            sizes: None,
            outcome: "failed",
            error: Some(reason.to_owned()),
        }
    }

    /// The reason as stored: bounded, and never longer than a Git one-liner.
    fn bounded(mut self) -> Self {
        self.error = self.error.map(|error| {
            let mut text: String = error.chars().take(500).collect();
            if text.len() < error.len() {
                text.push('…');
            }
            text
        });
        self
    }
}

async fn record_outcome(
    state: &AppState,
    repository_id: Uuid,
    organization_id: Uuid,
    actor: Uuid,
    outcome: OutcomeRow,
) -> Result<(), sqlx::Error> {
    let outcome = outcome.bounded();
    let (bytes_before, bytes_after, expired) = match outcome.sizes {
        Some((before, after, expired)) => (Some(before), Some(after), Some(expired)),
        None => (None, None, None),
    };
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO repository_housekeeping \
         (repository_id, bytes_before, bytes_after, expired_objects, last_run_at, last_outcome, last_error) \
         VALUES ($1, COALESCE($2, 0), COALESCE($3, 0), COALESCE($4, 0), now(), $5, $6) \
         ON CONFLICT (repository_id) DO UPDATE SET \
           bytes_before = COALESCE($2, repository_housekeeping.bytes_before), \
           bytes_after = COALESCE($3, repository_housekeeping.bytes_after), \
           expired_objects = COALESCE($4, repository_housekeeping.expired_objects), \
           last_run_at = EXCLUDED.last_run_at, last_outcome = EXCLUDED.last_outcome, \
           last_error = EXCLUDED.last_error, updated_at = now()",
    )
    .bind(repository_id)
    .bind(bytes_before)
    .bind(bytes_after)
    .bind(expired)
    .bind(outcome.outcome)
    .bind(&outcome.error)
    .execute(&mut *tx)
    .await?;
    // The job is not an account, so the audit row names the repository's
    // creator, as the reconciler's delete path does (ADR 0013 "The durable
    // job"). Payload: ids and sizes only, never content.
    crate::event_with(
        &mut tx,
        Some(organization_id),
        actor,
        JOB_KIND,
        json!({
            "repository_id": repository_id,
            "outcome": outcome.outcome,
            "bytes_before": bytes_before,
            "bytes_after": bytes_after,
            "expired_objects": expired,
        }),
    )
    .await?;
    tx.commit().await
}

/// Enqueue a run for every active, unarchived repository whose last run is
/// older than `housekeeping_interval_seconds`, at most
/// `housekeeping_concurrency` at a time. Returns how many jobs were enqueued.
///
/// Idempotent: a repository that already has a pending or running run is
/// skipped by the dedupe index as well as by the query.
pub async fn enqueue_due(state: &AppState) -> Result<usize, sqlx::Error> {
    if !state.config.housekeeping_enabled {
        return Ok(0);
    }
    let mut conn = state.pool.acquire().await?;
    let in_flight: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND state IN ('pending', 'running')",
    )
    .bind(JOB_KIND)
    .fetch_one(&mut *conn)
    .await?;
    let allowance = (state.config.housekeeping_concurrency as i64 - in_flight).max(0);
    if allowance == 0 {
        return Ok(0);
    }
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM repositories r \
         LEFT JOIN repository_housekeeping h ON h.repository_id = r.id \
         WHERE r.status = 'active' AND NOT r.archived \
           AND (h.last_run_at IS NULL OR h.last_run_at < now() - make_interval(secs => $1)) \
           AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.kind = $2 AND j.dedupe_key = r.id::text \
                           AND j.state IN ('pending', 'running')) \
         ORDER BY h.last_run_at NULLS FIRST, r.id \
         LIMIT $3",
    )
    .bind(state.config.housekeeping_interval_seconds as f64)
    .bind(JOB_KIND)
    .bind(allowance)
    .fetch_all(&mut *conn)
    .await?;
    let mut enqueued = 0;
    for repository_id in due {
        jobs::enqueue(
            &mut conn,
            None,
            JOB_KIND,
            json!({ "repository_id": repository_id }),
            &repository_id.to_string(),
            None,
        )
        .await?;
        enqueued += 1;
    }
    drop(conn);
    if enqueued > 0 {
        tracing::info!(enqueued, "repository housekeeping scheduled");
    }
    Ok(enqueued)
}

/// The scheduler task: enqueue due runs every `housekeeping_interval_seconds`,
/// never more often than once a minute. Never panics or exits on errors.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !state.config.housekeeping_enabled {
            return;
        }
        let period = Duration::from_secs(state.config.housekeeping_interval_seconds).min(
            // A repository is due at most once per interval; checking more
            // often only shortens the delay after a restart.
            Duration::from_secs(60),
        );
        let mut tick = tokio::time::interval(period);
        loop {
            // The first tick fires at once, so a restart catches up instead of
            // waiting a whole period.
            tick.tick().await;
            match enqueue_due(&state).await {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "housekeeping schedule pass failed; will retry");
                }
            }
        }
    })
}
