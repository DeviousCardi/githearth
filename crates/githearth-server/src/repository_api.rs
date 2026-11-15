use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Row};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Page, authenticate, event, facts, has_nul, idempotency, invalid,
    repositories, valid_path_param,
};

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn org_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
/// The address belonged to a repository that moved to another organization.
pub(crate) fn moved_away() -> ApiError {
    ApiError(StatusCode::GONE, "this repository has moved")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
fn name_taken() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository name already taken")
}
pub(crate) fn gone() -> ApiError {
    ApiError(StatusCode::GONE, "repository is gone")
}

fn storage_error() -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage operation failed",
    )
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewRepository {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    object_format: Option<String>,
}

pub(crate) fn valid_repo_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name != "new"
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
    parent_repository_id: Option<Uuid>,
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
            parent_repository_id: row.parent_repository_id,
        }
    }
}

pub(crate) struct Loaded {
    pub(crate) actor_id: Uuid,
    pub(crate) auth_age: u64,
    pub(crate) org_id: Uuid,
    pub(crate) facts: crate::facts::Facts,
    pub(crate) repo: repositories::RepositoryRow,
}

pub(crate) async fn load_repo(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    name: &str,
) -> Result<Loaded, ApiError> {
    if !valid_path_param(org_slug) || !valid_path_param(name) {
        return Err(invalid());
    }
    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;

    // ADR 0003: an address a repository moved away from is `410 Gone` for
    // every signed-in caller, before any fact about the repository is loaded, so
    // a later reuse of the name can never make it resolve again.
    if repositories::path_moved_away(&mut conn, org_id, name).await? {
        return Err(moved_away());
    }
    let Some(facts) = facts::load(&mut conn, actor_id, org_id).await? else {
        return Err(org_not_found());
    };

    let repo = match repositories::find_by_name(&mut conn, org_id, name).await? {
        Some(repo) => repo,
        None => {
            // A deleted or restoring row still owns this path: `410 Gone` until
            // the restore completes, never a redirect to another repository.
            match repositories::find_by_name_any_status(&mut conn, org_id, name).await? {
                Some(row) if is_gone(row.status) => row,
                _ => return Err(not_found()),
            }
        }
    };

    if is_gone(repo.status) {
        // `find_by_name` includes `restoring` (only `deleted` is excluded), so
        // both gone states are checked here. The path is 410 until the row is
        // active again — but only for a caller that could have seen the
        // repository anyway: one that may read it (public, or a grant) or may
        // restore it (owner, site administrator). Anyone else gets the shared
        // 404, so `410` is not an existence oracle for a deleted private name.
        let loaded = Loaded {
            actor_id,
            auth_age,
            org_id,
            facts,
            repo,
        };
        return if may_see_gone(state, &loaded) {
            Err(gone())
        } else {
            Err(not_found())
        };
    }

    Ok(Loaded {
        actor_id,
        auth_age,
        org_id,
        facts,
        repo,
    })
}

/// Whether a caller may learn that the path is gone rather than missing: it
/// may read the row, or it may restore it.
fn may_see_gone(state: &AppState, loaded: &Loaded) -> bool {
    permitted(state, loaded, Action::RepositoryRead)
        || permitted(state, loaded, Action::RepositoryRestore)
}

/// Whether the policy engine permits `action` for this row, with no
/// read-fallback rewriting of the result (unlike [`require`]).
pub(crate) fn permitted(state: &AppState, loaded: &Loaded, action: Action) -> bool {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    state
        .authz
        .authorize(&loaded.facts.request(
            action,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed()
}

fn is_gone(status: RepositoryStatus) -> bool {
    matches!(
        status,
        RepositoryStatus::Deleted | RepositoryStatus::Restoring
    )
}

/// Mutations must name the repository's current name; renamed-away names only redirect reads.
pub(crate) async fn load_current(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    name: &str,
) -> Result<Loaded, ApiError> {
    let loaded = load_repo(state, actor_id, auth_age, org_slug, name).await?;
    if !loaded.repo.name.eq_ignore_ascii_case(name) {
        return Err(ApiError(StatusCode::NOT_FOUND, "not found"));
    }
    Ok(loaded)
}

/// Resolve a repository by its immutable creation operation id, for resuming a
/// crashed mutation against the exact row it started on (never a newer repository
/// that has since reused the name).
async fn load_by_operation(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    operation_id: Uuid,
) -> Result<Loaded, ApiError> {
    let mut conn = state.pool.acquire().await?;
    let Some(repo) = repositories::find_by_operation(&mut conn, operation_id).await? else {
        return Err(ApiError(StatusCode::NOT_FOUND, "not found"));
    };
    let org_id = repo.organization_id;
    let Some(facts) = facts::load(&mut conn, actor_id, org_id).await? else {
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

/// Load a repository for an anonymous visitor. Only a public repository in a
/// non-suspended organization is visible; anything else yields `None` so the
/// caller can fall back to a sign-in prompt without revealing the repository.
///
/// The anonymous read decision is shared with the git fetch path: this calls
/// [`crate::git_admission::anonymous_readable_repository`], which additionally
/// requires the clone action before serving a fetch over HTTP or SSH.
pub(crate) async fn load_public_anonymous(
    state: &AppState,
    org_slug: &str,
    name: &str,
) -> Option<repositories::RepositoryRow> {
    crate::git_admission::anonymous_readable_repository(state, org_slug, name).await
}

pub(crate) fn require(state: &AppState, loaded: &Loaded, action: Action) -> Result<(), ApiError> {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let request = loaded.facts.request(
        action,
        Resource::Repository(&authz_repo),
        Some(Duration::from_secs(loaded.auth_age)),
    );
    let decision = state.authz.authorize(&request);
    if decision.is_allowed() {
        return Ok(());
    }
    // Keep the engine's reason: a `sensitive` action refused only because the
    // session is stale becomes the re-authentication envelope, so the API and
    // the pages can prompt for a password instead of reporting a plain denial.
    // The forbid fires for every stale caller, so the engine is also asked
    // whether a fresh password would actually allow it; a caller without the
    // grant is still denied and falls through to the ordinary refusal below.
    if decision.denied_for_fresh_auth() && state.authz.allows_if_fresh(&request) {
        return Err(crate::reauthentication_required());
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

    if !valid_path_param(&org_slug) {
        return Err(invalid());
    }
    if !valid_repo_name(&input.name)
        || input.description.chars().count() > 1000
        || has_nul(&input.description)
    {
        return Err(invalid());
    }
    if let Some(ref v) = input.visibility
        && !matches!(v.as_str(), "private" | "internal" | "public")
    {
        return Err(invalid());
    }
    if let Some(ref v) = input.object_format
        && !matches!(v.as_str(), "sha1" | "sha256")
    {
        return Err(invalid());
    }
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(org_not_found)?;

    let Some(facts) = facts::load(&mut conn, actor.id, org_id).await? else {
        return Err(org_not_found());
    };
    let decision = state.authz.authorize(&facts.request(
        Action::RepositoryCreate,
        Resource::Organization,
        Some(Duration::from_secs(auth_age)),
    ));
    if !decision.is_allowed() {
        // Do not mask an authorization failure as a missing organization: an
        // organization member who lacks the grant gets 403, only a stranger
        // gets 404 (issue #133).
        if facts.account.organizations.contains(&org_id) {
            return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
        }
        return Err(org_not_found());
    }

    drop(conn);
    let state = &state;
    let key = idempotency::extract_key(&headers).transpose()?;
    idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        &body,
        key,
        |idem| async move {
            let visibility = input.visibility.as_deref().unwrap_or("private");
            let object_format = input.object_format.as_deref().unwrap_or("sha1");
            let object_format_enum = repositories::object_format(object_format);
            let description = &input.description;

            let mut tx = state.pool.begin().await?;
            let repo = match idem.prior_operation_id {
                // An abandoned attempt left a `creating` row; continue it instead of a second one.
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
                    // create against the same tmp/<operation> while we build it.
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
                    match repositories::insert_creating_with_format(
                        &mut tx,
                        org_id,
                        &input.name,
                        actor.id,
                        operation_id,
                        visibility,
                        description,
                        object_format,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            // 23505 is the name already being taken; `RowNotFound`
                            // is a name a transfer has just claimed at `org_id`.
                            if e.as_database_error()
                                .is_some_and(|d| d.code().as_deref() == Some("23505"))
                                || matches!(e, sqlx::Error::RowNotFound)
                            {
                                return Err(name_taken());
                            }
                            return Err(e.into());
                        }
                    }
                }
            };
            // Only the first attempt emits repository.created; a resume reuses
            // the already-committed row and must not replay the event.
            if idem.prior_operation_id.is_none() {
                event(&mut tx, Some(org_id), actor.id, "repository.created").await?;
            }
            let operation_id = repo.operation_id;
            tx.commit().await?;

            // Acquire the per-repository filesystem lock and re-check the
            // row's status: a later delete may have already removed it, and
            // a stale handler must never recreate the directory of a deleted
            // repository. The guard is then moved into the blocking worker,
            // so a cancelled request cannot drop the lock while the directory
            // operator is still running.
            let repo_id = repo.id;
            let storage = state.storage.clone();
            let guard = storage
                .lock_repository(repo_id)
                .await
                .map_err(|_| storage_error())?;
            let current_status = {
                let mut conn = state.pool.acquire().await?;
                let row = repositories::get(&mut conn, repo_id)
                    .await?
                    .ok_or(ApiError(StatusCode::NOT_FOUND, "not found"))?;
                drop(conn);
                row.status
            };
            match current_status {
                RepositoryStatus::Creating => {
                    tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        storage.create(repo_id, operation_id, object_format_enum)
                    })
                    .await
                    .map_err(|_| {
                        ApiError(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "storage operation failed",
                        )
                    })?
                    // ADR 0003: stay `creating` so the reconciler redoes the create; `failed` only after repeated failures.
                    .map_err(|_| {
                        ApiError(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "repository storage unavailable",
                        )
                    })?;
                }
                // Finished elsewhere (a concurrent retry): nothing to create.
                RepositoryStatus::Active => drop(guard),
                _ => {
                    drop(guard);
                    return Err(not_active());
                }
            }

            // Store the response atomically with the mark-active step (ADR 0003).
            let mut tx = state.pool.begin().await?;
            repositories::set_status(
                &mut tx,
                repo.id,
                RepositoryStatus::Creating,
                RepositoryStatus::Active,
            )
            .await?;
            let row = repositories::get(&mut tx, repo.id)
                .await?
                .ok_or(ApiError(StatusCode::NOT_FOUND, "not found"))?;
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
    if !valid_path_param(&org_slug) {
        return Err(invalid());
    }
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

/// Where a transfer target stands, relative to the source organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAccess {
    /// The caller may create repositories in the target organization.
    MayCreate,
    /// A member of the target who lacks `repository.create`.
    Denied,
    /// Not a member at all: the target is reported exactly as a missing one.
    Unknown,
}

/// Whether the caller may create in `target_org`, asked of the engine the same
/// way the create and fork handlers ask it, including the in-transaction
/// re-authorization every mutation does after locking.
pub(crate) async fn target_access(
    conn: &mut PgConnection,
    state: &AppState,
    actor: Uuid,
    auth_age: u64,
    target_org: Uuid,
    in_transaction: bool,
) -> Result<TargetAccess, ApiError> {
    let facts = if in_transaction {
        facts::load_in_transaction(conn, actor, target_org).await?
    } else {
        facts::load(conn, actor, target_org).await?
    };
    let Some(facts) = facts else {
        return Ok(TargetAccess::Unknown);
    };
    let permitted = state
        .authz
        .authorize(&facts.request(
            Action::RepositoryCreate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    if permitted {
        return Ok(TargetAccess::MayCreate);
    }
    // A member who lacks the grant is a refusal; a stranger to the target only
    // learns that it does not exist, like every other organization endpoint.
    if facts.account.organizations.contains(&target_org) {
        Ok(TargetAccess::Denied)
    } else {
        Ok(TargetAccess::Unknown)
    }
}

/// The refusal a target the caller may not create in gives. Both the outer
/// check and the in-transaction re-check answer the same way.
pub(crate) fn target_refusal(access: TargetAccess) -> ApiError {
    match access {
        // The route names the source repository: a stranger to the target must
        // not be able to tell it apart from a missing source.
        TargetAccess::MayCreate => not_found(),
        TargetAccess::Denied => ApiError(StatusCode::FORBIDDEN, "not permitted"),
        TargetAccess::Unknown => not_found(),
    }
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
    let state = &state;
    let body = &body;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}");

    idempotency::run(
        &state.pool,
        actor.id,
        "PATCH",
        &path_str,
        body,
        key,
        |idem| async move {
            let input: UpdateRepository = serde_json::from_slice(body).map_err(|_| invalid())?;

            if let Some(ref new_name) = input.name
                && !valid_repo_name(new_name)
            {
                return Err(invalid());
            }
            if let Some(ref desc) = input.description
                && (desc.chars().count() > 1000 || has_nul(desc))
            {
                return Err(invalid());
            }

            let loaded = load_current(state, actor.id, auth_age, &org_slug, &name).await?;
            require(state, &loaded, Action::RepositoryUpdate)?;

            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, true).await? {
                return Err(not_active());
            }
            if let Some(ref new_name) = input.name {
                match repositories::rename(&mut tx, loaded.repo.id, new_name).await {
                    Err(e)
                        if e.as_database_error()
                            .is_some_and(|d| d.code().as_deref() == Some("23505"))
                            || matches!(e, sqlx::Error::RowNotFound) =>
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
            let updated = repositories::get(&mut tx, loaded.repo.id).await?.unwrap();
            let result_json = RepositoryJson::from(&updated);
            let status = StatusCode::OK;
            let body = serde_json::to_value(&result_json).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
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
            let state = &state;
            let key = idempotency::extract_key(&headers).transpose()?;

            idempotency::run(
                &state.pool,
                actor.id,
                "POST",
                &path_str,
                b"",
                key,
                |idem| async move {
                    let loaded = load_current(state, actor.id, auth_age, &org_slug, &name).await?;
                    let action = if $archived {
                        Action::RepositoryArchive
                    } else {
                        Action::RepositoryUnarchive
                    };
                    require(state, &loaded, action)?;

                    if loaded.repo.status != RepositoryStatus::Active {
                        return Err(not_active());
                    }

                    if loaded.repo.archived == $archived {
                        let result_json = RepositoryJson::from(&loaded.repo);
                        let mut tx = state.pool.begin().await?;
                        let status = StatusCode::OK;
                        let body = serde_json::to_value(&result_json).unwrap();
                        if !idem.store(&mut tx, status, &body).await? {
                            return Err(idempotency::claim_lost());
                        }
                        tx.commit().await?;
                        return Ok((status, body));
                    }

                    let mut tx = state.pool.begin().await?;
                    if !repositories::lock_active(&mut tx, loaded.repo.id, true).await? {
                        return Err(not_active());
                    }
                    repositories::set_archived(&mut tx, loaded.repo.id, $archived).await?;
                    let event_kind = if $archived {
                        "repository.archived"
                    } else {
                        "repository.unarchived"
                    };
                    event(&mut tx, Some(loaded.org_id), loaded.actor_id, event_kind).await?;
                    // Maintenance skips archived repositories (ADR 0013), so an
                    // unarchive asks for a run rather than waiting a whole
                    // interval. The dedupe key keeps it to one run either way.
                    if !$archived {
                        crate::jobs::enqueue(
                            &mut tx,
                            Some(loaded.org_id),
                            crate::housekeeping::JOB_KIND,
                            serde_json::json!({ "repository_id": loaded.repo.id }),
                            &loaded.repo.id.to_string(),
                            None,
                        )
                        .await?;
                    }
                    let updated = repositories::get(&mut tx, loaded.repo.id).await?.unwrap();
                    let result_json = RepositoryJson::from(&updated);
                    let status = StatusCode::OK;
                    let body = serde_json::to_value(&result_json).unwrap();
                    if !idem.store(&mut tx, status, &body).await? {
                        return Err(idempotency::claim_lost());
                    }
                    tx.commit().await?;
                    Ok((status, body))
                },
            )
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
    let state = &state;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}");
    let key = idempotency::extract_key(&headers).transpose()?;

    let (status, _) = idempotency::run(
        &state.pool,
        actor.id,
        "DELETE",
        &path_str,
        b"",
        key,
        |idem| async move {
            // A crashed delete records the target's operation id, so the retry
            // resolves the exact repository (never a newer one reusing its name)
            // and resumes instead of starting over.
            let resuming = idem.prior_operation_id.is_some();
            let loaded = match idem.prior_operation_id {
                Some(operation_id) => {
                    load_by_operation(state, actor.id, auth_age, operation_id).await?
                }
                None => load_current(state, actor.id, auth_age, &org_slug, &name).await?,
            };
            require(state, &loaded, Action::RepositoryDelete)?;

            let mut was_failed = false;
            if !resuming {
                // A `failed` repository must be deletable, or its name stays reserved forever.
                if !matches!(
                    loaded.repo.status,
                    RepositoryStatus::Active | RepositoryStatus::Failed
                ) {
                    return Err(not_active());
                }
                was_failed = loaded.repo.status == RepositoryStatus::Failed;

                let mut tx = state.pool.begin().await?;
                if !repositories::set_status(
                    &mut tx,
                    loaded.repo.id,
                    loaded.repo.status,
                    RepositoryStatus::Deleting,
                )
                .await?
                {
                    return Err(not_active());
                }
                if !idem
                    .record_operation(&mut tx, loaded.repo.operation_id)
                    .await?
                {
                    return Err(idempotency::claim_lost());
                }
                event(
                    &mut tx,
                    Some(loaded.org_id),
                    loaded.actor_id,
                    "repository.deleted",
                )
                .await?;
                tx.commit().await?;
            } else {
                if !matches!(
                    loaded.repo.status,
                    RepositoryStatus::Deleting | RepositoryStatus::Deleted
                ) {
                    return Err(not_active());
                }
                // Refresh updated_at before storage so the reconciler (which
                // visits only rows older than its grace) does not move the same
                // directory to trash while we trash it below.
                let mut tx = state.pool.begin().await?;
                if !repositories::touch(&mut tx, loaded.repo.id).await? {
                    return Err(not_found());
                }
                tx.commit().await?;
            }

            // A failed create never produced a directory; a resumed delete may
            // already have moved it to trash, so a missing source is not an error.
            // The per-repository lock is held by the blocking worker so a
            // cancelled request cannot drop it while the directory operator is
            // still running.
            let repo_id = loaded.repo.id;
            let storage = state.storage.clone();
            let guard = storage
                .lock_repository(repo_id)
                .await
                .map_err(|_| storage_error())?;
            let trash_result = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                if was_failed {
                    return Ok(());
                }
                match storage.trash(repo_id) {
                    Ok(_) => Ok(()),
                    Err(githearth_git::StorageError::Io(e))
                        if e.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage operation failed",
                )
            })?;

            match trash_result {
                Ok(_) => {
                    // Store the response atomically with the mark-deleted step.
                    let mut tx = state.pool.begin().await?;
                    repositories::finish_delete(&mut tx, repo_id, loaded.actor_id).await?;
                    let status = StatusCode::NO_CONTENT;
                    let body = serde_json::Value::Null;
                    if !idem.store(&mut tx, status, &body).await? {
                        return Err(idempotency::claim_lost());
                    }
                    tx.commit().await?;
                    Ok((status, body))
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

/// Restore a deleted repository from trash (U23c). Shares the operation with
/// the browser form and the CLI; this handler adds authentication,
/// idempotency and the JSON envelope only.
pub(crate) async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
) -> Result<(StatusCode, Json<RepositoryJson>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}/restore");
    let key = idempotency::extract_key(&headers).transpose()?;

    idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &path_str,
        b"",
        key,
        |idem| async move {
            let row = crate::repository_restore::restore_repository(
                state,
                actor.id,
                auth_age,
                &org_slug,
                &name,
                idem.prior_operation_id,
                Some(&idem),
            )
            .await?;
            let result_json = RepositoryJson::from(&row);
            let status = StatusCode::OK;
            let body = serde_json::to_value(&result_json).unwrap();
            let mut tx = state.pool.begin().await?;
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await
    .map(|(_, value)| (StatusCode::OK, Json(serde_json::from_value(value).unwrap())))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AccessGrant {
    id: Uuid,
    subject_kind: String,
    subject_id: Uuid,
    role: String,
    expires_at: Option<String>,
    granted_by: Uuid,
    created_at: Option<String>,
}

// Timestamps are rendered in UTC whatever the session time zone is.
const GRANT_COLUMNS: &str = "id, subject_kind, subject_id, role, granted_by, \
 to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

impl AccessGrant {
    fn from_row(row: &sqlx::postgres::PgRow) -> Self {
        AccessGrant {
            id: row.get("id"),
            subject_kind: row.get("subject_kind"),
            subject_id: row.get("subject_id"),
            role: row.get("role"),
            expires_at: row.get("expires_at"),
            granted_by: row.get("granted_by"),
            created_at: row.get("created_at"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantAccessBody {
    subject_kind: String,
    subject_id: Uuid,
    role: String,
    expires_at: Option<String>,
}

pub(crate) async fn list_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<AccessGrant>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = load_repo(&state, actor.id, auth_age, &org_slug, &name).await?;
    require(&state, &loaded, Action::RepositoryManageAccess)?;

    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }

    let rows = sqlx::query::<sqlx::Postgres>(&format!(
        "SELECT {GRANT_COLUMNS} FROM grants \
         WHERE repository_id = $1 AND revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
         AND ($2::uuid IS NULL OR id > $2) ORDER BY id LIMIT $3"
    ))
    .bind(loaded.repo.id)
    .bind(page.after)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let grants = rows.iter().map(AccessGrant::from_row).collect();
    Ok(Json(grants))
}

pub(crate) async fn grant_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<AccessGrant>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let body = &body;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{name}/access");

    idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let input: GrantAccessBody =
                serde_json::from_slice(body).map_err(|_| invalid())?;

            if !matches!(input.subject_kind.as_str(), "account" | "team") {
                return Err(invalid());
            }
            if !matches!(
                input.role.as_str(),
                "read" | "triage" | "write" | "maintain" | "admin"
            ) {
                return Err(invalid());
            }

            let loaded = load_current(state, actor.id, auth_age, &org_slug, &name).await?;
            require(state, &loaded, Action::RepositoryManageAccess)?;

            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }

            sqlx::query(
                "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
                 WHERE repository_id = $2 AND subject_kind = $3 AND subject_id = $4 \
                 AND revoked_at IS NULL",
            )
            .bind(actor.id)
            .bind(loaded.repo.id)
            .bind(&input.subject_kind)
            .bind(input.subject_id)
            .execute(&mut *tx)
            .await?;

            let grant_id = Uuid::new_v4();
            // The trigger rejects bad subjects/roles (23514); PostgreSQL rejects a bad expires_at (22007/22008).
            sqlx::query(
                "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, repository_id, expires_at, granted_by) \
                 VALUES ($1, $2, $3, $4, 'repository', $5, $6, $7::timestamptz, $8)",
            )
            .bind(grant_id)
            .bind(&input.role)
            .bind(&input.subject_kind)
            .bind(input.subject_id)
            .bind(loaded.org_id)
            .bind(loaded.repo.id)
            .bind(input.expires_at.as_deref())
            .bind(actor.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if e.as_database_error().is_some_and(|d| {
                    matches!(d.code().as_deref(), Some("23514" | "22007" | "22008"))
                })
                {
                    ApiError(StatusCode::BAD_REQUEST, "invalid grant")
                } else {
                    e.into()
                }
            })?;

            event(
                &mut tx,
                Some(loaded.org_id),
                loaded.actor_id,
                "repository.access_granted",
            )
            .await?;
            let row = sqlx::query(&format!("SELECT {GRANT_COLUMNS} FROM grants WHERE id = $1"))
                .bind(grant_id)
                .fetch_one(&mut *tx)
                .await?;
            let grant = AccessGrant::from_row(&row);
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&grant).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
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

pub(crate) async fn revoke_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name, grant_id)): Path<(String, String, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{name}/access/{grant_id}");
    let key = idempotency::extract_key(&headers).transpose()?;

    let (status, _) = idempotency::run(
        &state.pool,
        actor.id,
        "DELETE",
        &path_str,
        b"",
        key,
        |idem| async move {
            let loaded = load_current(state, actor.id, auth_age, &org_slug, &name).await?;
            require(state, &loaded, Action::RepositoryManageAccess)?;

            // Unlike granting, revoking is allowed while the repository is `deleting`: removing access is always safe.
            let mut tx = state.pool.begin().await?;
            let result = sqlx::query(
                "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
                 WHERE id = $2 AND repository_id = $3 AND revoked_at IS NULL",
            )
            .bind(actor.id)
            .bind(grant_id)
            .bind(loaded.repo.id)
            .execute(&mut *tx)
            .await?;

            if result.rows_affected() == 0 {
                return Err(ApiError(StatusCode::NOT_FOUND, "not found"));
            }

            event(
                &mut tx,
                Some(loaded.org_id),
                loaded.actor_id,
                "repository.access_revoked",
            )
            .await?;
            let status = StatusCode::NO_CONTENT;
            let body = serde_json::Value::Null;
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use githearth_git::layout::Storage;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use uuid::Uuid;

    /// The repository lock must live inside the blocking worker, not the
    /// awaiting task: if the awaiting task is cancelled the worker must keep
    /// the lock until it exits, so a re-claimed handler cannot start the same
    /// directory operation concurrently.
    #[tokio::test]
    async fn storage_lock_lives_in_the_blocking_worker_until_it_exits() {
        let temp =
            std::env::temp_dir().join(format!("githearth-lock-test-{}", Uuid::new_v4().simple()));
        let storage = Storage::open(&temp).unwrap();
        let repo_id = Uuid::new_v4();

        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));

        let entered_w = entered.clone();
        let release_w = release.clone();
        let lock = storage.repository_lock(repo_id);
        let guard = lock.lock_owned().await;
        let worker = tokio::task::spawn_blocking(move || {
            let _held = guard;
            entered_w.store(true, Ordering::SeqCst);
            while !release_w.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });

        // Detach the handle ("cancel" the awaiting task): the worker keeps the lock.
        drop(worker);

        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_w = acquired.clone();
        let lock2 = storage.repository_lock(repo_id);
        let waiter = tokio::spawn(async move {
            let _g = lock2.lock_owned().await;
            acquired_w.store(true, Ordering::SeqCst);
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !acquired.load(Ordering::SeqCst),
            "second acquisition won the lock while the worker still held it"
        );

        release.store(true, Ordering::SeqCst);
        waiter.await.unwrap();
        assert!(acquired.load(Ordering::SeqCst));

        // Cleanup.
        std::fs::remove_dir_all(&temp).unwrap();
    }
}
