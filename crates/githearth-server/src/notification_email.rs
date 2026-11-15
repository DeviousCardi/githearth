//! Email notification preferences and delivery (M4.1 U08).
//!
//! The notification consumer writes one `email_intents` row per notification it
//! would mail, in the same transaction as the in-app row, and then sends from
//! that row inside the same durable `notification.outbox` job. The outbox
//! delivery row is therefore the retry/dead-letter record for email too: no
//! second job kind and no second retry path.
//!
//! Email is only ever sent when all of these are true:
//!  * the account has verified its address (`email_verified_at` is not null);
//!  * the preference for this kind is not muted (a missing row means enabled);
//!  * no earlier attempt delivered this intent (`sent_at` is set once).
//!
//! A muted kind or an unverified address writes no intent at all, so there is
//! nothing queued to discard. Two deliveries of the same notification may run
//! at once; a short `claim_expires_at` lease makes one of them the sender.
//! Every message is plain text built here from ids and safe names: no token, no
//! secret and no other account's data.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Credential, authenticate_token_hash, bearer_credential, event_with,
    jobs::JobOutcome, mail::EmailMessage,
};

type ApiResult<T> = Result<T, ApiError>;

/// The kinds that can be delivered by email. These are the notification kinds
/// the consumer writes; account mail (verification, reset) is transactional and
/// is not subject to these preferences.
pub const EMAILABLE_KINDS: &[&str] = &[
    "pull_request.merged",
    "pull_request.reviewed",
    "pull_request.review_requested",
];

pub fn is_emailable(kind: &str) -> bool {
    EMAILABLE_KINDS.contains(&kind)
}

pub(crate) fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/user/notification-preferences",
        get(list_preferences).put(update_preferences),
    )
}

/// Session only: an access token cannot change where mail is sent.
async fn session_account(pool: &PgPool, headers: &HeaderMap) -> ApiResult<crate::Account> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => {
            Ok(authenticate_token_hash(pool, token_hash).await?.0)
        }
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot manage notification preferences",
        )),
    }
}

#[derive(Serialize)]
struct PreferenceJson {
    kind: String,
    email: bool,
}

/// The preference model in one place: a row is a choice, absence is enabled.
/// Reading and writing both go through this module, and the send path asks the
/// same function the API does.
async fn preference_enabled(
    conn: &mut PgConnection,
    account_id: Uuid,
    kind: &str,
) -> Result<bool, sqlx::Error> {
    let value: Option<bool> = sqlx::query_scalar(
        "SELECT email_enabled FROM notification_preferences WHERE account_id = $1 AND kind = $2",
    )
    .bind(account_id)
    .bind(kind)
    .fetch_optional(conn)
    .await?;
    Ok(value.unwrap_or(true))
}

async fn preferences(pool: &PgPool, account_id: Uuid) -> Result<Vec<PreferenceJson>, sqlx::Error> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT kind, email_enabled FROM notification_preferences WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    let stored: BTreeMap<String, bool> = rows.into_iter().collect();
    Ok(EMAILABLE_KINDS
        .iter()
        .map(|kind| PreferenceJson {
            kind: (*kind).to_string(),
            email: stored.get(*kind).copied().unwrap_or(true),
        })
        .collect())
}

async fn list_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<PreferenceJson>>> {
    let account = session_account(&state.pool, &headers).await?;
    Ok(Json(
        preferences(&state.pool, account.id)
            .await
            .map_err(ApiError::from)?,
    ))
}

/// A map of kind to whether email is enabled. Unknown kinds are refused as
/// input, so a typo cannot silently create a preference nobody reads.
type PreferenceUpdate = BTreeMap<String, bool>;

async fn update_preferences(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreferenceUpdate>,
) -> ApiResult<Json<Vec<PreferenceJson>>> {
    let account = session_account(&state.pool, &headers).await?;
    if input
        .keys()
        .any(|kind| !EMAILABLE_KINDS.contains(&kind.as_str()))
    {
        return Err(crate::invalid());
    }
    let rows = {
        let mut tx = state.pool.begin().await?;
        for (kind, enabled) in &input {
            sqlx::query(
                "INSERT INTO notification_preferences (account_id, kind, email_enabled, updated_at) \
                 VALUES ($1, $2, $3, now()) \
                 ON CONFLICT (account_id, kind) DO UPDATE SET \
                     email_enabled = EXCLUDED.email_enabled, updated_at = now()",
            )
            .bind(account.id)
            .bind(kind)
            .bind(enabled)
            .execute(&mut *tx)
            .await?;
        }
        if !input.is_empty() {
            event_with(
                &mut tx,
                None,
                account.id,
                "account.notification_preferences_changed",
                serde_json::json!({"account_id": account.id, "preferences": input}),
            )
            .await
            .map_err(ApiError::from)?;
        }
        tx.commit().await?;
        preferences(&state.pool, account.id)
            .await
            .map_err(ApiError::from)?
    };
    Ok(Json(rows))
}

/// Writes the durable intent for one notification, if this account wants this
/// kind by email and has a verified address. The caller has already inserted
/// the notification in this transaction. `ON CONFLICT (notification_id)` makes
/// the row the one and only email intent for that notification, whatever
/// event or delivery produced it.
pub(crate) async fn queue(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    notification_id: Uuid,
    account_id: Uuid,
    kind: &str,
) -> Result<(), sqlx::Error> {
    if !is_emailable(kind) {
        return Ok(());
    }
    let verified: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1 AND email IS NOT NULL \
         AND email_verified_at IS NOT NULL AND NOT suspended)",
    )
    .bind(account_id)
    .fetch_one(&mut **tx)
    .await?;
    if !verified {
        return Ok(());
    }
    if !preference_enabled(&mut *tx, account_id, kind).await? {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO email_intents (id, notification_id, account_id, kind) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (notification_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(notification_id)
    .bind(account_id)
    .bind(kind)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Compose and send one notification's email. Called after the transaction that
/// wrote the in-app row and the intent has committed, from the same durable
/// `notification.outbox` job, so a transport failure retries the event and a
/// delivered intent is never sent twice.
pub(crate) async fn deliver(state: &AppState, notification_id: Uuid) -> JobOutcome {
    // Claim the intent. A second job delivering the same notification sees the
    // live claim and returns Done; its own outbox delivery is still delivered.
    let claim: Option<Uuid> = match sqlx::query_scalar(
        "UPDATE email_intents SET claim_expires_at = now() + interval '120 seconds' \
         WHERE notification_id = $1 AND sent_at IS NULL \
           AND (claim_expires_at IS NULL OR claim_expires_at < now()) \
         RETURNING id",
    )
    .bind(notification_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(_) => return db_retry(),
    };
    let Some(intent_id) = claim else {
        return JobOutcome::Done;
    };

    let context: Result<Option<Context>, sqlx::Error> = sqlx::query_as(
        "SELECT i.kind, pr.number, r.name AS repository, o.slug AS organization, \
                a.email, a.email_verified_at IS NOT NULL AS verified \
         FROM email_intents i \
         JOIN notifications n ON n.id = i.notification_id \
         JOIN pull_requests pr ON pr.id = n.pull_request_id \
         JOIN repositories r ON r.id = pr.repository_id \
         JOIN organizations o ON o.id = r.organization_id \
         JOIN accounts a ON a.id = i.account_id \
         WHERE i.id = $1 AND NOT a.suspended",
    )
    .bind(intent_id)
    .fetch_optional(&state.pool)
    .await;

    let context = match context {
        Ok(Some(context)) => context,
        // The pull request or repository is gone, or the account was disabled
        // after the intent was queued: there is nothing left to send.
        Ok(None) => {
            let _ = mark_sent(&state.pool, intent_id).await;
            return JobOutcome::Done;
        }
        Err(_) => {
            let _ = release(&state.pool, intent_id).await;
            return db_retry();
        }
    };

    let Some(to) = context.email.clone().filter(|_| context.verified) else {
        // The address was removed or unverified between queueing and sending.
        // This is the U23a rule at the last moment: no mail to an unverified
        // address. Close the intent instead of retrying forever.
        let _ = mark_sent(&state.pool, intent_id).await;
        return JobOutcome::Done;
    };

    let (subject, body) = template(
        &context.kind,
        context.number,
        &context.organization,
        &context.repository,
        state.config.public_url.trim_end_matches('/'),
    );
    let message = EmailMessage {
        to,
        subject,
        body,
        id: intent_id,
    };
    match state.mailer.send(&message).await {
        Ok(()) => match mark_sent(&state.pool, intent_id).await {
            Ok(()) => JobOutcome::Done,
            Err(_) => JobOutcome::Retry {
                after: std::time::Duration::ZERO,
                reason: "database error".into(),
            },
        },
        Err(error) => {
            // Fixed category only: a transport error can never carry a
            // credential into a log line or `jobs.last_error`.
            tracing::warn!(%intent_id, error = %error, "notification email send failed; will retry");
            let _ = release(&state.pool, intent_id).await;
            JobOutcome::Retry {
                after: std::time::Duration::ZERO,
                reason: "mail transport failed".into(),
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct Context {
    kind: String,
    number: i32,
    repository: String,
    organization: String,
    email: Option<String>,
    verified: bool,
}

async fn mark_sent(pool: &PgPool, intent_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE email_intents SET sent_at = clock_timestamp(), claim_expires_at = NULL \
         WHERE id = $1 AND sent_at IS NULL",
    )
    .bind(intent_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn release(pool: &PgPool, intent_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE email_intents SET claim_expires_at = NULL WHERE id = $1")
        .bind(intent_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn db_retry() -> JobOutcome {
    JobOutcome::Retry {
        after: std::time::Duration::ZERO,
        reason: "database error".into(),
    }
}

/// What each notification says. Plain text, no token, no secret, no actor's
/// name and no other account's data. The link is the recipient's own pull
/// request, and reading it still enforces access.
fn template(
    kind: &str,
    number: i32,
    organization: &str,
    repository: &str,
    public_url: &str,
) -> (String, String) {
    let link = format!("{public_url}/{organization}/{repository}/pulls/{number}");
    match kind {
        "pull_request.merged" => (
            format!("[GitHearth] Pull request #{number} merged"),
            format!(
                "Your pull request #{number} in {organization}/{repository} was merged.\n\n\
                 Open {link}\n"
            ),
        ),
        "pull_request.review_requested" => (
            format!("[GitHearth] Review requested on pull request #{number}"),
            format!(
                "You were asked to review pull request #{number} in {organization}/{repository}.\n\n\
                 Open {link}\n"
            ),
        ),
        _ => (
            format!("[GitHearth] New review on pull request #{number}"),
            format!(
                "Your pull request #{number} in {organization}/{repository} received a review.\n\n\
                 Open {link}\n"
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_carry_no_token_or_other_account_data() {
        for kind in EMAILABLE_KINDS {
            let (subject, body) = template(kind, 7, "team", "repo", "https://forge.example");
            assert!(!body.contains("token"));
            assert!(body.contains("https://forge.example/team/repo/pulls/7"));
            assert!(!subject.contains("token"));
        }
    }

    #[test]
    fn unknown_kinds_are_not_emailable() {
        assert!(!is_emailable("account.email_verification_requested"));
        assert!(!is_emailable("pull_request.closed"));
        for kind in EMAILABLE_KINDS {
            assert!(is_emailable(kind));
        }
    }
}
