//! Fork creation (M3 "fork contributions", ADR 0008).
//!
//! A user with read access on a parent repository can fork it into a namespace
//! they can create repositories in. The fork stores the parent id, copies Git
//! objects and branch/tag refs through the existing storage layer, and inherits
//! the parent's visibility (a private parent always yields a private fork).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, event, facts, has_nul, idempotency, invalid, repositories,
    repository_api::{self, RepositoryJson},
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/organizations/{org}/repositories/{repo}/forks",
        axum::routing::post(create).get(list),
    )
}

#[derive(Serialize)]
pub(crate) struct ForkListing {
    /// The repository this fork came from, when it is a fork and the caller may
    /// read the parent.
    pub(crate) parent: Option<RepositoryJson>,
    /// Forks of this repository the caller may read.
    pub(crate) children: Vec<RepositoryJson>,
    pub(crate) count: usize,
}

/// `GET .../forks`: the parent link and the readable child forks (issue #156).
async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
) -> Result<Json<ForkListing>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    repository_api::require(&state, &loaded, Action::RepositoryRead)?;
    crate::issues::require_readable_repo(&loaded)?;

    let can_read = |facts: &facts::Facts, row: &repositories::RepositoryRow| {
        let authz_repo = repositories::authz_repository(row);
        state
            .authz
            .authorize(&facts.request(
                Action::RepositoryRead,
                Resource::Repository(&authz_repo),
                Some(Duration::from_secs(auth_age)),
            ))
            .is_allowed()
    };

    let mut conn = state.pool.acquire().await?;
    let parent = match loaded.repo.parent_repository_id {
        Some(parent_id) => {
            let row = repositories::get(&mut conn, parent_id)
                .await?
                .filter(|row| row.status != RepositoryStatus::Deleted);
            match row {
                Some(row) => {
                    let parent_facts = if row.organization_id == loaded.org_id {
                        Some(loaded.facts.clone())
                    } else {
                        facts::load(&mut conn, actor.id, row.organization_id).await?
                    };
                    parent_facts
                        .filter(|f| can_read(f, &row))
                        .map(|_| RepositoryJson::from(&row))
                }
                None => None,
            }
        }
        None => None,
    };

    let mut children = Vec::new();
    for row in repositories::list_children(&mut conn, loaded.repo.id).await? {
        if row.organization_id == loaded.org_id {
            if can_read(&loaded.facts, &row) {
                children.push(RepositoryJson::from(&row));
            }
            continue;
        }
        let Some(facts) = facts::load(&mut conn, actor.id, row.organization_id).await? else {
            continue;
        };
        if can_read(&facts, &row) {
            children.push(RepositoryJson::from(&row));
        }
    }
    let count = children.len();
    Ok(Json(ForkListing {
        parent,
        children,
        count,
    }))
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn org_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn name_taken() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository name already taken")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewFork {
    /// The organization slug to fork into (a namespace the actor can write to).
    organization: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<RepositoryJson>), ApiError> {
    serde_json::from_slice::<NewFork>(&body).map_err(|_| invalid())?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (_status, value) = create_fork(
        &state, actor.id, auth_age, &org_slug, &repo_name, &body, key,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::from_value(value).unwrap()),
    ))
}

/// Forks `repo_name` under `org_slug` as `actor_id`; shared by the JSON handler
/// and the browser form. The body is a `NewFork` JSON object. Validation and
/// authorization happen once here.
pub(crate) async fn create_fork(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let input: NewFork = serde_json::from_slice(body).map_err(|_| invalid())?;
    let fork_name = input.name.clone().unwrap_or_else(|| repo_name.to_owned());
    if !repository_api::valid_repo_name(&fork_name)
        || input
            .description
            .as_deref()
            .is_some_and(|d| d.chars().count() > 1000 || has_nul(d))
        || input.organization.trim().is_empty()
        || has_nul(&input.organization)
    {
        return Err(invalid());
    }

    // Authorize: read on the parent, create on the target namespace.
    let parent =
        repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name).await?;
    repository_api::require(state, &parent, Action::RepositoryRead)?;
    if parent.repo.status != RepositoryStatus::Active {
        return Err(not_active());
    }

    let mut conn = state.pool.acquire().await?;
    let target_org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&input.organization)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;
    let Some(target_facts) = facts::load(&mut conn, actor_id, target_org_id).await? else {
        return Err(org_not_found());
    };
    if !state
        .authz
        .authorize(&target_facts.request(
            Action::RepositoryCreate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed()
    {
        // An organization member who lacks the create grant gets 403; only a
        // stranger gets 404, matching repository creation (issue #133).
        if target_facts.account.organizations.contains(&target_org_id) {
            return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
        }
        return Err(org_not_found());
    }
    drop(conn);

    // A private parent always yields a private fork; otherwise the fork
    // inherits the parent's visibility.
    let fork_visibility = match parent.repo.visibility.as_str() {
        "private" => "private".to_owned(),
        other => other.to_owned(),
    };

    let state = &state;
    let body = &body;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/forks");
    let description = input.description.unwrap_or_default();

    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            let fork = match idem.prior_operation_id {
                Some(operation_id) => {
                    let repo = repositories::find_by_operation(&mut tx, operation_id)
                        .await?
                        .ok_or_else(not_found)?;
                    // A previously deleted repository must never be resurrected.
                    if matches!(
                        repo.status,
                        RepositoryStatus::Deleted | RepositoryStatus::Deleting
                    ) {
                        return Err(not_found());
                    }
                    // Refresh updated_at before storage so the reconciler (which
                    // visits only rows older than its grace) does not redo this
                    // fork copy against the same tmp/<operation> while we build it.
                    if !repositories::touch(&mut tx, repo.id).await? {
                        return Err(not_found());
                    }
                    repo
                }
                None => {
                    let operation_id = Uuid::new_v4();
                    if !idem.record_operation(&mut tx, operation_id).await? {
                        return Err(idempotency::claim_lost());
                    }
                    match repositories::insert_creating_with_parent(
                        &mut tx,
                        target_org_id,
                        &fork_name,
                        actor_id,
                        operation_id,
                        &fork_visibility,
                        &description,
                        &parent.repo.object_format,
                        Some(parent.repo.id),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            if e.as_database_error()
                                .is_some_and(|d| d.code().as_deref() == Some("23505"))
                            {
                                return Err(name_taken());
                            }
                            return Err(e.into());
                        }
                    }
                }
            };
            if idem.prior_operation_id.is_none() {
                event(&mut tx, Some(target_org_id), actor_id, "repository.forked").await?;
            }
            let operation_id = fork.operation_id;
            let fork_id = fork.id;
            let parent_id = parent.repo.id;
            tx.commit().await?;

            // Storage: create the fork's bare repository, then copy the parent's
            // refs and objects in (ADR 0008 chooses a full copy). Both locks are
            // taken before any pool connection (lock ordering).
            let storage = state.storage.clone();
            let fork_guard = storage.repository_lock(fork_id).lock_owned().await;
            let parent_guard = storage.repository_lock(parent_id).lock_owned().await;
            let current_status = {
                let mut conn = state.pool.acquire().await?;
                let row = repositories::get(&mut conn, fork_id)
                    .await?
                    .ok_or_else(not_found)?;
                drop(conn);
                row.status
            };
            match current_status {
                RepositoryStatus::Creating => {
                    tokio::task::spawn_blocking(move || {
                        let _fork = fork_guard;
                        let _parent = parent_guard;
                        let parent_path = storage.read_path(parent_id)?;
                        let format = githearth_git::layout::detect_object_format(&parent_path)?;
                        storage.create(fork_id, operation_id, format)?;
                        storage.fetch_refs(fork_id, parent_id)
                    })
                    .await
                    .map_err(|_| {
                        ApiError(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "storage operation failed",
                        )
                    })?
                    .map_err(|_| {
                        ApiError(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "repository storage unavailable",
                        )
                    })?;
                }
                RepositoryStatus::Active => {
                    drop(fork_guard);
                    drop(parent_guard);
                }
                _ => {
                    drop(fork_guard);
                    drop(parent_guard);
                    return Err(not_active());
                }
            }

            let mut tx = state.pool.begin().await?;
            repositories::set_status(
                &mut tx,
                fork_id,
                RepositoryStatus::Creating,
                RepositoryStatus::Active,
            )
            .await?;
            let row = repositories::get(&mut tx, fork_id)
                .await?
                .ok_or_else(not_found)?;
            let result = RepositoryJson::from(&row);
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&result).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await
}
