//! In-app notifications (ADR 0006 "Settled with Person A").
//!
//! The first two real job handlers. Each is idempotent on a stable key: a
//! merged pull request keys its notification on the audit event id, a review on
//! the review id. Delivery is at-least-once, so a second run inserts nothing
//! (`ON CONFLICT (event_id, account_id) DO NOTHING`). Payloads carry only ids,
//! never content; handlers load the current rows when they run, and a deleted
//! pull request or review (or repository) yields `Done` without a row.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Credential, authenticate_token_hash, bearer_credential,
    jobs::{self, Job, JobOutcome},
};

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/user/notifications", get(list))
        .route(
            "/api/v1/user/notifications/{id}/read",
            axum::routing::post(mark_read),
        )
}

/// Authenticates a session only; an access token cannot read notifications.
async fn session_auth(pool: &PgPool, headers: &HeaderMap) -> ApiResult<(crate::Account, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot view notifications",
        )),
    }
}

pub fn register(registry: &mut jobs::Registry) {
    registry.register("notification.pull_request_merged", merged_handler());
    registry.register("notification.pull_request_reviewed", reviewed_handler());
    registry.register(
        "notification.pull_request_review_requested",
        review_requested_handler(),
    );
}

fn merged_handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: Job| {
        Box::pin(async move { merged(&state, &job).await })
    })
}

fn reviewed_handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: Job| {
        Box::pin(async move { reviewed(&state, &job).await })
    })
}

fn review_requested_handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: Job| {
        Box::pin(async move { review_requested(&state, &job).await })
    })
}

/// Reads a required UUID id from a job payload; a missing or malformed id is
/// a permanent failure (the enqueuer's data is wrong), not a retry.
fn payload_uuid(payload: &Value, key: &str) -> Result<Uuid, JobOutcome> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(|| JobOutcome::Failed {
            reason: format!("missing or invalid `{key}` in payload"),
        })
}

/// Recipient: the pull request author, unless the author merged it themself.
async fn merged(state: &AppState, job: &Job) -> JobOutcome {
    let event_id = match payload_uuid(&job.payload, "event_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };
    let pull_request_id = match payload_uuid(&job.payload, "pull_request_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };

    let result = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT author_id, merged_by FROM pull_requests WHERE id = $1",
    )
    .bind(pull_request_id)
    .fetch_optional(&state.pool)
    .await;

    let (author, actor) = match result {
        Ok(Some((author, actor))) => (author, actor),
        // The pull request no longer exists: nothing to notify.
        Ok(None) => return JobOutcome::Done,
        Err(_) => return db_retry(),
    };

    if actor == Some(author) {
        // Self-merge: no notification.
        return JobOutcome::Done;
    }

    insert_notification(
        state,
        event_id,
        "pull_request.merged",
        pull_request_id,
        author,
        actor,
    )
    .await
}

/// Recipient: the pull request author, unless the reviewer is the author.
async fn reviewed(state: &AppState, job: &Job) -> JobOutcome {
    let review_id = match payload_uuid(&job.payload, "review_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };

    let result = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT pull_request_id, reviewer_id FROM pull_request_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_optional(&state.pool)
    .await;

    let (pull_request_id, reviewer) = match result {
        Ok(Some(row)) => row,
        // The review no longer exists.
        Ok(None) => return JobOutcome::Done,
        Err(_) => return db_retry(),
    };

    let author: Option<Uuid> =
        match sqlx::query_scalar("SELECT author_id FROM pull_requests WHERE id = $1")
            .bind(pull_request_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(author) => author,
            Err(_) => return db_retry(),
        };
    let Some(author) = author else {
        // The pull request no longer exists.
        return JobOutcome::Done;
    };

    if author == reviewer {
        // Self-review: no notification.
        return JobOutcome::Done;
    }

    insert_notification(
        state,
        review_id,
        "pull_request.reviewed",
        pull_request_id,
        author,
        Some(reviewer),
    )
    .await
}

/// Recipient: the requested reviewer, unless the request has since gone away
/// (removed manually, or satisfied by a submitted review).
async fn review_requested(state: &AppState, job: &Job) -> JobOutcome {
    let request_id = match payload_uuid(&job.payload, "request_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };
    let pull_request_id = match payload_uuid(&job.payload, "pull_request_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };
    let reviewer_id = match payload_uuid(&job.payload, "reviewer_id") {
        Ok(id) => id,
        Err(failed) => return failed,
    };

    let still_exists: bool = match sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pull_request_review_requests \
         WHERE pull_request_id = $1 AND reviewer_id = $2)",
    )
    .bind(pull_request_id)
    .bind(reviewer_id)
    .fetch_one(&state.pool)
    .await
    {
        Ok(exists) => exists,
        Err(_) => return db_retry(),
    };
    if !still_exists {
        // The request was removed or the reviewer already submitted a review.
        return JobOutcome::Done;
    }

    let requested_by = match payload_uuid(&job.payload, "requested_by") {
        Ok(id) => id,
        Err(failed) => return failed,
    };

    insert_notification(
        state,
        request_id,
        "pull_request.review_requested",
        pull_request_id,
        reviewer_id,
        Some(requested_by),
    )
    .await
}

fn db_retry() -> JobOutcome {
    JobOutcome::Retry {
        after: std::time::Duration::ZERO,
        reason: "database error".into(),
    }
}

/// Idempotent insert, keyed on `(event_id, account_id)`.
async fn insert_notification(
    state: &AppState,
    event_id: Uuid,
    kind: &str,
    pull_request_id: Uuid,
    account_id: Uuid,
    actor_id: Option<Uuid>,
) -> JobOutcome {
    match sqlx::query(
        "INSERT INTO notifications (id, account_id, kind, event_id, pull_request_id, actor_id) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (event_id, account_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(kind)
    .bind(event_id)
    .bind(pull_request_id)
    .bind(actor_id)
    .execute(&state.pool)
    .await
    {
        Ok(_) => JobOutcome::Done,
        Err(_) => db_retry(),
    }
}

const NOTIFICATION_COLUMNS: &str = "n.id, n.kind, n.pull_request_id, a.username AS actor_username, \
 to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(n.read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS read_at";

#[derive(Serialize, sqlx::FromRow)]
struct NotificationJson {
    id: Uuid,
    kind: String,
    pull_request_id: Uuid,
    actor_username: Option<String>,
    created_at: String,
    read_at: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    unread: Option<bool>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<NotificationJson>>> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    let unread = query.unread;
    let sql = format!(
        "SELECT {NOTIFICATION_COLUMNS} FROM notifications n \
         LEFT JOIN accounts a ON a.id = n.actor_id \
         WHERE n.account_id = $1 AND ($2::boolean IS NOT TRUE OR n.read_at IS NULL) \
         ORDER BY n.created_at DESC, n.id DESC LIMIT 50"
    );
    let rows = sqlx::query_as::<_, NotificationJson>(&sql)
        .bind(account.id)
        .bind(unread)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(rows))
}

async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    let result = sqlx::query(
        // The first read time wins; re-marking a read notification keeps it (#70).
        "UPDATE notifications SET read_at = COALESCE(read_at, clock_timestamp()) \
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(account.id)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "notification not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
