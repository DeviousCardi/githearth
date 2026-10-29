//! Pull requests, review comments and reviews between two branches of one
//! repository.
//!
//! A pull request stores branch names, not commits: its head and base are
//! resolved from Git on every read. Each review records the head commit it
//! was given on, and only reviews at the current head count, so new commits
//! make earlier approvals and change requests stale.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, PullRequest, Ref, Resource};
use githearth_core::RepositoryStatus;
use githearth_git::browse as git_browse;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, browse, event, idempotency, invalid,
    issues::{allocate_number, not_active, require_readable_repo},
    repositories,
    repository_api::{self, Loaded},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls",
            axum::routing::post(create).get(list),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}",
            axum::routing::get(read).patch(update),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/comments",
            axum::routing::post(create_comment).get(list_comments),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/reviews",
            axum::routing::post(create_review).get(list_reviews),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "pull request not found")
}
fn branch_missing() -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, "branch not found")
}
fn head_moved() -> ApiError {
    ApiError(StatusCode::CONFLICT, "commit is not the pull request head")
}
fn path_not_found() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "path not found at commit")
}
fn not_text() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "path is not a text file")
}
fn line_out_of_range() -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "line out of range")
}
fn not_open() -> ApiError {
    ApiError(StatusCode::CONFLICT, "pull request is not open")
}

/// Re-checks inside the transaction, on the same row lock the PATCH
/// close/reopen path takes, that the pull request is still open. A close that
/// commits between the earlier head check and the review insert must not leave
/// a review on a closed pull request.
async fn require_open(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(conn)
            .await?;
    match state.as_deref() {
        Some("open") => Ok(()),
        _ => Err(not_open()),
    }
}
fn already_open() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "an open pull request already exists for these branches",
    )
}
fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("23505"))
}

/// A short branch name, checked more strictly than `git check-ref-format`
/// needs, so it can never name a revision expression or another ref.
pub(crate) fn valid_branch(name: &str) -> bool {
    (1..=255).contains(&name.len())
        && git_browse::valid_rev(name)
        && !name.starts_with("refs/")
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("//")
        && !name.contains("/.")
        && !name.starts_with('.')
        && !name.contains(['~', '^', '@', '{', '}', '\\', '*', '?', '['])
}

/// Full lowercase commit ids only, as stored and compared.
fn valid_commit_id(id: &str) -> bool {
    browse::is_commit_id(id) && !id.bytes().any(|b| b.is_ascii_uppercase())
}

/// Number of text lines in a blob's content: newline-terminated segments plus
/// a trailing segment with no newline. An empty blob has no lines.
fn line_count(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.iter().filter(|&&b| b == b'\n').count();
    if content.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

const PULL_COLUMNS: &str = "id, number, title, body, state, author_id, source_branch, target_branch, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, \
 to_char(closed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS closed_at";

const COMMENT_COLUMNS: &str = "id, author_id, body, seq, commit_id, path, line, in_reply_to, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

const REVIEW_COLUMNS: &str = "id, seq, reviewer_id, verdict, commit_id, body, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct PullJson {
    pub(crate) id: Uuid,
    pub(crate) number: i32,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) state: String,
    pub(crate) author_id: Uuid,
    pub(crate) source_branch: String,
    pub(crate) target_branch: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) closed_at: Option<String>,
}

/// A pull request with its live head and base and the reviews that count
/// at that head. A deleted branch reads as a null id.
#[derive(Serialize, Deserialize)]
pub(crate) struct PullDetail {
    #[serde(flatten)]
    pub(crate) pull: PullJson,
    pub(crate) head_id: Option<String>,
    pub(crate) base_id: Option<String>,
    pub(crate) approvals: i64,
    pub(crate) changes_requested: i64,
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct CommentJson {
    pub(crate) id: Uuid,
    pub(crate) author_id: Uuid,
    pub(crate) body: String,
    pub(crate) seq: i32,
    pub(crate) commit_id: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<i32>,
    pub(crate) in_reply_to: Option<Uuid>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ReviewJson {
    pub(crate) id: Uuid,
    pub(crate) seq: i64,
    pub(crate) reviewer_id: Uuid,
    pub(crate) verdict: String,
    pub(crate) commit_id: String,
    pub(crate) body: String,
    pub(crate) created_at: String,
}

async fn fetch_pull(conn: &mut PgConnection, id: Uuid) -> Result<Option<PullJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {PULL_COLUMNS} FROM pull_requests WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
}

pub(crate) async fn find_pull(
    conn: &mut PgConnection,
    repository_id: Uuid,
    number: i32,
) -> Result<Option<PullJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {PULL_COLUMNS} FROM pull_requests WHERE repository_id = $1 AND number = $2"
    ))
    .bind(repository_id)
    .bind(number)
    .fetch_optional(&mut *conn)
    .await
}

/// Loads pull request `number` and requires `action` on it.
async fn load_pull(
    state: &AppState,
    loaded: &Loaded,
    number: i32,
    action: Action,
) -> Result<PullJson, ApiError> {
    let mut conn = state.pool.acquire().await?;
    let pull = find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    require_pull(state, loaded, &pull, action)?;
    Ok(pull)
}

pub(crate) fn require_pull(
    state: &AppState,
    loaded: &Loaded,
    pull: &PullJson,
    action: Action,
) -> Result<(), ApiError> {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let entity = PullRequest {
        id: pull.id,
        repository: loaded.repo.id,
        author: pull.author_id,
        // shortcut: no protection rules yet; they arrive with branch protection.
        target: Ref {
            repository: loaded.repo.id,
            name: format!("refs/heads/{}", pull.target_branch),
            protection: None,
            exists: true,
        },
    };
    let decision = state.authz.authorize(&loaded.facts.request(
        action,
        Resource::PullRequest(&authz_repo, &entity),
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

/// The commit `branch` points at, or None when the branch does not exist.
pub(crate) async fn branch_tip(
    state: &AppState,
    repo_id: Uuid,
    branch: &str,
) -> Result<Option<String>, ApiError> {
    let rev = format!("refs/heads/{branch}");
    browse::git_read(state, repo_id, move |path, timeout| {
        git_browse::resolve(path, &rev, timeout)
    })
    .await
}

async fn tips(state: &AppState, repo_id: Uuid, pull: &PullJson) -> Result<Tips, ApiError> {
    Ok((
        branch_tip(state, repo_id, &pull.source_branch).await?,
        branch_tip(state, repo_id, &pull.target_branch).await?,
    ))
}
type Tips = (Option<String>, Option<String>);

/// Builds the detail on `conn`, so a caller inside a transaction needs no
/// second connection; `tips` are resolved beforehand, outside it.
async fn detail(
    conn: &mut PgConnection,
    pull: PullJson,
    tips: Tips,
) -> Result<PullDetail, ApiError> {
    let (head_id, base_id) = tips;
    let (approvals, changes_requested) = match &head_id {
        // Each reviewer's latest verdict at this head; comment-only reviews
        // neither grant nor withdraw approval.
        Some(head) => {
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE verdict = 'approved'), \
                 count(*) FILTER (WHERE verdict = 'changes_requested') FROM ( \
                 SELECT DISTINCT ON (reviewer_id) verdict FROM pull_request_reviews \
                 WHERE pull_request_id = $1 AND commit_id = $2 AND verdict <> 'commented' \
                 ORDER BY reviewer_id, seq DESC) latest",
            )
            .bind(pull.id)
            .bind(head)
            .fetch_one(&mut *conn)
            .await?
        }
        None => (0, 0),
    };
    Ok(PullDetail {
        pull,
        head_id,
        base_id,
        approvals,
        changes_requested,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPull {
    title: String,
    #[serde(default)]
    body: String,
    source_branch: String,
    target_branch: String,
}

fn new_pull(body: &[u8]) -> Result<NewPull, ApiError> {
    let input: NewPull = serde_json::from_slice(body).map_err(|_| invalid())?;
    if input.title.trim().is_empty()
        || input.title.chars().count() > 256
        || input.body.chars().count() > 65536
        || !valid_branch(&input.source_branch)
        || !valid_branch(&input.target_branch)
        || input.source_branch == input.target_branch
    {
        return Err(invalid());
    }
    Ok(input)
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<PullDetail>), ApiError> {
    new_pull(&body)?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, value) = create_pull(
        &state, actor.id, auth_age, &org_slug, &repo_name, &body, key,
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

/// Opens a pull request as `actor_id`.
pub(crate) async fn create_pull(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let input = new_pull(body)?;
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls");

    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            repository_api::require(state, &loaded, Action::PullRequestCreate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }
            let head = branch_tip(state, loaded.repo.id, &input.source_branch).await?;
            let base = branch_tip(state, loaded.repo.id, &input.target_branch).await?;
            if head.is_none() || base.is_none() {
                return Err(branch_missing());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let number = allocate_number(&mut tx, loaded.repo.id).await?;
            let id = Uuid::new_v4();
            let inserted = sqlx::query(
                "INSERT INTO pull_requests \
                 (id, repository_id, number, title, body, author_id, source_branch, target_branch) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (repository_id, source_branch, target_branch) WHERE state = 'open' \
                 DO NOTHING",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .bind(number)
            .bind(&input.title)
            .bind(&input.body)
            .bind(actor_id)
            .bind(&input.source_branch)
            .bind(&input.target_branch)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                return Err(already_open());
            }
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "pull_request.created",
            )
            .await?;

            let pull = fetch_pull(&mut tx, id).await?.unwrap();
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(detail(&mut tx, pull, (head, base)).await?).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<i64>,
    #[serde(default)]
    pub(crate) after: Option<i32>,
}

#[derive(Serialize)]
pub(crate) struct PullPage {
    pub(crate) items: Vec<PullJson>,
    pub(crate) next: Option<i32>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Json<PullPage>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    list_pulls(&state, actor.id, auth_age, &org_slug, &repo_name, query)
        .await
        .map(Json)
}

/// Newest first; list items carry no live head, which needs Git per item.
pub(crate) async fn list_pulls(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    query: ListQuery,
) -> Result<PullPage, ApiError> {
    let state_filter = query.state.as_deref().unwrap_or("open");
    if !matches!(state_filter, "open" | "closed" | "all") {
        return Err(invalid());
    }
    let limit = query.limit.unwrap_or(30);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }

    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    repository_api::require(state, &loaded, Action::RepositoryRead)?;
    require_readable_repo(&loaded)?;

    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, PullJson>(&format!(
        "SELECT {PULL_COLUMNS} FROM pull_requests WHERE repository_id = $1 \
         AND ($2::integer IS NULL OR number < $2) \
         AND ($3::text IS NULL OR state = $3) \
         ORDER BY number DESC LIMIT $4"
    ))
    .bind(loaded.repo.id)
    .bind(query.after)
    .bind((state_filter != "all").then_some(state_filter))
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;

    let has_more = rows.len() as i64 > limit;
    let mut items = rows;
    items.truncate(limit as usize);
    let next = if has_more {
        items.last().map(|p| p.number)
    } else {
        None
    };
    Ok(PullPage { items, next })
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<PullDetail>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    read_pull(&state, actor.id, auth_age, &org_slug, &repo_name, number)
        .await
        .map(Json)
}

pub(crate) async fn read_pull(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
) -> Result<PullDetail, ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    let pull = load_pull(state, &loaded, number, Action::PullRequestRead).await?;
    let tips = tips(state, loaded.repo.id, &pull).await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, pull, tips).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchPull {
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
) -> Result<Json<PullDetail>, ApiError> {
    let input: PatchPull = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if (input.title.is_none() && input.body.is_none() && input.state.is_none())
        || input
            .title
            .as_ref()
            .is_some_and(|t| t.trim().is_empty() || t.chars().count() > 256)
        || input
            .body
            .as_ref()
            .is_some_and(|b| b.chars().count() > 65536)
        || input
            .state
            .as_ref()
            .is_some_and(|s| !matches!(s.as_str(), "open" | "closed"))
    {
        return Err(invalid());
    }

    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let state = &state;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}");

    let (_, value) = idempotency::run(
        &state.pool,
        actor.id,
        "PATCH",
        &path_str,
        &body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
                    .await?;
            // Closing, reopening and editing all gate on the same triage action.
            let pull = load_pull(state, &loaded, number, Action::PullRequestClose).await?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }
            let tips = tips(state, loaded.repo.id, &pull).await?;

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let locked: PullJson = sqlx::query_as(&format!(
                "SELECT {PULL_COLUMNS} FROM pull_requests WHERE id = $1 FOR UPDATE"
            ))
            .bind(pull.id)
            .fetch_one(&mut *tx)
            .await?;
            let new_state = input.state.as_deref().unwrap_or(locked.state.as_str());
            let updated = sqlx::query(
                "UPDATE pull_requests SET title = $1, body = $2, state = $3, updated_at = now() \
                 WHERE id = $4",
            )
            .bind(input.title.as_deref().unwrap_or(&locked.title))
            .bind(input.body.as_deref().unwrap_or(&locked.body))
            .bind(new_state)
            .bind(locked.id)
            .execute(&mut *tx)
            .await;
            match updated {
                Err(error) if is_unique_violation(&error) => return Err(already_open()),
                other => other?,
            };

            let kind = match (locked.state.as_str(), new_state) {
                ("open", "closed") => "pull_request.closed",
                ("closed", "open") => "pull_request.reopened",
                _ => "pull_request.updated",
            };
            event(&mut tx, Some(loaded.org_id), actor.id, kind).await?;

            // Closing a pull request cancels any queued or validating merge
            // intent; an executing intent is not cancelled here (the ref commit
            // wins, and record_merge_result / the reconciler settle it).
            if locked.state == "open" && new_state == "closed" {
                sqlx::query(
                    "UPDATE merge_intents SET state = 'cancelled', updated_at = now() \
                     WHERE pull_request_id = $1 AND state IN ('queued', 'validating')",
                )
                .bind(locked.id)
                .execute(&mut *tx)
                .await?;
            }

            let pull = fetch_pull(&mut tx, locked.id).await?.unwrap();
            let status = StatusCode::OK;
            let body = serde_json::to_value(detail(&mut tx, pull, tips).await?).unwrap();
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
    #[serde(default)]
    commit_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<i32>,
    #[serde(default)]
    in_reply_to: Option<Uuid>,
}

fn new_comment(body: &[u8]) -> Result<NewComment, ApiError> {
    let input: NewComment = serde_json::from_slice(body).map_err(|_| invalid())?;
    let anchored = input.commit_id.is_some();
    if input.body.trim().is_empty()
        || input.body.chars().count() > 65536
        // An anchor names all three of commit, path and line, or none of them.
        || anchored != input.path.is_some()
        || anchored != input.line.is_some()
        || (anchored && input.in_reply_to.is_some())
        || input.commit_id.as_deref().is_some_and(|c| !valid_commit_id(c))
        || input.path.as_deref().is_some_and(|p| !git_browse::valid_path(p))
        || input.line.is_some_and(|l| l < 1)
    {
        return Err(invalid());
    }
    Ok(input)
}

async fn create_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<CommentJson>), ApiError> {
    new_comment(&body)?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, value) = create_pull_comment(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, &body, key,
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

/// Comments on a pull request as `actor_id`. An anchored comment must name
/// the current head; a reply must name a comment on the same pull request.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_pull_comment(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let input = new_comment(body)?;
    let path_str = format!(
        "/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/comments"
    );

    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            let pull = load_pull(state, &loaded, number, Action::PullRequestComment).await?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }
            if let Some(commit) = input.commit_id.clone() {
                if branch_tip(state, loaded.repo.id, &pull.source_branch)
                    .await?
                    .as_ref()
                    != Some(&commit)
                {
                    return Err(head_moved());
                }
                // An anchored comment must point at a real line of a real
                // blob at that commit. `browse::blob` reads through the
                // blocking Git pool with the configured timeout and reports a
                // missing path, a directory or a submodule as `None`.
                let path = input.path.clone().unwrap();
                let line = input.line.unwrap();
                let commit_for_git = commit.clone();
                let blob = browse::git_read(state, loaded.repo.id, move |fs_path, timeout| {
                    git_browse::blob(
                        fs_path,
                        &commit_for_git,
                        &path,
                        git_browse::MAX_BLOB_BYTES,
                        timeout,
                    )
                })
                .await?
                .ok_or_else(path_not_found)?;
                if blob.binary {
                    return Err(not_text());
                }
                // A truncated blob undercounts its lines; only a complete one
                // can prove the line is past the end.
                if !blob.truncated && line as usize > line_count(&blob.content) {
                    return Err(line_out_of_range());
                }
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            if let Some(parent) = input.in_reply_to {
                let same_pull: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pull_request_comments \
                     WHERE id = $1 AND pull_request_id = $2)",
                )
                .bind(parent)
                .bind(pull.id)
                .fetch_one(&mut *tx)
                .await?;
                if !same_pull {
                    return Err(invalid());
                }
            }
            let seq: i32 = sqlx::query_scalar(
                "INSERT INTO pull_request_comment_counters (pull_request_id, last) VALUES ($1, 1) \
                 ON CONFLICT (pull_request_id) DO UPDATE \
                 SET last = pull_request_comment_counters.last + 1 RETURNING last",
            )
            .bind(pull.id)
            .fetch_one(&mut *tx)
            .await?;
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO pull_request_comments \
                 (id, pull_request_id, author_id, body, seq, commit_id, path, line, in_reply_to) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(id)
            .bind(pull.id)
            .bind(actor_id)
            .bind(&input.body)
            .bind(seq)
            .bind(&input.commit_id)
            .bind(&input.path)
            .bind(input.line)
            .bind(input.in_reply_to)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE pull_requests SET updated_at = now() WHERE id = $1")
                .bind(pull.id)
                .execute(&mut *tx)
                .await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "pull_request.commented",
            )
            .await?;

            let comment: CommentJson = sqlx::query_as(&format!(
                "SELECT {COMMENT_COLUMNS} FROM pull_request_comments WHERE id = $1"
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
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeqQuery {
    #[serde(default)]
    pub(crate) limit: Option<i64>,
    #[serde(default)]
    pub(crate) after: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct Page<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next: Option<i64>,
}

/// Checks a sequence-cursor query and loads the pull request it pages.
async fn seq_page_start(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    query: &SeqQuery,
) -> Result<(PullJson, i64), ApiError> {
    let limit = query.limit.unwrap_or(30);
    if !(1..=100).contains(&limit) || query.after.is_some_and(|a| a < 1) {
        return Err(invalid());
    }
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    let pull = load_pull(state, &loaded, number, Action::PullRequestRead).await?;
    Ok((pull, limit))
}

fn page<T>(mut rows: Vec<T>, limit: i64, seq: impl Fn(&T) -> i64) -> Page<T> {
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next = if has_more { rows.last().map(seq) } else { None };
    Page { items: rows, next }
}

async fn list_comments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    Query(query): Query<SeqQuery>,
) -> Result<Json<Page<CommentJson>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    list_pull_comments(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, query,
    )
    .await
    .map(Json)
}

/// Oldest first, threads flattened; a reply names its parent.
pub(crate) async fn list_pull_comments(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    query: SeqQuery,
) -> Result<Page<CommentJson>, ApiError> {
    let (pull, limit) = seq_page_start(
        state, actor_id, auth_age, org_slug, repo_name, number, &query,
    )
    .await?;
    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, CommentJson>(&format!(
        "SELECT {COMMENT_COLUMNS} FROM pull_request_comments WHERE pull_request_id = $1 \
         AND ($2::bigint IS NULL OR seq > $2) ORDER BY seq LIMIT $3"
    ))
    .bind(pull.id)
    .bind(query.after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    Ok(page(rows, limit, |c| c.seq.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReview {
    verdict: String,
    commit_id: String,
    #[serde(default)]
    body: String,
}

fn new_review(body: &[u8]) -> Result<NewReview, ApiError> {
    let input: NewReview = serde_json::from_slice(body).map_err(|_| invalid())?;
    let valid_verdict = match input.verdict.as_str() {
        "approved" | "changes_requested" => true,
        "commented" => !input.body.trim().is_empty(),
        _ => false,
    };
    if !valid_verdict || !valid_commit_id(&input.commit_id) || input.body.chars().count() > 65536 {
        return Err(invalid());
    }
    Ok(input)
}

async fn create_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<ReviewJson>), ApiError> {
    new_review(&body)?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, value) = create_pull_review(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, &body, key,
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

/// Reviews a pull request as `actor_id` at the commit the reviewer saw,
/// which must still be the head. Approving and requesting changes need
/// `pull_request.approve` (never on one's own pull request); a comment-only
/// review needs `pull_request.comment`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_pull_review(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let input = new_review(body)?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/reviews");

    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            let action = if input.verdict == "commented" {
                Action::PullRequestComment
            } else {
                Action::PullRequestApprove
            };
            let pull = load_pull(state, &loaded, number, action).await?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }
            if pull.state != "open" {
                return Err(not_open());
            }
            // A push racing this check leaves a review bound to the commit the
            // reviewer saw, which simply stops counting.
            if branch_tip(state, loaded.repo.id, &pull.source_branch)
                .await?
                .as_ref()
                != Some(&input.commit_id)
            {
                return Err(head_moved());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            // The early `pull.state != "open"` check above races a concurrent
            // close; re-check under the row lock the close/reopen path takes.
            require_open(&mut tx, pull.id).await?;
            let id = Uuid::new_v4();
            let review: ReviewJson = sqlx::query_as(&format!(
                "INSERT INTO pull_request_reviews \
                 (id, pull_request_id, reviewer_id, verdict, commit_id, body) \
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING {REVIEW_COLUMNS}"
            ))
            .bind(id)
            .bind(pull.id)
            .bind(actor_id)
            .bind(&input.verdict)
            .bind(&input.commit_id)
            .bind(&input.body)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE pull_requests SET updated_at = now() WHERE id = $1")
                .bind(pull.id)
                .execute(&mut *tx)
                .await?;
            let kind = match input.verdict.as_str() {
                "approved" => "pull_request.approved",
                "changes_requested" => "pull_request.changes_requested",
                _ => "pull_request.reviewed",
            };
            event(&mut tx, Some(loaded.org_id), actor_id, kind).await?;

            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&review).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await
}

async fn list_reviews(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    Query(query): Query<SeqQuery>,
) -> Result<Json<Page<ReviewJson>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    list_pull_reviews(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, query,
    )
    .await
    .map(Json)
}

/// Every review in order, stale ones included; `commit_id` says which head
/// each was given on.
pub(crate) async fn list_pull_reviews(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    query: SeqQuery,
) -> Result<Page<ReviewJson>, ApiError> {
    let (pull, limit) = seq_page_start(
        state, actor_id, auth_age, org_slug, repo_name, number, &query,
    )
    .await?;
    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, ReviewJson>(&format!(
        "SELECT {REVIEW_COLUMNS} FROM pull_request_reviews WHERE pull_request_id = $1 \
         AND ($2::bigint IS NULL OR seq > $2) ORDER BY seq LIMIT $3"
    ))
    .bind(pull.id)
    .bind(query.after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    Ok(page(rows, limit, |r| r.seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_cannot_name_other_refs_or_expressions() {
        for ok in ["main", "feature/x", "release-1.2", "a_b"] {
            assert!(valid_branch(ok), "{ok}");
        }
        for bad in [
            "",
            "refs/heads/main",
            "-x",
            "a..b",
            "a~1",
            "a^",
            "a@{1}",
            "@",
            "a b",
            "a:b",
            "x.lock",
            "/a",
            "a/",
            "a//b",
            "a/.b",
            ".a",
            "a.",
            "a*",
            "a?",
            "a[",
            "a\\b",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
        assert!(!valid_branch(&"a".repeat(256)));
    }

    #[test]
    fn blob_line_counts_match_text_lines() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"feature\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\nb\n"), 2);
        assert_eq!(line_count(b"a\nb\nc\n"), 3);
    }

    #[test]
    fn comment_anchors_are_all_or_nothing() {
        let id = "a".repeat(40);
        let ok = |v: serde_json::Value| new_comment(v.to_string().as_bytes()).is_ok();
        assert!(ok(serde_json::json!({"body": "x"})));
        assert!(ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a.rs", "line": 3})
        ));
        assert!(!ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a.rs"})
        ));
        assert!(!ok(
            serde_json::json!({"body": "x", "path": "a.rs", "line": 1})
        ));
        assert!(!ok(
            serde_json::json!({"body": "x", "commit_id": id.to_uppercase(), "path": "a", "line": 1})
        ));
        assert!(!ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "../a", "line": 1})
        ));
        assert!(!ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a", "line": 0})
        ));
        assert!(!ok(
            serde_json::json!({"body": " ", "in_reply_to": Uuid::nil()})
        ));
        assert!(!ok(serde_json::json!({
            "body": "x", "commit_id": id, "path": "a", "line": 1, "in_reply_to": Uuid::nil()
        })));
    }

    #[test]
    fn comment_reviews_need_a_body() {
        let id = "b".repeat(64);
        let ok = |v: serde_json::Value| new_review(v.to_string().as_bytes()).is_ok();
        assert!(ok(
            serde_json::json!({"verdict": "approved", "commit_id": id})
        ));
        assert!(ok(
            serde_json::json!({"verdict": "changes_requested", "commit_id": id})
        ));
        assert!(!ok(
            serde_json::json!({"verdict": "commented", "commit_id": id})
        ));
        assert!(ok(
            serde_json::json!({"verdict": "commented", "commit_id": id, "body": "hm"})
        ));
        assert!(!ok(
            serde_json::json!({"verdict": "merged", "commit_id": id})
        ));
        assert!(!ok(
            serde_json::json!({"verdict": "approved", "commit_id": "main"})
        ));
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn require_open_locks_and_rejects_a_closed_pull_request() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("rq-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(format!("o-{}", Uuid::new_v4().simple()))
        .execute(&pool)
        .await
        .unwrap();
        let repo = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
             VALUES ($1,$2,'r',$3,$4,'active','private')",
        )
        .bind(repo)
        .bind(org)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        let pull = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
             VALUES ($1,$2,1,'t','',$3,'src','dst')",
        )
        .bind(pull)
        .bind(repo)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        require_open(&mut conn, pull).await.unwrap();

        sqlx::query("UPDATE pull_requests SET state = 'closed', closed_at = now() WHERE id = $1")
            .bind(pull)
            .execute(&pool)
            .await
            .unwrap();
        let error = require_open(&mut conn, pull).await.unwrap_err();
        assert_eq!(error.status_code(), StatusCode::CONFLICT);
        assert_eq!(error.message(), "pull request is not open");

        // close() waits for every checked-out connection, so return ours first.
        drop(conn);
        pool.close().await;
    }
}
