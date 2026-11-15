//! In-app notifications (ADR 0006 "Settled with Person A").
//!
//! Since M4.1 one outbox consumer enqueues one `notification.outbox` job per
//! event, and this module's handler routes it by event kind. Each notification
//! is idempotent on the **source row** it is about: a merged pull request keys on
//! the merge intent's `operation_id`, a review on its `review_id` and a review
//! request on its `request_id`. The key is stored in `notifications.event_id`
//! and deduplicated with `UNIQUE (event_id, account_id)`, so a redelivered job
//! writes nothing and a second event naming the same source row writes nothing
//! either: two `pull_request.merged` events can name one intent when a journal
//! entry is replayed, and the author is still notified once. Payloads carry only
//! ids, never content; handlers load the current rows when they run, and a
//! deleted pull request or review (or repository) yields `Done` without a row.
//!
//! The three pre-M4.1 job kinds remain registered for one release so a job
//! queued by the previous release still drains after an upgrade; no path
//! enqueues them any more (the event is the only input).

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

/// The job kind the outbox notification consumer enqueues. One handler serves
/// every notification: the event kind in the job payload selects the recipient
/// rule, so one outbox event produces exactly one notification job.
pub const OUTBOX_JOB_KIND: &str = "notification.outbox";

/// The event kinds the notification consumer takes. Review notifications are
/// the `pull_request.reviewed` / `approved` / `changes_requested` events, so the
/// consumer is filtered to the pull-request kinds and not, say, queue events.
pub const OUTBOX_EVENT_KINDS: &[&str] = &[
    "pull_request.merged",
    "pull_request.reviewed",
    "pull_request.approved",
    "pull_request.changes_requested",
    "pull_request.review_requested",
];

/// The consumer registered at start-up; migration `0050` inserts the same row
/// so an upgrade has a registration position from the cutover.
pub fn outbox_consumer() -> crate::outbox::Consumer {
    crate::outbox::Consumer::new("notification.outbox", OUTBOX_JOB_KIND)
        .with_kinds(OUTBOX_EVENT_KINDS)
}

pub fn register(registry: &mut jobs::Registry) {
    registry.register(OUTBOX_JOB_KIND, outbox_handler());
    // Upgrade compatibility only: jobs of these kinds may still be pending from
    // the previous release. Nothing enqueues them now.
    registry.register(
        "notification.pull_request_merged",
        std::sync::Arc::new(|state: AppState, job: Job| {
            Box::pin(async move {
                let event_id = match payload_uuid(&job.payload, "event_id") {
                    Ok(id) => id,
                    Err(failed) => return failed,
                };
                let pull_request_id = match payload_uuid(&job.payload, "pull_request_id") {
                    Ok(id) => id,
                    Err(failed) => return failed,
                };
                merged(&state, event_id, pull_request_id).await
            })
        }),
    );
    registry.register(
        "notification.pull_request_reviewed",
        std::sync::Arc::new(|state: AppState, job: Job| {
            Box::pin(async move {
                let review_id = match payload_uuid(&job.payload, "review_id") {
                    Ok(id) => id,
                    Err(failed) => return failed,
                };
                reviewed(&state, review_id).await
            })
        }),
    );
    registry.register(
        "notification.pull_request_review_requested",
        std::sync::Arc::new(|state: AppState, job: Job| {
            Box::pin(async move {
                let request_id = match payload_uuid(&job.payload, "request_id") {
                    Ok(id) => id,
                    Err(failed) => return failed,
                };
                review_requested(&state, request_id).await
            })
        }),
    );
}

fn outbox_handler() -> jobs::Handler {
    std::sync::Arc::new(|state: AppState, job: Job| {
        Box::pin(async move { from_outbox(&state, &job).await })
    })
}

/// One notification for one outbox event. The recipient is chosen by the
/// event's kind; the notification is keyed on the source row the event is about
/// (the merge intent, the review, the review request), so a redelivered job or a
/// second event naming the same row writes nothing new.
async fn from_outbox(state: &AppState, job: &Job) -> JobOutcome {
    // The event id is validated (the relay is its only writer, and a malformed
    // payload is a permanent failure) but it is not the notification's key.
    if let Err(failed) = payload_uuid(&job.payload, "event_id") {
        return failed;
    }
    let kind = match job.payload.get("event_kind").and_then(Value::as_str) {
        Some(kind) => kind,
        None => {
            return JobOutcome::Failed {
                reason: "missing `kind` in payload".into(),
            };
        }
    };
    // The event's own payload: ids only (ADR 0006).
    let event = job.payload.get("event").cloned().unwrap_or(Value::Null);
    let event_uuid = |key: &str| -> Option<Uuid> {
        event
            .get(key)
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Uuid>().ok())
    };

    match kind {
        "pull_request.merged" => {
            // The event names the repository and verb; the pull request is the
            // merge intent's. A merge without a live intent (for example a
            // pull request closed and merged by the ref move) has no intent and
            // notifies nobody.
            let repository_id = event_uuid("repository_id");
            let Some(operation_id) = event_uuid("operation_id") else {
                return JobOutcome::Done;
            };
            let pull_request_id = match repository_id {
                Some(repository_id) => {
                    match sqlx::query_scalar::<_, Uuid>(
                        "SELECT pull_request_id FROM merge_intents \
                         WHERE operation_id = $1 AND repository_id = $2",
                    )
                    .bind(operation_id)
                    .bind(repository_id)
                    .fetch_optional(&state.pool)
                    .await
                    {
                        Ok(id) => id,
                        Err(_) => return db_retry(),
                    }
                }
                None => None,
            };
            let Some(pull_request_id) = pull_request_id else {
                return JobOutcome::Done;
            };
            // Keyed on the merge intent, not the event: a replayed journal
            // entry writes a second merged event for the same intent.
            merged(state, operation_id, pull_request_id).await
        }
        "pull_request.reviewed" | "pull_request.approved" | "pull_request.changes_requested" => {
            // The review event names its review; without it there is no
            // recipient rule to apply.
            let Some(review_id) = event_uuid("review_id") else {
                return JobOutcome::Done;
            };
            reviewed(state, review_id).await
        }
        "pull_request.review_requested" => {
            // The request id is the notification's key; the pull request,
            // reviewer and requester are read from the request row when the
            // handler runs.
            let Some(request_id) = event_uuid("request_id") else {
                return JobOutcome::Done;
            };
            review_requested(state, request_id).await
        }
        other => JobOutcome::Failed {
            reason: format!("no notification for event kind: {other}"),
        },
    }
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
/// `operation_id` is the notification's key.
async fn merged(state: &AppState, operation_id: Uuid, pull_request_id: Uuid) -> JobOutcome {
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
        operation_id,
        "pull_request.merged",
        pull_request_id,
        author,
        actor,
    )
    .await
}

/// Recipient: the pull request author, unless the reviewer is the author.
async fn reviewed(state: &AppState, review_id: Uuid) -> JobOutcome {
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
async fn review_requested(state: &AppState, request_id: Uuid) -> JobOutcome {
    // The request row holds the pull request, the reviewer and who asked; the
    // handler reads them here rather than trusting a second copy in the event.
    let request: Option<(Uuid, Uuid, Option<Uuid>)> = match sqlx::query_as(
        // This exact request, not merely one for the same reviewer (#86).
        "SELECT pull_request_id, reviewer_id, requested_by \
         FROM pull_request_review_requests WHERE id = $1",
    )
    .bind(request_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(request) => request,
        Err(_) => return db_retry(),
    };
    let Some((pull_request_id, reviewer_id, Some(requested_by))) = request else {
        // The request was removed or the reviewer already submitted a review;
        // a request whose requester's account is gone notifies nobody.
        return JobOutcome::Done;
    };

    // Requesting yourself is not a notification (the other handlers skip
    // self-notifications too).
    if reviewer_id == requested_by {
        return JobOutcome::Done;
    }

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

/// Idempotent insert, keyed on `(event_id, account_id)`. `event_id` is the
/// notification's **stable key**: the id of the source row the event is about
/// (a merge intent, a review, a review request), which the column name predates.
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
