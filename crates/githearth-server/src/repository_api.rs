use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Page, authenticate, event, facts, idempotency, invalid, repositories,
};

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn org_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "organization not found")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
fn name_taken() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository name already taken")
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewRepository {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    visibility: Option<String>,
}

fn valid_repo_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        && name != "."
        && name != ".."
        && !name.ends_with(".git")
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RepositoryJson {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    description: String,
    visibility: String,
    archived: bool,
    status: String,
}

impl From<&repositories::RepositoryRow> for RepositoryJson {
    fn from(row: &repositories::RepositoryRow) -> Self {
        Self {
            id: row.id,
            organization_id: row.organization_id,
            name: row.name.clone(),
            description: row.description.clone(),
            visibility: row.visibility.clone(),
            archived: row.archived,
            status: row.status.as_str().to_owned(),
        }
    }
}

struct Loaded {
    actor_id: Uuid,
    auth_age: u64,
    org_id: Uuid,
    facts: crate::facts::Facts,
    repo: repositories::RepositoryRow,
}

async fn load_repo(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    name: &str,
) -> Result<Loaded, ApiError> {
    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;

    let Some(facts) = facts::load(&mut conn, actor_id, org_id).await? else {
        return Err(org_not_found());
    };

    let Some(repo) = repositories::find_by_name(&mut conn, org_id, name).await? else {
        return Err(org_not_found());
    };

    Ok(Loaded {
        actor_id,
        auth_age,
        org_id,
        facts,
        repo,
    })
}

fn require(state: &AppState, loaded: &Loaded, action: Action) -> Result<(), ApiError> {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let decision = state.authz.authorize(&loaded.facts.request(
        action,
        Resource::Repository(&authz_repo),
        Some(Duration::from_secs(loaded.auth_age)),
    ));
    if decision.is_allowed() {
        return Ok(());
    }
    let read_ok = state
        .authz
        .authorize(&loaded.facts.request(
            Action::RepositoryRead,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed();
    if read_ok {
        Err(ApiError(StatusCode::FORBIDDEN, "not permitted"))
    } else {
        Err(not_found())
    }
}

pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<RepositoryJson>), ApiError> {
    let input: NewRepository = serde_json::from_slice(&body).map_err(|_| invalid())?;

    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    if !valid_repo_name(&input.name) || input.description.len() > 1000 {
        return Err(invalid());
    }
    if let Some(ref v) = input.visibility
        && !matches!(v.as_str(), "private" | "internal" | "public")
    {
        return Err(invalid());
    }

    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;

    let Some(facts) = facts::load(&mut conn, actor.id, org_id).await? else {
        return Err(org_not_found());
    };
    if !state
        .authz
        .authorize(&facts.request(
            Action::RepositoryCreate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed()
    {
        return Err(org_not_found());
    }

    let key = idempotency::extract_key(&headers).transpose()?;
    idempotency::run(
        &mut conn,
        actor.id,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        &body,
        key,
        || async {
            let operation_id = Uuid::new_v4();
            let visibility = input.visibility.as_deref().unwrap_or("private");
            let description = &input.description;

            let mut tx = state.pool.begin().await?;
            let repo = match repositories::insert_creating(
                &mut tx,
                org_id,
                &input.name,
                actor.id,
                operation_id,
                visibility,
                description,
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
            };
            event(&mut tx, Some(org_id), actor.id, "repository.created").await?;
            tx.commit().await?;

            let storage = state.storage.clone();
            let create_result = tokio::task::spawn_blocking(move || {
                storage.create(
                    repo.id,
                    operation_id,
                    githearth_git::layout::ObjectFormat::Sha1,
                )
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage operation failed",
                )
            })?;

            let mut conn2 = state.pool.acquire().await?;
            match create_result {
                Ok(_) => {
                    repositories::set_status(
                        &mut conn2,
                        repo.id,
                        RepositoryStatus::Creating,
                        RepositoryStatus::Active,
                    )
                    .await?;
                    let row = repositories::get(&mut conn2, repo.id)
                        .await?
                        .ok_or(ApiError(StatusCode::NOT_FOUND, "repository not found"))?;
                    let result = RepositoryJson::from(&row);
                    Ok((StatusCode::CREATED, serde_json::to_value(&result).unwrap()))
                }
                Err(_) => {
                    repositories::set_status(
                        &mut conn2,
                        repo.id,
                        RepositoryStatus::Creating,
                        RepositoryStatus::Failed,
                    )
                    .await?;
                    Err(ApiError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "repository storage unavailable",
                    ))
                }
            }
        },
    )
    .await
    .map(|(_, value)| {
        (
            StatusCode::CREATED,
            Json(serde_json::from_value(value).unwrap()),
        )
    })
}

pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<RepositoryJson>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }

    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;

    let Some(facts) = facts::load(&mut conn, actor.id, org_id).await? else {
        return Err(org_not_found());
    };
    if !state
        .authz
        .authorize(&facts.request(
            Action::OrganizationRead,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed()
    {
        return Err(org_not_found());
    }

    let rows = repositories::list(&mut conn, org_id, page.after, limit).await?;
    let mut result = Vec::new();
    for row in &rows {
        let authz_repo = repositories::authz_repository(row);
        let decision = state.authz.authorize(&facts.request(
            Action::RepositoryRead,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(auth_age)),
        ));
        if decision.is_allowed() {
            result.push(RepositoryJson::from(row));
        }
    }
    Ok(Json(result))
}

pub(crate) async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
) -> Result<Json<RepositoryJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = load_repo(&state, actor.id, auth_age, &org_slug, &name).await?;
    require(&state, &loaded, Action::RepositoryRead)?;
    Ok(Json(RepositoryJson::from(&loaded.repo)))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateRepository {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<RepositoryJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    let key = idempotency::extract_key(&headers).transpose()?;
    let mut conn = state.pool.acquire().await?;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}");

    idempotency::run(
        &mut conn,
        actor.id,
        "PATCH",
        &path_str,
        &body,
        key,
        || async {
            let input: UpdateRepository = serde_json::from_slice(&body).map_err(|_| invalid())?;

            if let Some(ref new_name) = input.name
                && !valid_repo_name(new_name)
            {
                return Err(invalid());
            }
            if let Some(ref desc) = input.description
                && desc.len() > 1000
            {
                return Err(invalid());
            }

            let loaded = load_repo(&state, actor.id, auth_age, &org_slug, &name).await?;
            require(&state, &loaded, Action::RepositoryUpdate)?;

            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if let Some(ref new_name) = input.name {
                match repositories::rename(&mut tx, loaded.repo.id, new_name).await {
                    Err(e)
                        if e.as_database_error()
                            .is_some_and(|d| d.code().as_deref() == Some("23505")) =>
                    {
                        return Err(name_taken());
                    }
                    Err(e) => return Err(e.into()),
                    Ok(_) => {}
                }
            }
            if let Some(ref desc) = input.description {
                repositories::set_description(&mut tx, loaded.repo.id, desc).await?;
            }
            event(
                &mut tx,
                Some(loaded.org_id),
                loaded.actor_id,
                "repository.updated",
            )
            .await?;
            tx.commit().await?;

            let mut conn2 = state.pool.acquire().await?;
            let updated = repositories::get(&mut conn2, loaded.repo.id)
                .await?
                .unwrap();
            let result_json = RepositoryJson::from(&updated);
            Ok((StatusCode::OK, serde_json::to_value(&result_json).unwrap()))
        },
    )
    .await
    .map(|(_, value)| Json(serde_json::from_value(value).unwrap()))
}

macro_rules! archive_handler {
    ($name:ident, $archived:literal) => {
        pub(crate) async fn $name(
            State(state): State<AppState>,
            headers: HeaderMap,
            Path((org_slug, name)): Path<(String, String)>,
        ) -> Result<(StatusCode, Json<RepositoryJson>), ApiError> {
            let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

            let suffix = if $archived { "archive" } else { "unarchive" };
            let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}/{suffix}");
            let key = idempotency::extract_key(&headers).transpose()?;
            let mut conn = state.pool.acquire().await?;

            idempotency::run(&mut conn, actor.id, "POST", &path_str, b"", key, || async {
                let loaded = load_repo(&state, actor.id, auth_age, &org_slug, &name).await?;
                let action = if $archived {
                    Action::RepositoryArchive
                } else {
                    Action::RepositoryUnarchive
                };
                require(&state, &loaded, action)?;

                if loaded.repo.status != RepositoryStatus::Active {
                    return Err(not_active());
                }

                if loaded.repo.archived == $archived && $archived {
                    let result_json = RepositoryJson::from(&loaded.repo);
                    return Ok((StatusCode::OK, serde_json::to_value(&result_json).unwrap()));
                }

                let mut tx = state.pool.begin().await?;
                repositories::set_archived(&mut tx, loaded.repo.id, $archived).await?;
                let event_kind = if $archived {
                    "repository.archived"
                } else {
                    "repository.unarchived"
                };
                event(&mut tx, Some(loaded.org_id), loaded.actor_id, event_kind).await?;
                tx.commit().await?;

                let mut conn2 = state.pool.acquire().await?;
                let updated = repositories::get(&mut conn2, loaded.repo.id)
                    .await?
                    .unwrap();
                let result_json = RepositoryJson::from(&updated);
                Ok((StatusCode::OK, serde_json::to_value(&result_json).unwrap()))
            })
            .await
            .map(|(_, value)| (StatusCode::OK, Json(serde_json::from_value(value).unwrap())))
        }
    };
}

archive_handler!(archive, true);
archive_handler!(unarchive, false);

pub(crate) async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}");
    let key = idempotency::extract_key(&headers).transpose()?;
    let mut conn = state.pool.acquire().await?;

    let (status, _) = idempotency::run(
        &mut conn,
        actor.id,
        "DELETE",
        &path_str,
        b"",
        key,
        || async {
            let loaded = load_repo(&state, actor.id, auth_age, &org_slug, &name).await?;
            require(&state, &loaded, Action::RepositoryDelete)?;

            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::set_status(
                &mut tx,
                loaded.repo.id,
                RepositoryStatus::Active,
                RepositoryStatus::Deleting,
            )
            .await?
            {
                return Err(not_active());
            }
            event(
                &mut tx,
                Some(loaded.org_id),
                loaded.actor_id,
                "repository.deleted",
            )
            .await?;
            tx.commit().await?;

            let storage = state.storage.clone();
            let trash_result = tokio::task::spawn_blocking(move || storage.trash(loaded.repo.id))
                .await
                .map_err(|_| {
                    ApiError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "storage operation failed",
                    )
                })?;

            let mut conn2 = state.pool.acquire().await?;
            match trash_result {
                Ok(_) => {
                    repositories::set_status(
                        &mut conn2,
                        loaded.repo.id,
                        RepositoryStatus::Deleting,
                        RepositoryStatus::Deleted,
                    )
                    .await?;
                    Ok((StatusCode::NO_CONTENT, serde_json::Value::Null))
                }
                Err(_) => Err(ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "repository storage unavailable",
                )),
            }
        },
    )
    .await?;
    Ok(status)
}
