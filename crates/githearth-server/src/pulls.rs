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
use githearth_git::journal::{Origin, Record};
use githearth_git::merge::Identity;
use githearth_git::suggestion;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, browse, event, event_with, facts, idempotency, invalid,
    issues::{allocate_number, not_active, require_readable_repo},
    protection, repositories,
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
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/comments/{comment_id}",
            axum::routing::patch(resolve_comment),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/comments/{comment_id}/apply",
            axum::routing::post(apply_suggestion),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/reviews",
            axum::routing::post(create_review).get(list_reviews),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/requested_reviewers",
            axum::routing::post(add_review_request).get(list_review_requests),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/requested_reviewers/{username}",
            axum::routing::delete(remove_review_request),
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
pub(crate) fn not_open() -> ApiError {
    ApiError(StatusCode::CONFLICT, "pull request is not open")
}
fn already_merged() -> ApiError {
    ApiError(StatusCode::CONFLICT, "pull request is merged")
}

/// Re-checks inside the transaction, on the same row lock the PATCH
/// close/reopen path takes, that the pull request is still open. A close that
/// commits between the earlier head check and the review insert must not leave
/// a review on a closed pull request.
pub(crate) async fn require_open(conn: &mut PgConnection, id: Uuid) -> Result<(), ApiError> {
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

/// Comment columns with the `c.` alias the thread-resolution join needs.
const COMMENT_ROW_COLUMNS: &str = "c.id, c.author_id, c.body, c.seq, c.commit_id, c.path, c.line, \
 c.side, c.in_reply_to, \
 to_char(c.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(c.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, \
 to_char(COALESCE(parent.resolved_at, c.resolved_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS resolved_at, \
 COALESCE(parent.resolved_by, c.resolved_by) AS resolved_by, \
 COALESCE(parent.resolved_at, c.resolved_at) IS NOT NULL AS resolved";

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

/// A pull request with its live head and base, the reviews that count at that
/// head, and the latest status per check context at that head. A deleted branch
/// reads as a null id.
#[derive(Serialize, Deserialize)]
pub(crate) struct PullDetail {
    #[serde(flatten)]
    pub(crate) pull: PullJson,
    pub(crate) head_id: Option<String>,
    pub(crate) base_id: Option<String>,
    pub(crate) approvals: i64,
    pub(crate) changes_requested: i64,
    pub(crate) statuses: Vec<crate::statuses::StatusJson>,
}

/// A comment's stored columns, including the thread resolution computed by the
/// query that loaded it (the root's `resolved_at`, via a self join).
#[derive(sqlx::FromRow)]
struct CommentRow {
    id: Uuid,
    author_id: Uuid,
    body: String,
    seq: i32,
    commit_id: Option<String>,
    path: Option<String>,
    line: Option<i32>,
    side: Option<String>,
    in_reply_to: Option<Uuid>,
    created_at: String,
    updated_at: String,
    resolved_at: Option<String>,
    resolved_by: Option<Uuid>,
    resolved: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CommentJson {
    pub(crate) id: Uuid,
    pub(crate) author_id: Uuid,
    pub(crate) body: String,
    pub(crate) seq: i32,
    pub(crate) commit_id: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) line: Option<i32>,
    pub(crate) side: Option<String>,
    pub(crate) in_reply_to: Option<Uuid>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) resolved_at: Option<String>,
    pub(crate) resolved_by: Option<Uuid>,
    pub(crate) resolved: bool,
    pub(crate) outdated: bool,
    pub(crate) suggestion: Option<String>,
}

/// Enriches a stored row with the live head and its suggestion text. `head` is
/// the current head commit id (None when the source branch is gone).
fn comment_json(row: CommentRow, head: Option<&str>) -> CommentJson {
    let outdated = match (&row.commit_id, head) {
        (Some(commit), Some(head)) => commit != head,
        (Some(_), None) => true,
        (None, _) => false,
    };
    // A suggestion applies to the head (new) side only; an "old"-side comment
    // may carry a ```suggestion block, but the server always refuses to apply
    // it, so no Apply button must be offered for it.
    let suggestion = (row.commit_id.is_some()
        && !row.side.as_deref().is_some_and(|side| side == "old"))
    .then(|| suggestion::suggestion_replacement(&row.body))
    .flatten();
    CommentJson {
        id: row.id,
        author_id: row.author_id,
        body: row.body,
        seq: row.seq,
        commit_id: row.commit_id,
        path: row.path,
        line: row.line,
        side: row.side,
        in_reply_to: row.in_reply_to,
        created_at: row.created_at,
        updated_at: row.updated_at,
        resolved_at: row.resolved_at,
        resolved_by: row.resolved_by,
        resolved: row.resolved,
        outdated,
        suggestion,
    }
}

/// Loads one comment with its thread's resolution (a reply's parent carries
/// the resolution; a root resolves itself).
async fn fetch_comment_row(
    conn: &mut PgConnection,
    comment_id: Uuid,
) -> Result<Option<CommentRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {COMMENT_ROW_COLUMNS} FROM pull_request_comments c \
         LEFT JOIN pull_request_comments parent ON parent.id = c.in_reply_to \
         WHERE c.id = $1"
    ))
    .bind(comment_id)
    .fetch_optional(&mut *conn)
    .await
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

pub(crate) async fn fetch_pull(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<PullJson>, sqlx::Error> {
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
    repository_id: Uuid,
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
    // The latest status per context at the current head, for the pull page.
    let statuses = match &head_id {
        Some(head) => crate::statuses::latest_by_context(conn, repository_id, head).await?,
        None => Vec::new(),
    };
    Ok(PullDetail {
        pull,
        head_id,
        base_id,
        approvals,
        changes_requested,
        statuses,
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
            let body =
                serde_json::to_value(detail(&mut tx, loaded.repo.id, pull, (head, base)).await?)
                    .unwrap();
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
    if !matches!(state_filter, "open" | "closed" | "merged" | "all") {
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
    detail(&mut conn, loaded.repo.id, pull, tips).await
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
            // Merged is terminal: its title and body stay editable, but it can
            // be neither reopened nor closed.
            if locked.state == "merged" && input.state.is_some() {
                return Err(already_merged());
            }
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
            let body =
                serde_json::to_value(detail(&mut tx, loaded.repo.id, pull, tips).await?).unwrap();
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
    side: Option<String>,
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
        // A side names which half of the diff the line is on, only for anchors.
        || input
            .side
            .as_deref()
            .is_some_and(|s| !matches!(s, "old" | "new"))
        || (input.side.is_some() && !anchored)
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
/// the current head; a reply must name a root comment on the same pull request.
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
                let parent_in_reply_to: Option<Option<Uuid>> = sqlx::query_scalar(
                    "SELECT in_reply_to FROM pull_request_comments \
                     WHERE id = $1 AND pull_request_id = $2",
                )
                .bind(parent)
                .bind(pull.id)
                .fetch_optional(&mut *tx)
                .await?;
                match parent_in_reply_to {
                    // The parent must be a comment on this pull request.
                    None => return Err(invalid()),
                    // Replies attach to the thread root, never another reply,
                    // so the display groups one root with its direct replies
                    // and a reply's resolution is its root's.
                    Some(Some(_)) => {
                        return Err(ApiError(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "replies attach to the thread root",
                        ))
                    }
                    Some(None) => {}
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
                 (id, pull_request_id, author_id, body, seq, commit_id, path, line, side, in_reply_to) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(id)
            .bind(pull.id)
            .bind(actor_id)
            .bind(&input.body)
            .bind(seq)
            .bind(&input.commit_id)
            .bind(&input.path)
            .bind(input.line)
            .bind(&input.side)
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

            let comment = fetch_comment_row(&mut tx, id).await?.expect("just inserted");
            let comment = comment_json(comment, input.commit_id.as_deref());
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
) -> Result<(PullJson, Uuid, i64), ApiError> {
    let limit = query.limit.unwrap_or(30);
    if !(1..=100).contains(&limit) || query.after.is_some_and(|a| a < 1) {
        return Err(invalid());
    }
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    let pull = load_pull(state, &loaded, number, Action::PullRequestRead).await?;
    Ok((pull, loaded.repo.id, limit))
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

/// Oldest first, threads flattened; a reply names its parent. Each comment is
/// enriched with its side, its thread's resolution, whether it is outdated
/// (anchored to a commit older than the current head), and its suggestion text.
pub(crate) async fn list_pull_comments(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    query: SeqQuery,
) -> Result<Page<CommentJson>, ApiError> {
    let (pull, repo_id, limit) = seq_page_start(
        state, actor_id, auth_age, org_slug, repo_name, number, &query,
    )
    .await?;
    let head = branch_tip(state, repo_id, &pull.source_branch).await?;
    let head = head.as_deref();
    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, CommentRow>(&format!(
        "SELECT {COMMENT_ROW_COLUMNS} FROM pull_request_comments c \
         LEFT JOIN pull_request_comments parent ON parent.id = c.in_reply_to \
         WHERE c.pull_request_id = $1 AND ($2::bigint IS NULL OR c.seq > $2) \
         ORDER BY c.seq LIMIT $3"
    ))
    .bind(pull.id)
    .bind(query.after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    let rows = rows
        .into_iter()
        .map(|row| comment_json(row, head))
        .collect::<Vec<_>>();
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
            // Any verdict satisfies a pending review request for this reviewer.
            sqlx::query(
                "DELETE FROM pull_request_review_requests \
                 WHERE pull_request_id = $1 AND reviewer_id = $2",
            )
            .bind(pull.id)
            .bind(actor_id)
            .execute(&mut *tx)
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

            crate::jobs::enqueue(
                &mut tx,
                Some(loaded.org_id),
                "notification.pull_request_reviewed",
                serde_json::json!({ "review_id": id }),
                &id.to_string(),
                None,
            )
            .await?;

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
    let (pull, _repo_id, limit) = seq_page_start(
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReviewRequest {
    reviewer: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct ReviewRequestJson {
    reviewer_id: Uuid,
    username: String,
    requested_by: Option<Uuid>,
    created_at: String,
}

const REVIEW_REQUEST_COLUMNS: &str = "rr.reviewer_id, a.username, rr.requested_by, \
 to_char(rr.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

/// The current requested reviewers, oldest first.
async fn list_review_requests_query(
    conn: &mut PgConnection,
    pull_id: Uuid,
) -> Result<Vec<ReviewRequestJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {REVIEW_REQUEST_COLUMNS} FROM pull_request_review_requests rr \
         JOIN accounts a ON a.id = rr.reviewer_id \
         WHERE rr.pull_request_id = $1 ORDER BY rr.created_at"
    ))
    .bind(pull_id)
    .fetch_all(&mut *conn)
    .await
}

/// Whether `facts` allow `action` on the pull request built exactly as
/// `require_pull` builds it. `None` facts mean the account is not resolvable in
/// the organization, so it is not allowed.
fn reviewer_allowed(
    action: Action,
    state: &AppState,
    loaded: &Loaded,
    pull: &PullJson,
    facts: Option<&facts::Facts>,
) -> bool {
    let Some(facts) = facts else {
        return false;
    };
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let entity = PullRequest {
        id: pull.id,
        repository: loaded.repo.id,
        author: pull.author_id,
        target: Ref {
            repository: loaded.repo.id,
            name: format!("refs/heads/{}", pull.target_branch),
            protection: None,
            exists: true,
        },
    };
    state
        .authz
        .authorize(&facts.request(action, Resource::PullRequest(&authz_repo, &entity), None))
        .is_allowed()
}

async fn list_review_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<Vec<ReviewRequestJson>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    // Read access alone suffices, matching list_reviews.
    require_readable_repo(&loaded)?;
    let pull = load_pull(&state, &loaded, number, Action::PullRequestRead).await?;
    let mut conn = state.pool.acquire().await?;
    Ok(Json(list_review_requests_query(&mut conn, pull.id).await?))
}

/// Permission shared by review requests and thread resolution: the caller
/// must be allowed `PullRequestComment` on the pull, and be either the author
/// or allowed `PullRequestApprove`. A reader who can merely comment is not
/// enough to resolve a thread or to request a reviewer.
pub(crate) fn require_review_permission(
    state: &AppState,
    loaded: &Loaded,
    pull: &PullJson,
) -> Result<(), ApiError> {
    require_pull(state, loaded, pull, Action::PullRequestComment)?;
    let is_author = loaded.actor_id == pull.author_id;
    // Approve is a routine action, so the auth age does not change the decision.
    let can_approve = reviewer_allowed(
        Action::PullRequestApprove,
        state,
        loaded,
        pull,
        Some(&loaded.facts),
    );
    if !is_author && !can_approve {
        return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
    }
    Ok(())
}

async fn add_review_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<Json<Vec<ReviewRequestJson>>, ApiError> {
    let input: NewReviewRequest = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    let loaded =
        repository_api::load_current(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    let pull = load_pull(&state, &loaded, number, Action::PullRequestComment).await?;
    require_review_permission(&state, &loaded, &pull)?;
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_active());
    }
    if pull.state != "open" {
        return Err(not_open());
    }

    // Resolve the reviewer by username.
    let mut conn = state.pool.acquire().await?;
    let reviewer_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
            .bind(&input.reviewer)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(reviewer_id) = reviewer_id else {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reviewer not found",
        ));
    };
    if reviewer_id == pull.author_id {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the author cannot review their own pull request",
        ));
    }
    drop(conn);

    let mut conn = state.pool.acquire().await?;
    let reviewer_facts = facts::load(&mut conn, reviewer_id, loaded.org_id).await?;
    drop(conn);
    if !reviewer_allowed(
        Action::PullRequestApprove,
        &state,
        &loaded,
        &pull,
        reviewer_facts.as_ref(),
    ) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reviewer cannot approve this pull request",
        ));
    }

    let mut tx = state.pool.begin().await?;
    if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
        return Err(not_active());
    }
    require_open(&mut tx, pull.id).await?;

    let request_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO pull_request_review_requests (pull_request_id, reviewer_id, requested_by, id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (pull_request_id, reviewer_id) DO NOTHING",
    )
    .bind(pull.id)
    .bind(reviewer_id)
    .bind(actor.id)
    .bind(request_id)
    .execute(&mut *tx)
    .await?;

    if inserted.rows_affected() > 0 {
        event_with(
            &mut tx,
            Some(loaded.org_id),
            actor.id,
            "pull_request.review_requested",
            serde_json::json!({ "pull_request_id": pull.id, "reviewer_id": reviewer_id }),
        )
        .await?;
        crate::jobs::enqueue(
            &mut tx,
            Some(loaded.org_id),
            "notification.pull_request_review_requested",
            serde_json::json!({
                "request_id": request_id,
                "pull_request_id": pull.id,
                "reviewer_id": reviewer_id,
                "requested_by": actor.id,
            }),
            &request_id.to_string(),
            None,
        )
        .await?;
    }

    let list = list_review_requests_query(&mut tx, pull.id).await?;
    tx.commit().await?;
    Ok(Json(list))
}

async fn remove_review_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number, username)): Path<(String, String, i32, String)>,
) -> Result<StatusCode, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;

    let loaded =
        repository_api::load_current(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    let pull = load_pull(&state, &loaded, number, Action::PullRequestComment).await?;
    require_review_permission(&state, &loaded, &pull)?;

    let mut tx = state.pool.begin().await?;
    let reviewer_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
            .bind(&username)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(reviewer_id) = reviewer_id else {
        return Err(ApiError(StatusCode::NOT_FOUND, "review request not found"));
    };

    let deleted = sqlx::query(
        "DELETE FROM pull_request_review_requests WHERE pull_request_id = $1 AND reviewer_id = $2",
    )
    .bind(pull.id)
    .bind(reviewer_id)
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "review request not found"));
    }

    event_with(
        &mut tx,
        Some(loaded.org_id),
        actor.id,
        "pull_request.review_request_removed",
        serde_json::json!({ "pull_request_id": pull.id, "reviewer_id": reviewer_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvePatch {
    resolved: bool,
}

async fn resolve_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number, comment_id)): Path<(String, String, i32, Uuid)>,
    body: Bytes,
) -> Result<Json<CommentJson>, ApiError> {
    let input: ResolvePatch = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    set_thread_resolved(
        &state,
        actor.id,
        auth_age,
        &org_slug,
        &repo_name,
        number,
        comment_id,
        input.resolved,
    )
    .await
    .map(Json)
}

/// Resolves or unresolves the thread `comment_id` belongs to, returning the
/// target comment enriched like a list entry. Resolution lives on the thread
/// root; a reply's parent carries it. The caller must be the pull request
/// author or able to approve.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn set_thread_resolved(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    comment_id: Uuid,
    resolved: bool,
) -> Result<CommentJson, ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    let pull = load_pull(state, &loaded, number, Action::PullRequestComment).await?;
    // Resolving a thread is a review action: the author or an approver, not a
    // member who can merely read and comment.
    require_review_permission(state, &loaded, &pull)?;
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_active());
    }
    if pull.state != "open" {
        return Err(not_open());
    }
    let head = branch_tip(state, loaded.repo.id, &pull.source_branch).await?;
    let head = head.as_deref();

    let mut tx = state.pool.begin().await?;
    if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
        return Err(not_active());
    }
    // The comment must be on this pull request; the thread root is its
    // topmost ancestor (walking `in_reply_to` up).
    let root_id: Option<Uuid> = sqlx::query_scalar(
        "WITH RECURSIVE thread AS ( \
           SELECT id, in_reply_to FROM pull_request_comments \
           WHERE id = $1 AND pull_request_id = $2 \
           UNION ALL \
           SELECT c.id, c.in_reply_to FROM pull_request_comments c \
           JOIN thread t ON c.id = t.in_reply_to \
         ) \
         SELECT id FROM thread WHERE in_reply_to IS NULL",
    )
    .bind(comment_id)
    .bind(pull.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(root_id) = root_id else {
        return Err(ApiError(StatusCode::NOT_FOUND, "comment not found"));
    };
    if resolved {
        // The first resolution wins; resolving again keeps the original actor.
        sqlx::query(
            "UPDATE pull_request_comments \
             SET resolved_at = COALESCE(resolved_at, now()), resolved_by = COALESCE(resolved_by, $1) \
             WHERE id = $2 AND in_reply_to IS NULL",
        )
        .bind(actor_id)
        .bind(root_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE pull_request_comments SET resolved_at = NULL, resolved_by = NULL \
             WHERE id = $1 AND in_reply_to IS NULL",
        )
        .bind(root_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE pull_requests SET updated_at = now() WHERE id = $1")
        .bind(pull.id)
        .execute(&mut *tx)
        .await?;
    event(
        &mut tx,
        Some(loaded.org_id),
        actor_id,
        if resolved {
            "pull_request.comment_resolved"
        } else {
            "pull_request.comment_unresolved"
        },
    )
    .await?;

    let row = fetch_comment_row(&mut tx, comment_id)
        .await?
        .expect("the target comment exists");
    tx.commit().await?;
    Ok(comment_json(row, head))
}

/// Whether `actor_id` may apply a suggestion: anyone allowed `ref.push` on the
/// source branch (including branch protection, exactly as a push would be
/// judged). The pull request author has no special path — an author who cannot
/// push to the head branch (read-only access, or a protected branch) may not
/// apply.
/// The shared authorization check for applying a suggestion: `ref.push` on the
/// source branch with its matching protection. A denial maps to 403 when the
/// viewer can still read the repository and to 404 otherwise.
fn apply_permission_decision(
    state: &AppState,
    facts: &facts::Facts,
    repo: &repositories::RepositoryRow,
    pull: &PullJson,
    rules: &[protection::ProtectionRule],
    auth_age: u64,
) -> Result<(), ApiError> {
    let ref_name = format!("refs/heads/{}", pull.source_branch);
    let reference = Ref {
        repository: repo.id,
        name: ref_name.clone(),
        protection: protection::matching(rules, &ref_name),
        exists: true,
    };
    let authz_repo = repositories::authz_repository(repo);
    let decision = state.authz.authorize(&facts.request(
        Action::RefPush,
        Resource::Ref(&authz_repo, &reference),
        Some(Duration::from_secs(auth_age)),
    ));
    if decision.is_allowed() {
        return Ok(());
    }
    let read_ok = state
        .authz
        .authorize(&facts.request(
            Action::RepositoryRead,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    if read_ok {
        Err(ApiError(StatusCode::FORBIDDEN, "not permitted"))
    } else {
        Err(not_found())
    }
}

pub(crate) async fn require_apply_permission(
    state: &AppState,
    loaded: &Loaded,
    pull: &PullJson,
) -> Result<(), ApiError> {
    let mut conn = state.pool.acquire().await?;
    let rules = protection::load_rules(&mut conn, loaded.repo.id).await?;
    drop(conn);
    apply_permission_decision(
        state,
        &loaded.facts,
        &loaded.repo,
        pull,
        &rules,
        loaded.auth_age,
    )
}

/// Re-checks apply permission under the repository lock, reloading facts and
/// rules so a protection rule added or a grant revoked between the early check
/// and the ref update is not missed.
pub(crate) async fn require_apply_permission_locked(
    state: &AppState,
    loaded: &Loaded,
    pull: &PullJson,
) -> Result<(), ApiError> {
    let mut conn = state.pool.acquire().await?;
    let rules = protection::load_rules(&mut conn, loaded.repo.id).await?;
    let facts = facts::load(&mut conn, loaded.actor_id, loaded.org_id)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    apply_permission_decision(state, &facts, &loaded.repo, pull, &rules, loaded.auth_age)
}

/// What the pull request page may render for the current viewer, mirroring the
/// permission each matching handler checks, so a control is never shown for an
/// action the server would refuse with 403.
pub(crate) struct ViewerAbilities {
    pub(crate) may_comment: bool,
    pub(crate) may_resolve: bool,
    pub(crate) may_apply: bool,
}

/// Loads the viewer's abilities for the browser pull page. Commenting maps to
/// [`Action::PullRequestComment`], resolving to [`require_review_permission`]
/// and applying to [`require_apply_permission`].
pub(crate) async fn viewer_abilities(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
) -> Result<ViewerAbilities, ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let pull = find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    let may_comment = require_pull(state, &loaded, &pull, Action::PullRequestComment).is_ok();
    let may_resolve = require_review_permission(state, &loaded, &pull).is_ok();
    let may_apply = require_apply_permission(state, &loaded, &pull)
        .await
        .is_ok();
    Ok(ViewerAbilities {
        may_comment,
        may_resolve,
        may_apply,
    })
}

fn suggestion_not_applyable(message: &'static str) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, message)
}

/// Maps the git errors of building a suggestion commit to API errors; the ref
/// compare-and-swap's `RefMoved` is handled separately as [`head_moved`].
fn suggestion_storage_error(error: githearth_git::StorageError) -> ApiError {
    match error {
        githearth_git::StorageError::Invalid(_) => {
            suggestion_not_applyable("the suggestion no longer applies to the file")
        }
        githearth_git::StorageError::TooLarge { .. } => {
            ApiError(StatusCode::PAYLOAD_TOO_LARGE, "suggestion is too large")
        }
        githearth_git::StorageError::Timeout => {
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "git operation timed out")
        }
        _ => ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage operation failed",
        ),
    }
}

async fn apply_suggestion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number, comment_id)): Path<(String, String, i32, Uuid)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let value = apply_pull_suggestion(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, comment_id,
    )
    .await?;
    Ok(Json(value))
}

/// Applies the ```suggestion block of `comment_id` as a commit on the pull
/// request's head branch, through the journaled compare-and-swap ref update.
/// Fails cleanly (409) if the head moved. Returns the new commit id.
pub(crate) async fn apply_pull_suggestion(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    comment_id: Uuid,
) -> Result<serde_json::Value, ApiError> {
    let loaded =
        repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name).await?;
    require_readable_repo(&loaded)?;
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_active());
    }
    let mut conn = state.pool.acquire().await?;
    let pull = find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    drop(conn);
    if pull.state != "open" {
        return Err(not_open());
    }

    let mut conn = state.pool.acquire().await?;
    let comment = fetch_comment_row(&mut conn, comment_id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "comment not found"))?;
    drop(conn);
    let (Some(commit_id), Some(path), Some(line)) =
        (&comment.commit_id, &comment.path, comment.line)
    else {
        return Err(suggestion_not_applyable(
            "comment is not anchored to a line",
        ));
    };
    // Only a new-side comment can carry an applied suggestion.
    if comment.side.as_deref().is_some_and(|side| side == "old") {
        return Err(suggestion_not_applyable(
            "a suggestion cannot apply to the base side",
        ));
    }
    let Some(replacement) = suggestion::suggestion_replacement(&comment.body) else {
        return Err(suggestion_not_applyable("comment has no suggestion"));
    };

    require_apply_permission(state, &loaded, &pull).await?;

    // Serialize with every other ref writer on this repository, held for the
    // whole mutation.
    let _guard = state
        .storage
        .repository_lock(loaded.repo.id)
        .lock_owned()
        .await;
    // Re-check under the lock: a protection rule added or a grant revoked
    // between the early check and now must not let a stale permission through.
    require_apply_permission_locked(state, &loaded, &pull).await?;
    let repo_path = state.storage.repository_path(loaded.repo.id);
    let hooks_dir = state.config.storage_root.join("hooks");
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);

    // The expected old oid is the current head; the compare-and-swap refuses
    // the update if it moves between this read and the ref update.
    let head = branch_tip(state, loaded.repo.id, &pull.source_branch)
        .await?
        .ok_or_else(branch_missing)?;
    // A suggestion applies to the exact commit it commented; once the head has
    // moved the comment is outdated and must not rewrite the current line.
    if commit_id.as_str() != head.as_str() {
        return Err(head_moved());
    }

    let (actor_username, comment_author_username) = {
        let mut conn = state.pool.acquire().await?;
        let actor_username: String =
            sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
                .bind(actor_id)
                .fetch_one(&mut *conn)
                .await?;
        let comment_author_username: String =
            sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
                .bind(comment.author_id)
                .fetch_one(&mut *conn)
                .await?;
        (actor_username, comment_author_username)
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let identity = Identity {
        name: actor_username.clone(),
        email: format!("{actor_username}@users.noreply.githearth.invalid"),
        time: now,
        offset: "+0000".into(),
    };
    let message = format!("Apply suggestion from @{comment_author_username}\n");

    let operation = Uuid::new_v4();
    let build = {
        let repo = repo_path.clone();
        let head = head.clone();
        let path = path.clone();
        let replacement = replacement.into_bytes();
        let message = message.into_bytes();
        let identity = identity.clone();
        tokio::task::spawn_blocking(move || {
            suggestion::apply_suggestion(
                &repo,
                &head,
                &path,
                line as usize,
                &replacement,
                &message,
                &identity,
                &identity,
                timeout,
            )
        })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage operation failed",
            )
        })?
    };
    let new_oid = build.map_err(suggestion_storage_error)?;

    let update = {
        let repo = repo_path.clone();
        let reference = format!("refs/heads/{}", pull.source_branch);
        let new_oid = new_oid.clone();
        let head = head.clone();
        tokio::task::spawn_blocking(move || {
            githearth_git::merge::update_ref_expected(
                &repo,
                &hooks_dir,
                &reference,
                &new_oid,
                &head,
                operation,
                actor_id,
                Origin::Suggestion,
                timeout,
            )
        })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage operation failed",
            )
        })?
    };
    match update {
        Ok(()) => {}
        Err(githearth_git::StorageError::RefMoved(_)) => return Err(head_moved()),
        Err(error) => return Err(suggestion_storage_error(error)),
    }

    // Record the push + audit event from the journal; the reconciler retries.
    if let Err(error) = crate::pushes::replay(state, loaded.repo.id).await {
        tracing::warn!(
            repository_id = %loaded.repo.id,
            error = %error,
            "post-suggestion journal replay failed; reconciler will retry"
        );
    }

    Ok(serde_json::json!({ "commit_id": new_oid }))
}

/// Records an applied suggestion whose ref move was journaled with
/// `Origin::Suggestion`. Idempotent by `(repository_id, journal_sequence)`.
pub(crate) async fn apply_suggestion_record(
    conn: &mut PgConnection,
    repository: Uuid,
    prepared: &Record,
) -> Result<(), crate::pushes::ReplayError> {
    use crate::pushes::ReplayError;
    let [update] = prepared.updates.as_slice() else {
        return Err(ReplayError::Invalid(
            "suggestion record must contain exactly one ref update",
        ));
    };
    let actor = prepared
        .actor
        .ok_or(ReplayError::Invalid("suggestion record actor is required"))?;
    if !crate::pushes::valid_oid(&update.old) || !crate::pushes::valid_oid(&update.new) {
        return Err(ReplayError::Invalid("invalid OID in suggestion record"));
    }
    if update.old.len() != update.new.len() {
        return Err(ReplayError::Invalid("old and new OIDs differ in length"));
    }
    if !crate::pushes::valid_ref_name(&update.reference) {
        return Err(ReplayError::Invalid(
            "ref must start with refs/heads/ or refs/tags/",
        ));
    }

    let sequence = prepared.sequence as i64;
    let mut tx = conn.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO repository_pushes (repository_id, journal_sequence, operation_id, actor_id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(repository)
    .bind(sequence)
    .bind(prepared.operation)
    .bind(actor)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if inserted {
        sqlx::query(
            "INSERT INTO ref_updates (repository_id, journal_sequence, ref_name, old_oid, new_oid) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(repository)
        .bind(sequence)
        .bind(&update.reference)
        .bind(&update.old)
        .bind(&update.new)
        .execute(&mut *tx)
        .await?;
        let organization_id: Option<Uuid> =
            sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id = $1")
                .bind(repository)
                .fetch_one(&mut *tx)
                .await?;
        event_with(
            &mut tx,
            organization_id,
            actor,
            "pull_request.suggestion_applied",
            serde_json::json!({
                "repository_id": repository,
                "journal_sequence": sequence,
                "operation_id": prepared.operation,
                "ref": update.reference,
                "old": update.old,
                "new": update.new,
            }),
        )
        .await?;
        // A suggestion commit is also a push: emit the same `repository.pushed`
        // audit + outbox pair a normal push emits, so push webhooks fire.
        let audit_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_events (id, organization_id, actor_id, kind) \
             SELECT $1, r.organization_id, $2, 'repository.pushed' \
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(actor)
        .bind(repository)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO outbox_events (id, organization_id, kind, payload) \
             SELECT $1, r.organization_id, 'repository.pushed', $2 \
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(serde_json::json!({
            "repository_id": repository,
            "journal_sequence": sequence,
            "operation_id": prepared.operation,
            "actor_id": actor,
            "credential_kind": null,
            "credential_id": null,
            "updates": [{
                "ref": update.reference,
                "old": update.old,
                "new": update.new,
            }],
        }))
        .bind(repository)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
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
    fn comment_side_is_old_or_new_and_anchored_only() {
        let id = "a".repeat(40);
        let ok = |v: serde_json::Value| new_comment(v.to_string().as_bytes()).is_ok();
        assert!(ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a.rs", "line": 3, "side": "new"})
        ));
        assert!(ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a.rs", "line": 3, "side": "old"})
        ));
        // A side without an anchor is invalid.
        assert!(!ok(serde_json::json!({"body": "x", "side": "new"})));
        // An unknown side is invalid.
        assert!(!ok(
            serde_json::json!({"body": "x", "commit_id": id, "path": "a.rs", "line": 3, "side": "both"})
        ));
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

    #[test]
    fn suggestion_storage_errors_map_to_api_statuses() {
        use githearth_git::StorageError;

        let invalid = suggestion_storage_error(StorageError::Invalid("x".into()));
        assert_eq!(invalid.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            invalid.message(),
            "the suggestion no longer applies to the file"
        );

        let too_large = suggestion_storage_error(StorageError::TooLarge { size: 1, limit: 0 });
        assert_eq!(too_large.status_code(), StatusCode::PAYLOAD_TOO_LARGE);

        let timeout = suggestion_storage_error(StorageError::Timeout);
        assert_eq!(timeout.status_code(), StatusCode::SERVICE_UNAVAILABLE);

        let other = suggestion_storage_error(StorageError::Git("boom".into()));
        assert_eq!(other.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
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

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn apply_permission_is_rechecked_under_the_lock() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("al-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        let org_slug = format!("o-{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
        )
        .bind(org)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
        let grant_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
             VALUES ($1,'write','account',$2,'organization',$3,$2)",
        )
        .bind(grant_id)
        .bind(account)
        .bind(org)
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

        let storage_root =
            std::env::temp_dir().join(format!("githearth-apply-lock-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&storage_root).unwrap();
        let config = crate::Config {
            database_url: url,
            database_max_connections: 4,
            database_timeout_seconds: 3,
            storage_root: storage_root.clone(),
            ..crate::Config::default()
        };
        let state = crate::AppState::new(pool.clone(), config).await.unwrap();

        let loaded = repository_api::load_repo(&state, account, 0, &org_slug, "r")
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let pull = find_pull(&mut conn, repo, 1).await.unwrap().unwrap();
        drop(conn);

        // The write grant admits the early check.
        require_apply_permission(&state, &loaded, &pull)
            .await
            .unwrap();

        // Revoke the write grant and leave only read access. The early check's
        // facts are stale, but the under-lock re-check reloads them and refuses.
        sqlx::query(
            "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 WHERE id = $2",
        )
        .bind(account)
        .bind(grant_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
             VALUES ($1,'read','account',$2,'organization',$3,$2)",
        )
        .bind(Uuid::new_v4())
        .bind(account)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

        let error = require_apply_permission_locked(&state, &loaded, &pull)
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), StatusCode::FORBIDDEN);

        std::fs::remove_dir_all(&storage_root).unwrap();
        pool.close().await;
    }
}
