//! Repository lifecycle reconciler (ADR 0003 "Repository lifecycle").
//!
//! Converges PostgreSQL repository rows with on-disk storage after crashes or
//! failed requests. One pass (`reconcile_once`) visits every repository whose
//! row is `creating` or `deleting` and whose `updated_at` is older than a
//! configurable grace period, brings storage into agreement, and cleans up
//! orphans, stale tmp entries and expired trash.
//!
//! Rows are ordered by `updated_at`; each pass fetches up to 1000 rows.  If
//! more than 1000 repositories are pending the remainder is handled in
//! subsequent passes.

use crate::AppState;
use crate::repositories;
use githearth_core::RepositoryStatus;
use githearth_git::layout::{ObjectFormat, TmpEntry};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Maximum number of consecutive create failures before marking `failed`.
const MAX_CREATE_ATTEMPTS: u32 = 5;

/// How many pending rows to fetch in one pass (see module docs).
const PENDING_LIMIT: i64 = 1000;

/// Lightweight row for the reconciler's own query.
struct PendingRow {
    id: Uuid,
    status: RepositoryStatus,
    operation_id: Uuid,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub activated: usize,
    pub failed: usize,
    pub deleted: usize,
    pub orphaned: usize,
    pub tmp_cleaned: usize,
    pub trash_purged: usize,
    pub errors: usize,
}

impl Summary {
    pub fn has_changes(&self) -> bool {
        self.activated > 0
            || self.failed > 0
            || self.deleted > 0
            || self.orphaned > 0
            || self.tmp_cleaned > 0
            || self.trash_purged > 0
            || self.errors > 0
    }
}

/// Run one reconciliation pass.  `attempts` tracks consecutive create failures
/// across passes; the caller (typically `spawn`) owns it.
pub async fn reconcile_once(
    state: &AppState,
    attempts: &mut HashMap<Uuid, u32>,
) -> anyhow::Result<Summary> {
    let mut summary = Summary::default();
    let grace = Duration::from_secs(state.config.reconcile_grace_seconds);

    // ---- list tmp candidates FIRST (blocking) ----------------------------
    // Must happen before we read the live operation set: any tmp directory
    // that exists now belongs to a `creating` row that was committed before
    // the directory was created (API ordering: INSERT row, THEN mkdir tmp/).
    // Directories created after this listing are not candidates and are safe.
    let storage = state.storage.clone();
    let candidates = tokio::task::spawn_blocking(move || storage.list_tmp())
        .await
        .map_err(|e| anyhow::anyhow!("list_tmp task panicked: {e}"))??;

    // ---- scan on-disk state (blocking) -----------------------------------
    let storage = state.storage.clone();
    let scan = tokio::task::spawn_blocking(move || storage.scan())
        .await
        .map_err(|e| anyhow::anyhow!("scan task panicked: {e}"))??;

    for malformed in &scan.malformed {
        tracing::warn!(malformed, "malformed repository directory; not touched");
    }

    let scanned: HashSet<Uuid> = scan.repositories.iter().copied().collect();

    // ---- fetch pending rows (own query, grace-aware) -----------------------
    let mut conn = state.pool.acquire().await?;

    // Rows eligible for processing: creating/deleting, past the grace period.
    let pending = fetch_pending(&mut conn, grace).await?;

    // Collect the set of creating ids from *this* pass (for attempts pruning).
    let mut pass_creating: HashSet<Uuid> = HashSet::new();

    // ---- query DB status for every on-disk repository ----------------------
    let mut db_status: HashMap<Uuid, RepositoryStatus> = HashMap::new();
    if !scanned.is_empty() {
        let ids: Vec<Uuid> = scanned.iter().copied().collect();
        let rows = sqlx::query("SELECT id, status FROM repositories WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            let id: Uuid = row.get("id");
            let raw: String = row.get("status");
            if let Ok(status) = raw.parse::<RepositoryStatus>() {
                db_status.insert(id, status);
            }
        }
    }

    // ---- process each pending row ------------------------------------------
    for row in &pending {
        if row.status == RepositoryStatus::Creating {
            pass_creating.insert(row.id);
        }

        let result = match row.status {
            RepositoryStatus::Creating => {
                reconcile_creating(state, &mut conn, row.id, row.operation_id, attempts).await
            }
            RepositoryStatus::Deleting => {
                reconcile_deleting(state, &mut conn, row.id, &scanned).await
            }
            _ => continue,
        };

        match result {
            Ok(Some(change)) => match change {
                Change::Activated => summary.activated += 1,
                Change::Failed => summary.failed += 1,
                Change::Deleted => summary.deleted += 1,
            },
            Ok(None) => { /* CAS race – concurrent request won, skip */ }
            Err(e) => {
                summary.errors += 1;
                tracing::warn!(repository_id = %row.id, error = %e, "reconcile error");
            }
        }
    }

    // ---- prune attempts map ------------------------------------------------
    // Keep only entries for ids that were in this pass's creating set.
    // (Rows that became active/failed had their entries removed already.)
    attempts.retain(|id, _| pass_creating.contains(id));

    // ---- orphans -----------------------------------------------------------
    // Every on-disk repository with no row, or whose row is `deleted`, is an
    // orphan.  Rows that are `creating` or `deleting` are *not* orphans –
    // their directory may legitimately exist.
    for id in &scan.repositories {
        match db_status.get(id) {
            None => {
                if let Err(e) = orphan_repo(state, *id).await {
                    summary.errors += 1;
                    tracing::warn!(repository_id = %id, error = %e, "orphan move failed");
                } else {
                    summary.orphaned += 1;
                }
            }
            Some(RepositoryStatus::Deleted) => {
                if let Err(e) = orphan_repo(state, *id).await {
                    summary.errors += 1;
                    tracing::warn!(repository_id = %id, error = %e, "orphan move failed");
                } else {
                    summary.orphaned += 1;
                }
            }
            Some(RepositoryStatus::Creating | RepositoryStatus::Deleting) => {
                // Not an orphan – the directory may legitimately exist.
            }
            _ => { /* active, failed – has a valid row, not an orphan */ }
        }
    }

    // ---- tmp cleanup (two-phase) -------------------------------------------
    // Phase 1 (list_tmp) ran at the top of this pass, before any DB queries.
    // Phase 2 reads the live set NOW and cleans using the earlier candidates.
    // Ordering: candidates listed → live set queried → clean with candidates.
    // This is safe because the API commits the creating row BEFORE it creates
    // tmp/<operation>; any directory seen in phase 1 belongs to a row that
    // phase 2 will observe if it is still creating.
    // Uses the caller's connection (no second pool acquire) so the pass works
    // even with database_max_connections = 1.
    summary.tmp_cleaned = clean_tmp_step(&mut conn, &state.storage, candidates).await?;

    // ---- trash purge -------------------------------------------------------
    let retention = Duration::from_secs(state.config.trash_retention_seconds);
    let older_than = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let storage = state.storage.clone();
    let purged = tokio::task::spawn_blocking(move || storage.purge_trash(older_than))
        .await
        .map_err(|e| anyhow::anyhow!("purge_trash task panicked: {e}"))??;
    summary.trash_purged = purged;

    Ok(summary)
}

/// Fetch rows eligible for reconciliation: `creating` or `deleting`, past the
/// grace period, ordered by `updated_at`.
async fn fetch_pending(
    conn: &mut sqlx::PgConnection,
    grace: Duration,
) -> Result<Vec<PendingRow>, sqlx::Error> {
    // Compare on the database clock, which also wrote `updated_at`.
    let rows = sqlx::query(
        "SELECT id, status, operation_id FROM repositories \
         WHERE status IN ('creating','deleting') \
         AND updated_at < now() - make_interval(secs => $1) \
         ORDER BY updated_at LIMIT $2",
    )
    .bind(grace.as_secs_f64())
    .bind(PENDING_LIMIT)
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let raw: String = row.get("status");
            PendingRow {
                id: row.get("id"),
                status: raw.parse().expect("database status is always valid"),
                operation_id: row.get("operation_id"),
            }
        })
        .collect())
}

/// Fetch ALL `creating` operation ids, regardless of grace period, so that
/// in-flight create tmp directories are not cleaned.
async fn fetch_live_operations(
    conn: &mut sqlx::PgConnection,
) -> Result<HashSet<Uuid>, sqlx::Error> {
    let rows = sqlx::query_scalar::<_, Uuid>(
        "SELECT operation_id FROM repositories WHERE status='creating'",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Second phase of two-phase tmp cleanup.
///
/// `candidates` must have been obtained via [`Storage::list_tmp`] *before*
/// this function is called.  This function reads the current live `creating`
/// operation set from the database (using the caller's connection — it does
/// NOT acquire from the pool) and then calls [`Storage::clean_tmp`] with the
/// earlier candidates.  Because the API inserts the `creating` row before it
/// creates `tmp/<operation>`, any directory that was present when `list_tmp`
/// ran belongs to a row that this query will observe (if it is still
/// `creating`).  Directories created *after* `list_tmp` are not in
/// `candidates` and therefore safe from removal.
pub async fn clean_tmp_step(
    conn: &mut sqlx::PgConnection,
    storage: &std::sync::Arc<githearth_git::layout::Storage>,
    candidates: Vec<TmpEntry>,
) -> anyhow::Result<usize> {
    let live = fetch_live_operations(conn).await?;
    let storage = storage.clone();
    let cleaned = tokio::task::spawn_blocking(move || storage.clean_tmp(&candidates, &live))
        .await
        .map_err(|e| anyhow::anyhow!("clean_tmp task panicked: {e}"))??;
    Ok(cleaned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Activated,
    Failed,
    Deleted,
}

async fn reconcile_creating(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    repo_id: Uuid,
    op_id: Uuid,
    attempts: &mut HashMap<Uuid, u32>,
) -> anyhow::Result<Option<Change>> {
    let storage = state.storage.clone();

    let create_result =
        tokio::task::spawn_blocking(move || storage.create(repo_id, op_id, ObjectFormat::Sha1))
            .await
            .map_err(|e| anyhow::anyhow!("create task panicked: {e}"))?;

    match create_result {
        Ok(_) => {
            attempts.remove(&repo_id);
            let ok = repositories::set_status(
                conn,
                repo_id,
                RepositoryStatus::Creating,
                RepositoryStatus::Active,
            )
            .await?;
            if ok {
                tracing::info!(repository_id = %repo_id, "reconciler activated creating repository");
                Ok(Some(Change::Activated))
            } else {
                Ok(None)
            }
        }
        Err(e) => {
            let current = attempts.get(&repo_id).copied().unwrap_or(0) + 1;
            attempts.insert(repo_id, current);
            tracing::warn!(
                repository_id = %repo_id,
                attempt = current,
                error = %e,
                "reconciler create attempt failed"
            );
            if current >= MAX_CREATE_ATTEMPTS {
                attempts.remove(&repo_id);
                let ok = repositories::set_status(
                    conn,
                    repo_id,
                    RepositoryStatus::Creating,
                    RepositoryStatus::Failed,
                )
                .await?;
                if ok {
                    tracing::error!(
                        repository_id = %repo_id,
                        attempts = current,
                        "reconciler marked creating repository as failed after {MAX_CREATE_ATTEMPTS} attempts"
                    );
                    Ok(Some(Change::Failed))
                } else {
                    Ok(None)
                }
            } else {
                Ok(None)
            }
        }
    }
}

async fn reconcile_deleting(
    state: &AppState,
    conn: &mut sqlx::PgConnection,
    repo_id: Uuid,
    scanned: &HashSet<Uuid>,
) -> anyhow::Result<Option<Change>> {
    if scanned.contains(&repo_id) {
        let storage = state.storage.clone();
        tokio::task::spawn_blocking(move || storage.trash(repo_id))
            .await
            .map_err(|e| anyhow::anyhow!("trash task panicked: {e}"))??;
    }
    let ok = repositories::set_status(
        conn,
        repo_id,
        RepositoryStatus::Deleting,
        RepositoryStatus::Deleted,
    )
    .await?;
    if ok {
        tracing::info!(repository_id = %repo_id, "reconciler deleted repository");
        Ok(Some(Change::Deleted))
    } else {
        Ok(None)
    }
}

async fn orphan_repo(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    let storage = state.storage.clone();
    tokio::task::spawn_blocking(move || storage.orphan(id))
        .await
        .map_err(|e| anyhow::anyhow!("orphan task panicked: {e}"))??;
    tracing::warn!(repository_id = %id, "orphaned on-disk repository with no corresponding metadata row");
    Ok(())
}

/// Spawn the reconciler background task.  Runs one pass at startup, then
/// every `reconcile_interval_seconds`.  Never panics or exits the loop on
/// errors.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(state.config.reconcile_interval_seconds);
        let mut attempts: HashMap<Uuid, u32> = HashMap::new();

        run_pass(&state, &mut attempts).await;

        let mut tick = tokio::time::interval(interval);
        tick.tick().await;

        loop {
            tick.tick().await;
            run_pass(&state, &mut attempts).await;
        }
    })
}

async fn run_pass(state: &AppState, attempts: &mut HashMap<Uuid, u32>) {
    match reconcile_once(state, attempts).await {
        Ok(summary) => {
            if summary.has_changes() {
                tracing::info!(
                    activated = summary.activated,
                    failed = summary.failed,
                    deleted = summary.deleted,
                    orphaned = summary.orphaned,
                    tmp_cleaned = summary.tmp_cleaned,
                    trash_purged = summary.trash_purged,
                    errors = summary.errors,
                    "reconciler pass complete"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "reconciler pass failed; will retry");
        }
    }
}
