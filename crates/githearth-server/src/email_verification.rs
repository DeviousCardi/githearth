//! Email verification (M4.1 U23a).
//!
//! A verification link is requested by the account owner or an administrator,
//! delivered through the same durable outbox consumer as notification email and
//! redeemed once. The table records the address the link verifies, so a link
//! requested before an address change cannot verify the new address.
//!
//! The plaintext token exists only while the mail is being composed: the row
//! starts with `token_hash` NULL, the consumer writes the SHA-256 just before
//! sending, and no job payload, text column or audit event carries the token.
//! A duplicate address is refused by the unique index 0049 already has; the
//! handler turns that database error into a fixed `409` that names no account.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Credential, authenticate_token_hash, bearer_credential, event_with,
    invitations::valid_email, jobs::JobOutcome, mail::EmailMessage,
};

type ApiResult<T> = Result<T, ApiError>;

/// How long a verification link stays redeemable.
const VERIFICATION_TTL_HOURS: i64 = 24;

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}

fn refused() -> ApiError {
    ApiError(
        StatusCode::UNAUTHORIZED,
        "invalid or expired verification token",
    )
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/me/email-verification",
            post(request_own_verification),
        )
        .route(
            "/api/v1/admin/accounts/{id}/email-verification",
            post(request_for_account),
        )
        .route(
            "/api/v1/email-verification/redeem",
            post(redeem_verification),
        )
        .route("/verify-email/{token}", get(verify_page).post(verify_form))
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A session credential, never an access token: changing where verification
/// mail is sent is an account action.
fn session_credential(headers: &HeaderMap) -> ApiResult<Vec<u8>> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => Ok(token_hash),
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot request email verification",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEmail {
    email: String,
}

#[derive(Serialize)]
struct Requested {
    expires_at: String,
}

async fn request_own_verification(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RequestEmail>,
) -> ApiResult<(StatusCode, Json<Requested>)> {
    let token_hash = session_credential(&headers)?;
    let (account, _) = authenticate_token_hash(&state.pool, token_hash).await?;
    let requested = request_inner(&state, account.id, account.id, &input.email).await?;
    Ok((StatusCode::ACCEPTED, Json(requested)))
}

async fn request_for_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<RequestEmail>,
) -> ApiResult<(StatusCode, Json<Requested>)> {
    let (actor, _) = crate::account_admin::administrator(&state, &headers).await?;
    let requested = request_inner(&state, actor.id, id, &input.email).await?;
    Ok((StatusCode::ACCEPTED, Json(requested)))
}

/// Shared by the owner and administrator paths. `actor` is who asked; the link
/// is addressed to `accounts.email` only after it has been verified.
async fn request_inner(
    state: &AppState,
    actor: Uuid,
    account_id: Uuid,
    email: &str,
) -> ApiResult<Requested> {
    let email = email.trim().to_ascii_lowercase();
    if !valid_email(&email) {
        return Err(crate::invalid());
    }
    let mut tx = state.pool.begin().await?;
    let row: Option<(Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT email, email_verified_at IS NOT NULL, suspended FROM accounts \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current, verified, suspended)) = row else {
        return Err(not_found());
    };
    if suspended {
        return Err(ApiError(StatusCode::CONFLICT, "account is disabled"));
    }
    if verified {
        // Re-verifying the same address changes nothing; replacing a verified
        // address is the identity slice's change-of-address flow, not M4.1.
        return Err(if current.as_deref() == Some(email.as_str()) {
            ApiError(StatusCode::CONFLICT, "email address is already verified")
        } else {
            ApiError(
                StatusCode::CONFLICT,
                "changing a verified email address is not supported yet",
            )
        });
    }
    match sqlx::query("UPDATE accounts SET email = $2 WHERE id = $1")
        .bind(account_id)
        .bind(&email)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {}
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            let _ = tx.rollback().await;
            // The unique index is the authority. The refusal names no account,
            // username or id, so it cannot be used to identify the holder.
            return Err(ApiError(
                StatusCode::CONFLICT,
                "email address is already in use",
            ));
        }
        Err(error) => return Err(error.into()),
    }
    // A new request supersedes every earlier unused link for this account.
    sqlx::query(
        "UPDATE email_verifications SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    let id = Uuid::new_v4();
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO email_verifications \
             (id, account_id, email, requested_by, expires_at) \
         VALUES ($1, $2, $3, $4, clock_timestamp() + $5 * interval '1 hour') \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(id)
    .bind(account_id)
    .bind(&email)
    .bind(actor)
    .bind(VERIFICATION_TTL_HOURS as f64)
    .fetch_one(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        actor,
        "account.email_verification_requested",
        // Ids only, like every other outbox payload: the handler loads the
        // address, expiry and token state from the row.
        serde_json::json!({
            "account_id": account_id,
            "verification_id": id,
        }),
    )
    .await
    .map_err(ApiError::from)?;
    tx.commit().await?;
    Ok(Requested { expires_at })
}

/// The outbox consumer's handler for one request. It mints the token, writes
/// only its digest and sends the single-use link. A retry mints a fresh token,
/// which supersedes a link whose delivery may have failed.
pub(crate) async fn send_requested(state: &AppState, verification_id: Uuid) -> JobOutcome {
    let row = sqlx::query(
        "SELECT ev.email, ev.sent_at IS NOT NULL AS sent, ev.used_at IS NOT NULL AS used, \
                ev.expires_at <= clock_timestamp() AS expired, \
                a.email AS account_email, a.email_verified_at IS NOT NULL AS verified \
         FROM email_verifications ev JOIN accounts a ON a.id = ev.account_id \
         WHERE ev.id = $1",
    )
    .bind(verification_id)
    .fetch_optional(&state.pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return JobOutcome::Done,
        Err(_) => return db_retry(),
    };
    let email: String = row.get("email");
    let account_email: Option<String> = row.get("account_email");
    let already_sent: bool = row.get("sent");
    let used: bool = row.get("used");
    let expired: bool = row.get("expired");
    let verified: bool = row.get("verified");
    if already_sent
        || used
        || expired
        || verified
        || account_email.as_deref() != Some(email.as_str())
    {
        return JobOutcome::Done;
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let hash = token_hash(&token);
    if sqlx::query("UPDATE email_verifications SET token_hash = $2 WHERE id = $1")
        .bind(verification_id)
        .bind(&hash)
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }

    let link = format!(
        "{}/verify-email/{token}",
        state.config.public_url.trim_end_matches('/')
    );
    let message = EmailMessage {
        to: email,
        subject: "[GitHearth] Verify your email address".to_string(),
        body: format!(
            "Confirm that this address belongs to your GitHearth account by opening:\n\n\
             {link}\n\n\
             This link works once and expires in {VERIFICATION_TTL_HOURS} hours. \
             If you did not request it, ignore this message.\n"
        ),
        // Stable across a retry of this request: the same row id is the same
        // Message-ID, so a receiving server can deduplicate a re-send.
        id: verification_id,
    };
    if let Err(error) = state.mailer.send(&message).await {
        tracing::warn!(%verification_id, error = %error, "verification email send failed; will retry");
        return JobOutcome::Retry {
            after: std::time::Duration::ZERO,
            reason: "mail transport failed".into(),
        };
    }
    if sqlx::query("UPDATE email_verifications SET sent_at = clock_timestamp() WHERE id = $1")
        .bind(verification_id)
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }
    JobOutcome::Done
}

fn db_retry() -> JobOutcome {
    JobOutcome::Retry {
        after: std::time::Duration::ZERO,
        reason: "database error".into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Redeem {
    token: String,
}

async fn redeem_verification(
    State(state): State<AppState>,
    Json(input): Json<Redeem>,
) -> ApiResult<StatusCode> {
    redeem_inner(&state, &input.token).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Single-use, expiring, and bound to the address the account still holds.
/// Every refusal is the same status and body.
pub(crate) async fn redeem_inner(state: &AppState, token: &str) -> ApiResult<()> {
    if !valid_token(token) {
        return Err(refused());
    }
    let hash = token_hash(token);
    let mut tx = state.pool.begin().await?;
    let live: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT ev.id, ev.account_id, ev.email FROM email_verifications ev \
         JOIN accounts a ON a.id = ev.account_id \
         WHERE ev.token_hash = $1 AND ev.used_at IS NULL \
           AND ev.expires_at > clock_timestamp() AND a.email = ev.email \
           AND NOT a.suspended \
         FOR UPDATE OF ev",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((verification_id, account_id, email)) = live else {
        return Err(refused());
    };
    // The lookup above is the single-use guard: `used_at IS NULL` plus
    // `FOR UPDATE OF ev` means a concurrent replay waits on the row and then
    // re-reads the predicate against the committed `used_at`, so it is not
    // found. This write records the use; it deliberately carries no second
    // `used_at IS NULL` guard, so the lookup is the one place the rule lives.
    let claimed =
        sqlx::query("UPDATE email_verifications SET used_at = clock_timestamp() WHERE id = $1")
            .bind(verification_id)
            .execute(&mut *tx)
            .await?;
    if claimed.rows_affected() != 1 {
        return Err(refused());
    }
    let verified = sqlx::query(
        "UPDATE accounts SET email_verified_at = clock_timestamp() \
         WHERE id = $1 AND email = $2 AND email_verified_at IS NULL",
    )
    .bind(account_id)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    if verified.rows_affected() != 1 {
        return Err(refused());
    }
    event_with(
        &mut tx,
        None,
        account_id,
        "account.email_verified",
        serde_json::json!({"account_id": account_id, "verification_id": verification_id}),
    )
    .await
    .map_err(ApiError::from)?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Browser link
// ---------------------------------------------------------------------------

async fn verify_page(Path(token): Path<String>) -> Response {
    if !valid_token(&token) {
        return (StatusCode::NOT_FOUND, Html(refusal_page())).into_response();
    }
    Html(format!(
        "<!doctype html><html><head><title>Verify email</title></head><body>\
         <main><h1>Verify email</h1>\
         <form method=\"post\" action=\"/verify-email/{token}\">\
         <button type=\"submit\">Verify this address</button>\
         </form></main></body></html>"
    ))
    .into_response()
}

async fn verify_form(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    match redeem_inner(&state, &token).await {
        Ok(()) => Redirect::to("/login").into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, Html(refusal_page())).into_response(),
    }
}

fn refusal_page() -> String {
    "<!doctype html><html><head><title>Verification refused</title></head><body>\
     <main><h1>Verification refused</h1>\
     <p>This verification link is invalid, expired or already used.</p>\
     </main></body></html>"
        .to_string()
}
