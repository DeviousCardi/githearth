//! Repository lifecycle reconciler (ADR 0003 "Repository lifecycle").
//!
//! Converges PostgreSQL repository rows with on-disk storage after crashes or
//! failed requests. One pass (`reconcile_once`) visits every repository whose
//! row is `creating`, `deleting` or `restoring` and whose `updated_at` is
//! older than a configurable grace period, brings storage into agreement, and
//! cleans up orphans, stale tmp entries and expired trash. A `restoring` row
//! is completed from trash (or from the directory a crash already moved back)
//! and returns to `deleted` when its trash entry is gone.
//!
//! Rows are ordered by `updated_at`; each pass fetches up to 1000 rows.  If
//! more than 1000 repositories are pending the remainder is handled in
//! subsequent passes.
//!
//! # Lock ordering
//!
//! The per-repository filesystem lock MUST be acquired **before** any pool
//! connection.  Never await the repository lock while holding a connection:
//! the API also takes the lock first and then a connection, so a pass holding
//! the only connection while waiting for the lock would deadlock with it.

use crate::AppState;
use crate::pushes;
use crate::repositories;
use githearth_core::RepositoryStatus;
use githearth_git::layout::TmpEntry;
use sqlx::{Connection, Row};
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
    pub restored: usize,
    pub failed: usize,
    pub deleted: usize,
    pub orphaned: usize,
    pub tmp_cleaned: usize,
    pub trash_purged: usize,
    pub push_credentials_purged: usize,
    pub uploads_cleaned: usize,
    pub errors: usize,
}

impl Summary {
    pub fn has_changes(&self) -> bool {
        self.activated > 0
            || self.restored > 0
            || self.failed > 0
            || self.deleted > 0
            || self.orphaned > 0
            || self.tmp_cleaned > 0
            || self.trash_purged > 0
            || self.push_credentials_purged > 0
            || self.uploads_cleaned > 0
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

    // ---- drop the pass connection before the per-row loop ------------------
    // Each per-row step acquires the repository lock *first*, then its own
    // pool connection.  Holding the pass connection across the lock wait
    // would deadlock with an API request that holds the lock while awaiting
    // a connection.
    drop(conn);

    // ---- process each pending row ------------------------------------------
    for row in &pending {
        if row.status == RepositoryStatus::Creating {
            pass_creating.insert(row.id);
        }

        let result = match row.status {
            RepositoryStatus::Creating => {
                reconcile_creating(state, row.id, row.operation_id, attempts).await
            }
            RepositoryStatus::Deleting => reconcile_deleting(state, row.id, &scanned).await,
            RepositoryStatus::Restoring => reconcile_restoring(state, row.id).await,
            _ => continue,
        };

        match result {
            Ok(Some(change)) => match change {
                Change::Activated => summary.activated += 1,
                Change::Restored => summary.restored += 1,
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
            None => match orphan_repo(state, *id).await {
                Ok(true) => summary.orphaned += 1,
                Ok(false) => { /* status changed under lock, skip */ }
                Err(e) => {
                    summary.errors += 1;
                    tracing::warn!(repository_id = %id, error = %e, "orphan move failed");
                }
            },
            Some(RepositoryStatus::Deleted) => {
                match orphan_repo(state, *id).await {
                    Ok(true) => summary.orphaned += 1,
                    Ok(false) => { /* status changed under lock, skip */ }
                    Err(e) => {
                        summary.errors += 1;
                        tracing::warn!(repository_id = %id, error = %e, "orphan move failed");
                    }
                }
            }
            Some(
                RepositoryStatus::Creating
                | RepositoryStatus::Deleting
                | RepositoryStatus::Restoring,
            ) => {
                // Not an orphan – the directory may legitimately exist while
                // the row is mid-lifecycle (a restore is moving it back).
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
    // Re-acquire a connection: clean_tmp_step takes no repository lock so
    // the lock ordering rule does not apply here.
    let mut conn = state.pool.acquire().await?;
    summary.tmp_cleaned = clean_tmp_step(&mut conn, &state.storage, candidates).await?;
    drop(conn);

    // ---- trash purge (serialized with a restore) ---------------------------
    // Two-phase, like tmp cleanup: list candidates first, then for each one
    // take the per-repository lock BEFORE its own DB connection and re-read
    // the row under that lock. A restore sets `restoring` before taking the
    // same lock, so:
    //   - purge first: it sees `deleted` and removes the entry; the restore
    //     then finds no trash entry and is refused (the purge won);
    //   - restore first: it sets `restoring`, and the purge that follows sees
    //     `restoring` and leaves the entry alone, even mid-move;
    //   - a status-only check without the lock would let the purge remove the
    //     entry between the restore's check and its rename.
    let retention = Duration::from_secs(state.config.trash_retention_seconds);
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let storage = state.storage.clone();
    let candidates = tokio::task::spawn_blocking(move || storage.list_trash())
        .await
        .map_err(|e| anyhow::anyhow!("list_trash task panicked: {e}"))??;
    let mut purged = 0usize;
    for entry in candidates {
        if entry.seconds >= cutoff {
            continue;
        }
        // Lock before any pool connection (module-level lock ordering).
        let guard = state.storage.lock_repository(entry.repository).await?;
        let mut conn = state.pool.acquire().await?;
        let status = repositories::get(&mut conn, entry.repository)
            .await?
            .map(|row| row.status);
        drop(conn);
        if status == Some(RepositoryStatus::Restoring) {
            // The restore owns this entry; leave it for the next pass.
            continue;
        }
        let repository_id = entry.repository;
        let storage = state.storage.clone();
        match tokio::task::spawn_blocking(move || {
            let _guard = guard;
            storage.remove_trash_entry(&entry)
        })
        .await
        .map_err(|e| anyhow::anyhow!("trash purge task panicked: {e}"))?
        {
            Ok(()) => purged += 1,
            Err(githearth_git::StorageError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // A restore moved it after the listing; nothing left to purge.
            }
            Err(e) => {
                summary.errors += 1;
                tracing::warn!(repository_id = %repository_id, error = %e, "trash purge failed");
            }
        }
    }
    summary.trash_purged = purged;

    // ---- push journal replay ----------------------------------------------
    push_replay(state).await?;

    // ---- push credential retention -----------------------------------------
    let mut conn = state.pool.acquire().await?;
    let purged = pushes::sweep_credentials(&mut conn).await?;
    drop(conn);
    summary.push_credentials_purged = purged as usize;
    if purged > 0 {
        tracing::info!(purged, "push credential retention swept rows");
    }

    // ---- abandoned object uploads ------------------------------------------
    // A multipart upload's deadline comes from
    // `object_upload_deadline_seconds`; the pass is the durable sweep, so a
    // restart finishes the cleanup rather than waiting for a new request.
    let objects = state.objects.clone();
    let cleaned = tokio::task::spawn_blocking(move || {
        objects.cleanup_abandoned(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("upload cleanup task panicked: {error}"))??;
    summary.uploads_cleaned = cleaned.len();
    if !cleaned.is_empty() {
        tracing::info!(
            cleaned = cleaned.len(),
            "reconciler removed abandoned object uploads"
        );
    }
    // The quota counter is a cache; one scan per pass repairs any drift a
    // crash left behind. Writes read the counter, never the whole tenant.
    let objects = state.objects.clone();
    tokio::task::spawn_blocking(move || objects.reconcile_usage())
        .await
        .map_err(|error| anyhow::anyhow!("usage reconciliation task panicked: {error}"))??;

    Ok(summary)
}

/// Applies journaled pushes that Git committed but metadata never recorded
/// (ADR 0003 "Recovery").  Runs after the lifecycle work so a repository
/// activated this pass is replayed too.
///
/// `// shortcut:` opens every active repository's journal each pass instead of
/// tracking which repositories gained records; rotation keeps each journal
/// bounded, and the replay itself is a no-op when nothing is pending.
async fn push_replay(state: &AppState) -> anyhow::Result<()> {
    let mut conn = state.pool.acquire().await?;
    let active: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM repositories WHERE status = 'active'")
            .fetch_all(&mut *conn)
            .await?;
    drop(conn);

    let mut committed = 0usize;
    let mut aborted = 0usize;
    for id in active {
        // Lock BEFORE any pool connection (module-level lock ordering).
        let guard = state.storage.lock_repository(id).await?;

        let mut conn = state.pool.acquire().await?;
        let applied = pushes::applied_through(&mut conn, id).await?;
        drop(conn);

        let storage = state.storage.clone();
        let pending = tokio::task::spawn_blocking(move || {
            let path = storage.repository_path(id);
            match githearth_git::journal::Journal::open(&path) {
                Ok(journal) => journal.records_after(applied).next().is_some(),
                Err(e) => {
                    tracing::warn!(repository_id = %id, error = %e, "cannot open journal; skipped");
                    false
                }
            }
        })
        .await
        .unwrap_or(false);
        if !pending {
            drop(guard);
            continue;
        }

        match pushes::replay(state, id).await {
            Ok(s) => {
                committed += s.committed;
                aborted += s.aborted;
            }
            Err(pushes::ReplayError::Diverged { sequence, .. }) => {
                tracing::error!(
                    repository_id = %id,
                    sequence,
                    "push journal replay diverged; repository needs repair"
                );
            }
            Err(e) => {
                tracing::warn!(repository_id = %id, error = %e, "push journal replay failed; will retry");
            }
        }
        drop(guard);
    }
    if committed > 0 || aborted > 0 {
        tracing::info!(committed, aborted, "push journal replay applied records");
    }
    Ok(())
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
         WHERE status IN ('creating','deleting','restoring') \
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
    Restored,
    Failed,
    Deleted,
}

async fn reconcile_creating(
    state: &AppState,
    repo_id: Uuid,
    op_id: Uuid,
    attempts: &mut HashMap<Uuid, u32>,
) -> anyhow::Result<Option<Change>> {
    let storage = state.storage.clone();

    // Acquire the per-repository filesystem lock *before* any DB connection
    // (see module-level lock-ordering docs).
    let guard = storage.lock_repository(repo_id).await?;

    // Re-read the row under the lock: a concurrent API delete may have already
    // removed it, and the reconciler must not recreate the directory.
    let mut conn = state.pool.acquire().await?;
    let row = repositories::get(&mut conn, repo_id).await?;
    let Some(row) = row.filter(|r| r.status == RepositoryStatus::Creating) else {
        attempts.remove(&repo_id);
        return Ok(None);
    };
    let format = repositories::object_format(&row.object_format);

    let create_result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        storage.create(repo_id, op_id, format)
    })
    .await
    .map_err(|e| anyhow::anyhow!("create task panicked: {e}"))?;

    match create_result {
        Ok(_) => {
            attempts.remove(&repo_id);
            let ok = repositories::set_status(
                &mut conn,
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
                    &mut conn,
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
    repo_id: Uuid,
    scanned: &HashSet<Uuid>,
) -> anyhow::Result<Option<Change>> {
    let storage = state.storage.clone();

    // Acquire the per-repository filesystem lock *before* any DB connection
    // (see module-level lock-ordering docs).
    let guard = storage.lock_repository(repo_id).await?;

    let mut conn = state.pool.acquire().await?;

    // Re-read the row under the lock: a concurrent request may have already
    // changed the status, and the reconciler must not trash a directory that
    // is no longer deleting.
    let row = repositories::get(&mut conn, repo_id).await?;
    let Some(row) = row.filter(|r| r.status == RepositoryStatus::Deleting) else {
        return Ok(None);
    };

    if scanned.contains(&repo_id) {
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            storage.trash(repo_id)
        })
        .await
        .map_err(|e| anyhow::anyhow!("trash task panicked: {e}"))??;
    } else {
        drop(guard);
    }

    let mut tx = conn.begin().await?;
    // shortcut: the deleting actor is not stored, so revoked_by names the creator; store the actor on the row if audits need it.
    let ok = repositories::finish_delete(&mut tx, repo_id, row.created_by).await?;
    tx.commit().await?;
    if ok {
        tracing::info!(repository_id = %repo_id, "reconciler deleted repository");
        Ok(Some(Change::Deleted))
    } else {
        Ok(None)
    }
}

/// Finish a restore that committed `restoring` but crashed before the row was
/// `active`. The directory may already be back (the move completed) or still
/// be in trash; both converge. If the purge removed the entry, the row returns
/// to `deleted`, which is the same refusal a restore after the purge gives.
async fn reconcile_restoring(state: &AppState, repo_id: Uuid) -> anyhow::Result<Option<Change>> {
    let storage = state.storage.clone();

    // Lock before any pool connection (module-level lock ordering).
    let guard = storage.lock_repository(repo_id).await?;

    let mut conn = state.pool.acquire().await?;
    let Some(row) = repositories::get(&mut conn, repo_id)
        .await?
        .filter(|row| row.status == RepositoryStatus::Restoring)
    else {
        return Ok(None);
    };
    let actor = row.created_by;
    drop(conn);

    let restore = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        if storage.has_repository(repo_id) {
            return Ok(());
        }
        match storage.find_trash(repo_id)? {
            Some(entry) => storage.restore(repo_id, &entry.path).map(|_| ()),
            None => Err(githearth_git::StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "trash entry is gone",
            ))),
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("restore task panicked: {e}"))?;

    match restore {
        Ok(()) => {
            let mut tx = state.pool.begin().await?;
            if !repositories::finish_restore(&mut tx, repo_id).await? {
                return Ok(None);
            }
            // shortcut: the restoring actor is not stored on the row; a
            // recovered restore is audited as its creator, like the deleting
            // reconciler does.
            crate::event(
                &mut tx,
                Some(row.organization_id),
                actor,
                "repository.restored",
            )
            .await?;
            tx.commit().await?;
            tracing::info!(repository_id = %repo_id, "reconciler restored repository");
            Ok(Some(Change::Restored))
        }
        Err(githearth_git::StorageError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut tx = state.pool.begin().await?;
            // Preserve the original `deleted_at`, like a failed move does.
            repositories::revert_restore(&mut tx, repo_id).await?;
            tx.commit().await?;
            tracing::warn!(repository_id = %repo_id, "restore had no trash entry; reverted to deleted");
            Ok(Some(Change::Deleted))
        }
        Err(e) => Err(anyhow::anyhow!("restore failed: {e}")),
    }
}

/// Outcome of [`recover_restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The directory is back and the row is `active`.
    Finished,
    /// The trash entry was gone; the row is `deleted` again.
    Reverted,
    /// The row had changed under the lock; nothing was done.
    Skipped,
}

/// Finish (or revert) one crashed restore now, without a full pass.
///
/// This is exactly the per-repository step [`reconcile_once`] runs for a
/// `restoring` row. It is public so the crash-recovery test can drive one
/// repository at a time: a suite that shares one database cannot backdate a
/// row for its own pass without every other suite's pass also seeing it and
/// acting on a different storage root.
pub async fn recover_restore(state: &AppState, repo_id: Uuid) -> anyhow::Result<RestoreOutcome> {
    Ok(match reconcile_restoring(state, repo_id).await? {
        Some(Change::Restored) => RestoreOutcome::Finished,
        Some(Change::Deleted) => RestoreOutcome::Reverted,
        _ => RestoreOutcome::Skipped,
    })
}

async fn orphan_repo(state: &AppState, id: Uuid) -> anyhow::Result<bool> {
    let storage = state.storage.clone();
    let guard = storage.lock_repository(id).await?;

    let mut conn = state.pool.acquire().await?;

    // Re-read the row under the lock: the orphan decision was made from a
    // status snapshot taken before the lock was acquired.  Only orphan if
    // the row is still missing or `Deleted`; otherwise a concurrent request
    // may have changed the status.
    let should_orphan = match repositories::get(&mut conn, id).await? {
        None => true,
        Some(r) if r.status == RepositoryStatus::Deleted => true,
        _ => false,
    };
    if !should_orphan {
        return Ok(false);
    }

    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        storage.orphan(id)
    })
    .await
    .map_err(|e| anyhow::anyhow!("orphan task panicked: {e}"))??;
    tracing::warn!(repository_id = %id, "orphaned on-disk repository with no corresponding metadata row");
    Ok(true)
}

/// Spawn the reconciler background task.  Runs one pass at startup, then
/// every `reconcile_interval_seconds`.  Never panics or exits the loop on
/// errors.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    let keys = state.installation_keys.clone();
    tokio::spawn(crate::secrets::scope(keys, async move {
        let interval = Duration::from_secs(state.config.reconcile_interval_seconds);
        let mut attempts: HashMap<Uuid, u32> = HashMap::new();

        run_pass(&state, &mut attempts).await;

        let mut tick = tokio::time::interval(interval);
        tick.tick().await;

        loop {
            tick.tick().await;
            run_pass(&state, &mut attempts).await;
        }
    }))
}

async fn run_pass(state: &AppState, attempts: &mut HashMap<Uuid, u32>) {
    // A pass creates, trashes, purges and orphans directories and replays
    // journals. The fence must therefore quiesce it, and `in_flight` must
    // count it while it runs. The whole pass holds one permit so no storage
    // operation of this pass escapes the drain.
    let Some(_write) = state.maintenance.admit_write() else {
        tracing::debug!("reconciler pass skipped; maintenance fence is up");
        return;
    };
    match reconcile_once(state, attempts).await {
        Ok(summary) => {
            if summary.has_changes() {
                tracing::info!(
                    activated = summary.activated,
                    restored = summary.restored,
                    failed = summary.failed,
                    deleted = summary.deleted,
                    orphaned = summary.orphaned,
                    tmp_cleaned = summary.tmp_cleaned,
                    trash_purged = summary.trash_purged,
                    push_credentials_purged = summary.push_credentials_purged,
                    uploads_cleaned = summary.uploads_cleaned,
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
