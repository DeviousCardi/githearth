use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Issue, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, event, idempotency, invalid, repositories,
    repository_api::{self, Loaded},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/issues",
            axum::routing::post(create).get(list),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/issues/{number}",
            axum::routing::get(read).patch(update),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/issues/{number}/comments",
            axum::routing::post(create_comment).get(list_comments),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "issue not found")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
// Reads treat a non-active repository like a missing one; only an `active`
// repository (archived included) is readable.
fn require_readable_repo(loaded: &Loaded) -> Result<(), ApiError> {
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_found());
    }
    Ok(())
}

const ISSUE_COLUMNS: &str = "id, number, title, body, state, author_id, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, \
 to_char(closed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS closed_at";

const COMMENT_COLUMNS: &str = "id, author_id, body, seq, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

#[derive(Serialize, Deserialize, sqlx::FromRow)]
struct IssueJson {
    id: Uuid,
    number: i32,
    title: String,
    body: String,
    state: String,
    author_id: Uuid,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
struct CommentJson {
    id: Uuid,
    author_id: Uuid,
    body: String,
    seq: i32,
    created_at: String,
    updated_at: String,
}

async fn fetch_issue(conn: &mut PgConnection, id: Uuid) -> Result<Option<IssueJson>, sqlx::Error> {
    sqlx::query_as(&format!("SELECT {ISSUE_COLUMNS} FROM issues WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
}

async fn find_issue(
    conn: &mut PgConnection,
    repository_id: Uuid,
    number: i32,
) -> Result<Option<IssueJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {ISSUE_COLUMNS} FROM issues WHERE repository_id = $1 AND number = $2"
    ))
    .bind(repository_id)
    .bind(number)
    .fetch_optional(&mut *conn)
    .await
}

fn require_issue(
    state: &AppState,
    loaded: &Loaded,
    issue: &IssueJson,
    action: Action,
) -> Result<(), ApiError> {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let entity = Issue {
        id: issue.id,
        repository: loaded.repo.id,
        author: issue.author_id,
    };
    let decision = state.authz.authorize(&loaded.facts.request(
        action,
        Resource::Issue(&authz_repo, &entity),
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

async fn allocate_number(conn: &mut PgConnection, repository_id: Uuid) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO issue_counters (repository_id, last) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET last = issue_counters.last + 1 \
         RETURNING last",
    )
    .bind(repository_id)
    .fetch_one(&mut *conn)
    .await
}

async fn allocate_comment_seq(conn: &mut PgConnection, issue_id: Uuid) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO issue_comment_counters (issue_id, last) VALUES ($1, 1) \
         ON CONFLICT (issue_id) DO UPDATE SET last = issue_comment_counters.last + 1 \
         RETURNING last",
    )
    .bind(issue_id)
    .fetch_one(&mut *conn)
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewIssue {
    title: String,
    #[serde(default)]
    body: String,
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<IssueJson>), ApiError> {
    let input: NewIssue = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !(1..=256).contains(&input.title.chars().count()) || input.body.chars().count() > 65536 {
        return Err(invalid());
    }

    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/issues");

    let (status, value) = idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &path_str,
        &body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
                    .await?;
            repository_api::require(state, &loaded, Action::IssueCreate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id).await? {
                return Err(not_active());
            }
            let number = allocate_number(&mut tx, loaded.repo.id).await?;
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO issues (id, repository_id, number, title, body, author_id) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .bind(number)
            .bind(&input.title)
            .bind(&input.body)
            .bind(actor.id)
            .execute(&mut *tx)
            .await?;
            event(&mut tx, Some(loaded.org_id), actor.id, "issue.created").await?;

            let issue = fetch_issue(&mut tx, id).await?.unwrap();
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&issue).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    after: Option<i32>,
}

#[derive(Serialize)]
struct IssuePage {
    items: Vec<IssueJson>,
    next: Option<i32>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Json<IssuePage>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state_filter = query.state.as_deref().unwrap_or("open");
    if !matches!(state_filter, "open" | "closed" | "all") {
        return Err(invalid());
    }
    let limit = query.limit.unwrap_or(30);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }

    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    repository_api::require(&state, &loaded, Action::RepositoryRead)?;
    require_readable_repo(&loaded)?;

    let mut conn = state.pool.acquire().await?;
    let sql = format!(
        "SELECT {ISSUE_COLUMNS} FROM issues WHERE repository_id = $1 \
         AND ($2::integer IS NULL OR number < $2) \
         AND ($3::text IS NULL OR state = $3) \
         ORDER BY number DESC LIMIT $4"
    );
    let state = if state_filter == "all" {
        None
    } else {
        Some(state_filter)
    };
    let rows = sqlx::query_as::<_, IssueJson>(&sql)
        .bind(loaded.repo.id)
        .bind(query.after)
        .bind(state)
        .bind(limit + 1)
        .fetch_all(&mut *conn)
        .await?;

    let has_more = rows.len() as i64 > limit;
    let mut items = rows;
    items.truncate(limit as usize);
    let next = if has_more {
        items.last().map(|i| i.number)
    } else {
        None
    };
    Ok(Json(IssuePage { items, next }))
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<IssueJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let issue = find_issue(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    require_issue(&state, &loaded, &issue, Action::IssueRead)?;
    Ok(Json(issue))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchIssue {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<IssueJson>, ApiError> {
    let input: PatchIssue = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if input.title.is_none() && input.body.is_none() && input.state.is_none() {
        return Err(invalid());
    }
    if let Some(ref t) = input.title
        && !(1..=256).contains(&t.chars().count())
    {
        return Err(invalid());
    }
    if let Some(ref b) = input.body
        && b.chars().count() > 65536
    {
        return Err(invalid());
    }
    if let Some(ref s) = input.state
        && !matches!(s.as_str(), "open" | "closed")
    {
        return Err(invalid());
    }

    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/issues/{number}");

    let (_, value) = idempotency::run(
        &state.pool,
        actor.id,
        "PATCH",
        &path_str,
        &body,
        key,
        |idem| async move {
            let loaded = repository_api::load_current(
                state,
                actor.id,
                auth_age,
                &org_slug,
                &repo_name,
            )
            .await?;

            let mut conn = state.pool.acquire().await?;
            let issue = find_issue(&mut conn, loaded.repo.id, number)
                .await?
                .ok_or_else(not_found)?;
            drop(conn);

            // Closing, reopening and editing all gate on the same triage action.
            require_issue(state, &loaded, &issue, Action::IssueClose)?;

            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id).await? {
                return Err(not_active());
            }
            // Lock the row so concurrent PATCHes cannot stomp fields we did not send.
            let locked: IssueJson = sqlx::query_as(&format!(
                "SELECT {ISSUE_COLUMNS} FROM issues WHERE id = $1 FOR UPDATE"
            ))
            .bind(issue.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(not_found)?;
            let new_state = input.state.as_deref().unwrap_or(locked.state.as_str());
            sqlx::query(
                "UPDATE issues SET title = $1, body = $2, state = $3, updated_at = now() WHERE id = $4",
            )
            .bind(input.title.as_deref().unwrap_or(&locked.title))
            .bind(input.body.as_deref().unwrap_or(&locked.body))
            .bind(new_state)
            .bind(locked.id)
            .execute(&mut *tx)
            .await?;

            let kind = if new_state != locked.state {
                if new_state == "closed" { "issue.closed" } else { "issue.reopened" }
            } else {
                "issue.updated"
            };
            event(&mut tx, Some(loaded.org_id), actor.id, kind).await?;

            let updated = fetch_issue(&mut tx, issue.id).await?.unwrap();
            let status = StatusCode::OK;
            let body = serde_json::to_value(&updated).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok(Json(serde_json::from_value(value).unwrap()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewComment {
    body: String,
}

async fn create_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<CommentJson>), ApiError> {
    let input: NewComment = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !(1..=65536).contains(&input.body.chars().count()) {
        return Err(invalid());
    }

    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str = format!(
        "/api/v1/organizations/{org_slug}/repositories/{repo_name}/issues/{number}/comments"
    );

    let (status, value) = idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &path_str,
        &body,
        key,
        |idem| async move {
            let loaded = repository_api::load_current(
                state,
                actor.id,
                auth_age,
                &org_slug,
                &repo_name,
            )
            .await?;

            let mut conn = state.pool.acquire().await?;
            let issue = find_issue(&mut conn, loaded.repo.id, number)
                .await?
                .ok_or_else(not_found)?;
            drop(conn);

            require_issue(state, &loaded, &issue, Action::IssueComment)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id).await? {
                return Err(not_active());
            }
            let id = Uuid::new_v4();
            let seq = allocate_comment_seq(&mut tx, issue.id).await?;
            sqlx::query(
                "INSERT INTO issue_comments (id, issue_id, author_id, body, seq) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(issue.id)
            .bind(actor.id)
            .bind(&input.body)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE issues SET updated_at = now() WHERE id = $1")
                .bind(issue.id)
                .execute(&mut *tx)
                .await?;
            event(&mut tx, Some(loaded.org_id), actor.id, "issue.commented").await?;

            let comment: CommentJson = sqlx::query_as(&format!(
                "SELECT {COMMENT_COLUMNS} FROM issue_comments WHERE id = $1"
            ))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&comment).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentListQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    after: Option<i32>,
}

#[derive(Serialize)]
struct CommentPage {
    items: Vec<CommentJson>,
    next: Option<i32>,
}

async fn list_comments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    Query(query): Query<CommentListQuery>,
) -> Result<Json<CommentPage>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let limit = query.limit.unwrap_or(30);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    // The cursor is a per-issue comment sequence; anything else is not a valid cursor.
    if let Some(after) = query.after
        && after < 1
    {
        return Err(invalid());
    }

    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    require_readable_repo(&loaded)?;

    let mut conn = state.pool.acquire().await?;
    let issue = find_issue(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    require_issue(&state, &loaded, &issue, Action::IssueRead)?;

    let rows = sqlx::query_as::<_, CommentJson>(&format!(
        "SELECT {COMMENT_COLUMNS} FROM issue_comments WHERE issue_id = $1 \
         AND ($2::integer IS NULL OR seq > $2) \
         ORDER BY seq LIMIT $3"
    ))
    .bind(issue.id)
    .bind(query.after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;

    let has_more = rows.len() as i64 > limit;
    let mut items = rows;
    items.truncate(limit as usize);
    let next = if has_more {
        items.last().map(|c| c.seq)
    } else {
        None
    };
    Ok(Json(CommentPage { items, next }))
}
