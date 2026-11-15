//! Restore a deleted repository from trash (M4.1 U23c, ADR 0003 revision).
//!
//! One operation is shared by the JSON API, the browser page and the CLI. It
//! asks the policy engine for one narrow action, `repository.restore`, which
//! only an organization owner (the `owner` group) or a site administrator can
//! hold. A repository-scoped `admin` grant is deliberately not enough, because
//! U23c restricts restore to an owner unlike delete.
//!
//! The row keeps its id, grants and history: restore touches only `status` and
//! `deleted_at`. A grant the delete revoked stays revoked; the restore does not
//! recreate it.
//!
//! Crash safety: the row enters `restoring` before the directory moves, and the
//! reconciler and the purge take the same per-repository filesystem lock. The
//! lock has an in-process half and an inter-process `flock`
//! ([`githearth_git::layout::Storage::lock_repository`]), because the CLI is a
//! second process that performs the same move: within one storage root there
//! is exactly one mover at a time. A purge that wins after the trash entry is
//! gone reverts the row to `deleted`; a purge that arrives while the row is
//! `restoring` skips the entry, so a mid-move restore is never destroyed.

use std::time::Duration;

use uuid::Uuid;

use githearth_authz::Action;
use githearth_core::RepositoryStatus;
use githearth_git::StorageError;

use crate::{
    ApiError, AppState, facts, idempotency::Idem, repositories, repositories::RepositoryRow,
    repository_api, valid_path_param,
};

fn not_found() -> ApiError {
    ApiError(axum::http::StatusCode::NOT_FOUND, "not found")
}
fn not_deleted() -> ApiError {
    ApiError(
        axum::http::StatusCode::CONFLICT,
        "repository is not deleted",
    )
}
fn name_taken() -> ApiError {
    ApiError(
        axum::http::StatusCode::CONFLICT,
        "repository name already taken",
    )
}
fn trash_gone() -> ApiError {
    ApiError(
        axum::http::StatusCode::CONFLICT,
        "repository is no longer in trash",
    )
}
fn storage_unavailable() -> ApiError {
    ApiError(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "repository storage unavailable",
    )
}

/// Whether `facts` may restore this repository. UI pages use it to decide
/// whether to offer the form; the operation asks the engine again.
pub(crate) fn may_restore(
    state: &AppState,
    facts: &facts::Facts,
    row: &RepositoryRow,
    auth_age: u64,
) -> bool {
    state
        .authz
        .authorize(&facts.request(
            Action::RepositoryRestore,
            githearth_authz::Resource::Repository(&repositories::authz_repository(row)),
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed()
}

/// Restore `name` in `org_slug` as `actor_id`.
///
/// `prior_operation` is the operation id an idempotent retry recorded, so a
/// crash after the row became `active` still resolves the exact row instead of
/// a newer repository that reused the name. `idem` records the operation id in
/// the same transaction that enters `restoring`.
pub async fn restore_repository(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    name: &str,
    prior_operation: Option<Uuid>,
    idem: Option<&Idem>,
) -> Result<RepositoryRow, ApiError> {
    if !valid_path_param(org_slug) || !valid_path_param(name) {
        return Err(crate::invalid());
    }
    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(not_found)?;
    let Some(facts) = facts::load(&mut conn, actor_id, org_id).await? else {
        return Err(not_found());
    };

    let mut repo = match prior_operation {
        Some(operation_id) => repositories::find_by_operation(&mut conn, operation_id)
            .await?
            .filter(|row| row.organization_id == org_id && row.name.eq_ignore_ascii_case(name))
            .ok_or_else(not_found)?,
        None => match repositories::find_restorable(&mut conn, org_id, name).await? {
            Some(row) => row,
            None => {
                // A repeated restore after success reaches the live row: say
                // "not deleted", while an unknown name stays a 404.
                if repositories::find_by_name(&mut conn, org_id, name)
                    .await?
                    .is_some()
                {
                    return Err(not_deleted());
                }
                return Err(not_found());
            }
        },
    };
    drop(conn);

    let loaded = repository_api::Loaded {
        actor_id,
        auth_age,
        org_id,
        facts,
        repo: repo.clone(),
    };
    repository_api::require(state, &loaded, Action::RepositoryRestore)?;

    // A retry of an already completed restore is success, not "not deleted":
    // the operation id is the proof that this is the same request.
    if repo.status == RepositoryStatus::Active && prior_operation.is_some() {
        return Ok(repo);
    }
    if !matches!(
        repo.status,
        RepositoryStatus::Deleted | RepositoryStatus::Restoring
    ) {
        return Err(not_deleted());
    }

    if repo.status == RepositoryStatus::Deleted {
        // A live repository that reused the name makes the restore a name
        // collision: refuse before touching the row, exactly as U23c asks.
        let taken: bool = {
            let mut conn = state.pool.acquire().await?;
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM repositories \
                 WHERE organization_id=$1 AND lower(name)=lower($2) AND id<>$3 AND status <> 'deleted')",
            )
            .bind(org_id)
            .bind(&repo.name)
            .bind(repo.id)
            .fetch_one(&mut *conn)
            .await?
        };
        if taken {
            return Err(name_taken());
        }

        let mut tx = state.pool.begin().await?;
        // A transfer tombstone holds this path for its moved repository; a
        // restore must not take the name back under it. (Before the transfer
        // slice's `transferred_at` exists, the tombstone is a redirect whose
        // target lives in another organization.)
        if repositories::transfer_tombstone_holds_name(&mut tx, org_id, &repo.name).await? {
            tx.rollback().await?;
            return Err(name_taken());
        }
        match repositories::set_status(
            &mut tx,
            repo.id,
            RepositoryStatus::Deleted,
            RepositoryStatus::Restoring,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                // Another request changed the row first. Resume a restore it
                // started; anything else is no longer restorable.
                let Some(current) = repositories::get(&mut tx, repo.id).await? else {
                    return Err(not_found());
                };
                if current.status == RepositoryStatus::Active && prior_operation.is_some() {
                    tx.rollback().await?;
                    return Ok(current);
                }
                if current.status != RepositoryStatus::Restoring {
                    return Err(not_deleted());
                }
                repo = current;
            }
            Err(e)
                if e.as_database_error()
                    .is_some_and(|d| d.code().as_deref() == Some("23505")) =>
            {
                return Err(name_taken());
            }
            Err(e) => return Err(e.into()),
        }
        // A rename redirect still claiming this name would resolve the
        // restored path to the renamed repository. Retire it in the same
        // transaction that puts the row into `restoring`; a transfer
        // tombstone is left for the transfer slice.
        repositories::retire_redirect(&mut tx, org_id, &repo.name).await?;
        if let Some(idem) = idem
            && !idem.record_operation(&mut tx, repo.operation_id).await?
        {
            return Err(crate::idempotency::claim_lost());
        }
        tx.commit().await?;
        repo.status = RepositoryStatus::Restoring;
    }

    // The lock serializes the move with the reconciler's purge — in this
    // process and, because the CLI is a separate process, across processes.
    // It is acquired before the next DB connection, per the reconciler's lock
    // ordering rule.
    let storage = state.storage.clone();
    let repo_id = repo.id;
    let guard = storage
        .lock_repository(repo_id)
        .await
        .map_err(|_| storage_unavailable())?;

    let mut conn = state.pool.acquire().await?;
    let Some(current) = repositories::get(&mut conn, repo_id).await? else {
        return Err(not_found());
    };
    if current.status == RepositoryStatus::Active {
        return Ok(current);
    }
    if current.status != RepositoryStatus::Restoring {
        return Err(not_deleted());
    }
    drop(conn);

    let moved = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        match storage.find_trash(repo_id)? {
            Some(entry) => storage.restore(repo_id, &entry.path),
            // A retry after a crash that moved the directory but not the row.
            None if storage.has_repository(repo_id) => storage.read_path(repo_id),
            None => Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "trash entry is gone",
            ))),
        }
    })
    .await
    .map_err(|_| storage_unavailable())?;

    match moved {
        Ok(_) => {
            let mut tx = state.pool.begin().await?;
            if !repositories::finish_restore(&mut tx, repo_id).await? {
                tx.rollback().await?;
                let mut conn = state.pool.acquire().await?;
                let Some(row) = repositories::get(&mut conn, repo_id).await? else {
                    return Err(not_found());
                };
                return if row.status == RepositoryStatus::Active {
                    Ok(row)
                } else {
                    Err(not_deleted())
                };
            }
            crate::event(&mut tx, Some(org_id), actor_id, "repository.restored").await?;
            let row = repositories::get(&mut tx, repo_id)
                .await?
                .ok_or_else(not_found)?;
            tx.commit().await?;
            Ok(row)
        }
        Err(StorageError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            // The purge removed the trash entry before the move started. Put
            // the row back where it was so it is not left claiming `restoring`.
            revert_to_deleted(state, repo_id).await?;
            Err(trash_gone())
        }
        Err(_) => {
            revert_to_deleted(state, repo_id).await?;
            Err(storage_unavailable())
        }
    }
}

async fn revert_to_deleted(state: &AppState, repo_id: Uuid) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    // Preserves the original `deleted_at`: a failed attempt must not restart
    // the retention clock.
    repositories::revert_restore(&mut tx, repo_id).await?;
    tx.commit().await?;
    Ok(())
}

/// CLI entry point: restore as the named account.
///
/// The command carries no session age, so the explicit host invocation is
/// treated as fresh: it bypasses the `sensitive` fresh-auth window that the
/// browser and the API enforce. That is a host-tool choice, not a new
/// authority — the account must still be an owner or a site administrator,
/// and every refusal is the same one the API gives. A host operator with
/// database access can do more than this command anyway.
pub async fn restore_by_username(
    state: &AppState,
    org_slug: &str,
    name: &str,
    username: &str,
) -> Result<RepositoryRow, ApiError> {
    let mut conn = state.pool.acquire().await?;
    // Match the session gate: a suspended account, or one that must change its
    // password after an administrator reset, cannot act through the API either.
    let actor: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE username=$1 AND NOT suspended AND NOT must_change_password",
    )
    .bind(username)
    .fetch_optional(&mut *conn)
    .await?;
    let actor = actor.ok_or_else(not_found)?;
    drop(conn);
    restore_repository(state, actor, 0, org_slug, name, None, None).await
}

/// The repository row named by a deleted or restoring path, for the browser's
/// `410` page. The caller still asks [`may_restore`] before showing a form and
/// is responsible for knowing that this viewer may see the path at all: see
/// [`anonymous_gone`] for the anonymous rule.
pub(crate) async fn deleted_row(
    state: &AppState,
    org_slug: &str,
    name: &str,
) -> Option<(Uuid, RepositoryRow)> {
    if !valid_path_param(org_slug) || !valid_path_param(name) {
        return None;
    }
    let mut conn = state.pool.acquire().await.ok()?;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await
        .ok()??;
    repositories::find_restorable(&mut conn, org_id, name)
        .await
        .ok()
        .flatten()
        .map(|row| (org_id, row))
}

/// Whether an anonymous visitor should see the `410` page for this path.
///
/// Anonymous visitors may learn that a path is gone only when the deleted row
/// is **public**: `410` for a private deleted name would be an existence
/// oracle that the live private path deliberately hides. A live replacement
/// that reused the name is not returned here either — it may be private, in
/// which case the visitor gets the ordinary sign-in challenge, not the old
/// row's `410` (live lookup wins).
pub(crate) async fn anonymous_gone(state: &AppState, org_slug: &str, name: &str) -> bool {
    if !valid_path_param(org_slug) || !valid_path_param(name) {
        return false;
    }
    let Ok(mut conn) = state.pool.acquire().await else {
        return false;
    };
    let Ok(Some(org_id)): Result<Option<Uuid>, _> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
            .bind(org_slug)
            .fetch_optional(&mut *conn)
            .await
    else {
        return false;
    };
    matches!(
        repositories::find_exact_any_status(&mut conn, org_id, name).await,
        Ok(Some(row))
            if row.visibility == "public"
                && matches!(
                    row.status,
                    RepositoryStatus::Deleted | RepositoryStatus::Restoring
                )
    )
}
