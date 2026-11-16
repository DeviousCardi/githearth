//! Identity slice (M11a increment 8A): the schema's application half.
//!
//! This module holds the account-link rule, change of address and self-service
//! recovery. The schema is migration `0058_identity_schema_and_recovery.sql`;
//! the design of record is [ADR 0015](../../../docs/decisions/0015-identity-slice.md).
//!
//! # What M4.1 already owns
//!
//! The primary email, its verification token table, the administrator reset
//! token table and `must_change_password` are M4.1's. This module consumes
//! them: a new address is verified before it replaces the old one, and recovery
//! reuses `password_reset_tokens` rather than adding a second token scheme.
//! Every token is stored as a SHA-256 digest and is single-use and expiring.
//!
//! # Change of address
//!
//! `accounts.email` is not written when a change is requested. It is written
//! only when a token addressed to the *new* address is redeemed. That is what
//! makes the old address stop receiving mail only after the new one verifies:
//! notification and recovery paths all read `accounts.email`, which is still
//! the old address until then. A redeemed change also queues a notice to the
//! old address.
//!
//! # Recovery
//!
//! An anonymous request names an address. Only a verified account address
//! produces a row and mail; an unknown, unverified or disabled address produces
//! the same `202` and nothing else. Two budgets are charged before anything is
//! stored: the per-client anonymous security budget (an hour, so one caller
//! cannot key an unbounded address map) and M4.2's per-destination-address
//! security-mail budget [`crate::limits::Limits::allow_email`]. A request above
//! either is refused with `429`. This module does not rebuild a limiter.
//!
//! # Data-key reference
//!
//! [`bind_account_data_key`] and [`bind_organization_data_key`] are the
//! provisioning writers of the reference the key lane derives. The reference
//! is deterministic from the subject, a plain uuid (the owner key is an
//! operator-custody file, not a database row), and the database refuses any
//! value other than the derived one. Shredding removes the custody file and
//! leaves the reference.
//!
//! # External identities
//!
//! [`link_external_identity`] is the account-takeover boundary used by 8B's
//! OIDC callback. It creates a link only when the provider says the email is
//! verified **and** that address is already the account's verified address. An
//! absent, unverified or mismatched provider email never links.

use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate_token_hash, bearer_credential, client_ip::ResolvedIp,
    data_keys, event_with, invalid, invitations::valid_email, jobs::JobOutcome, mail::EmailMessage,
};

type ApiResult<T> = Result<T, ApiError>;

/// A change-of-address link stays redeemable for one day.
const CHANGE_TTL_HOURS: i64 = 24;
/// A self-service recovery link stays redeemable for one hour, the same order
/// as the administrator-issued code.
const RECOVERY_TTL_MINUTES: i64 = 60;

fn change_refused() -> ApiError {
    ApiError(
        StatusCode::UNAUTHORIZED,
        "invalid or expired email change token",
    )
}

fn recovery_refused() -> ApiError {
    ApiError(
        StatusCode::UNAUTHORIZED,
        "invalid or expired recovery token",
    )
}

fn conflict(message: &'static str) -> ApiError {
    ApiError(StatusCode::CONFLICT, message)
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

/// A session credential, never an access token. Changing where account mail is
/// sent is an account action, like requesting email verification (M4.1).
fn session_credential(headers: &HeaderMap) -> ApiResult<Vec<u8>> {
    match bearer_credential(headers)? {
        crate::Credential::Session { token_hash } => Ok(token_hash),
        crate::Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot change account email",
        )),
    }
}

async fn session_account(state: &AppState, headers: &HeaderMap) -> ApiResult<crate::Account> {
    let token_hash = session_credential(headers)?;
    Ok(authenticate_token_hash(&state.pool, token_hash).await?.0)
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/me/email-change", post(request_email_change))
        .route("/api/v1/email-change/confirm", post(confirm_email_change))
        .route("/api/v1/password-recovery/request", post(request_recovery))
        .route("/api/v1/password-recovery/confirm", post(confirm_recovery))
        // The links in the two mails. JSON clients use the endpoints above;
        // these pages are the browser path to the same two functions.
        .route(
            "/change-email/{token}",
            get(change_email_page).post(change_email_form),
        )
        .route(
            "/reset-password/{token}",
            get(reset_password_page).post(reset_password_form),
        )
}

// ---------------------------------------------------------------------------
// External identity linking
// ---------------------------------------------------------------------------

/// One provider identity as the OIDC callback will present it.
#[derive(Clone, Debug)]
pub struct ExternalIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

/// Links a provider identity to an account, or refuses.
///
/// The rule is ADR 0015's account-takeover boundary:
///
///  * an existing `(issuer, subject)` link resolves to its account and never
///    moves to another account;
///  * a new link requires a present email, the provider's `email_verified`
///    claim, a syntactically valid address, and an account whose own address is
///    that same value **and** is verified;
///  * every other case returns `None` and writes nothing, so an unverified or
///    absent provider email can never take over an existing account.
///
/// `None` means "no existing account to link"; 8B's sign-up path may then create
/// a new account. It never means "linked to somebody else".
pub async fn link_external_identity(
    pool: &PgPool,
    identity: &ExternalIdentity,
) -> Result<Option<Uuid>, sqlx::Error> {
    if let Some(account_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT account_id FROM external_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(account_id));
    }
    if !identity.email_verified {
        return Ok(None);
    }
    let Some(email) = identity.email.as_deref().map(normalize_email) else {
        return Ok(None);
    };
    if !valid_email(&email) {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    // Only an account that has itself verified this exact address may be
    // linked. An unverified account address is not evidence.
    let account: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE email = $1 AND email_verified_at IS NOT NULL AND NOT suspended",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(account) = account else {
        tx.rollback().await?;
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO external_identities \
             (id, account_id, issuer, subject, email, email_verified) \
         VALUES ($1, $2, $3, $4, $5, true) \
         ON CONFLICT (issuer, subject) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(account)
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .bind(&email)
    .execute(&mut *tx)
    .await?;
    // A concurrent callback may have linked the same subject first. Reading
    // the row back makes both callbacks agree on one account instead of
    // trusting the insert's own target.
    let linked: Uuid = sqlx::query_scalar(
        "SELECT account_id FROM external_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(linked))
}

// ---------------------------------------------------------------------------
// Data-key reference (the crypto-shredding interface)
// ---------------------------------------------------------------------------

/// Writes the account's canonical data-key reference after the key lane has
/// created the key. Returns `None` if the account does not exist.
///
/// This is the provisioning writer for `accounts.data_key_id` (8B enrolment,
/// 8C creation and import re-provisioning). The value comes from the key
/// lane's [`data_keys::account_data_key_id`] — there is no second derivation
/// here — and the database CHECK (`identity_account_data_key`) refuses any
/// other, so a caller cannot bind a key that belongs to another subject. The
/// reference is a plain `uuid`, not a row foreign key: the owner key is an
/// operator-custody file (the agreed interface with the key lane), a shredded
/// key leaves no row, and the reference must survive the shred. `NULL` means
/// only "not provisioned yet".
pub async fn bind_account_data_key<'e, E>(
    executor: E,
    account: Uuid,
) -> Result<Option<Uuid>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar("UPDATE accounts SET data_key_id = $2 WHERE id = $1 RETURNING data_key_id")
        .bind(account)
        .bind(data_keys::account_data_key_id(account))
        .fetch_optional(executor)
        .await
}

/// The organization half of [`bind_account_data_key`].
pub async fn bind_organization_data_key<'e, E>(
    executor: E,
    organization: Uuid,
) -> Result<Option<Uuid>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(
        "UPDATE organizations SET data_key_id = $2 WHERE id = $1 RETURNING data_key_id",
    )
    .bind(organization)
    .bind(data_keys::organization_data_key_id(organization))
    .fetch_optional(executor)
    .await
}

// ---------------------------------------------------------------------------
// Change of address: request
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeEmail {
    email: String,
}

#[derive(Serialize)]
struct Requested {
    expires_at: String,
}

async fn request_email_change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ChangeEmail>,
) -> ApiResult<(StatusCode, Json<Requested>)> {
    let account = session_account(&state, &headers).await?;
    let new_email = normalize_email(&input.email);
    if !valid_email(&new_email) {
        return Err(invalid());
    }
    let mut tx = state.pool.begin().await?;
    let row: Option<(Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT email, email_verified_at IS NOT NULL, suspended \
         FROM accounts WHERE id = $1 FOR UPDATE",
    )
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current, verified, suspended)) = row else {
        return Err(crate::unauthorized());
    };
    if suspended {
        return Err(conflict("account is disabled"));
    }
    // A change of address replaces a verified address. An unverified account
    // uses M4.1's verification flow, which is what sets the first address.
    let Some(current) = current.filter(|_| verified) else {
        return Err(conflict("a verified email address is required"));
    };
    if current == new_email {
        return Err(conflict("email address is already verified"));
    }
    let taken: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = $1 AND id <> $2")
            .bind(&new_email)
            .bind(account.id)
            .fetch_optional(&mut *tx)
            .await?;
    if taken.is_some() {
        return Err(conflict("email address is already in use"));
    }
    // A new request supersedes every earlier unused change for the account.
    sqlx::query(
        "UPDATE email_changes SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(account.id)
    .execute(&mut *tx)
    .await?;
    let id = Uuid::new_v4();
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO email_changes \
             (id, account_id, previous_email, new_email, requested_by, expires_at) \
         VALUES ($1, $2, $3, $4, $5, clock_timestamp() + $6 * interval '1 hour') \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(id)
    .bind(account.id)
    .bind(&current)
    .bind(&new_email)
    .bind(account.id)
    .bind(CHANGE_TTL_HOURS as f64)
    .fetch_one(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        account.id,
        "account.email_change_requested",
        // Ids only: the consumer reads the addresses from the row.
        serde_json::json!({"account_id": account.id, "change_id": id}),
    )
    .await
    .map_err(ApiError::from)?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(Requested { expires_at })))
}

/// The outbox consumer for a change request: mint the token, store only its
/// digest, and send the single-use link to the new address. A retry mints a
/// fresh token, superseding one whose delivery may have failed.
pub(crate) async fn send_change_requested(state: &AppState, change_id: Uuid) -> JobOutcome {
    let row = sqlx::query(
        "SELECT ec.new_email, ec.previous_email, \
                ec.sent_at IS NOT NULL AS sent, ec.used_at IS NOT NULL AS used, \
                ec.expires_at <= clock_timestamp() AS expired, \
                a.email AS account_email, a.email_verified_at IS NOT NULL AS verified, \
                a.suspended \
         FROM email_changes ec JOIN accounts a ON a.id = ec.account_id \
         WHERE ec.id = $1",
    )
    .bind(change_id)
    .fetch_optional(&state.pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return JobOutcome::Done,
        Err(_) => return db_retry(),
    };
    let new_email: String = row.get("new_email");
    let previous_email: String = row.get("previous_email");
    let account_email: Option<String> = row.get("account_email");
    let already_sent: bool = row.get("sent");
    let used: bool = row.get("used");
    let expired: bool = row.get("expired");
    let verified: bool = row.get("verified");
    let suspended: bool = row.get("suspended");
    // The account must still hold the address the change was requested from;
    // an intervening change makes this request stale and unusable.
    if already_sent
        || used
        || expired
        || suspended
        || !verified
        || account_email.as_deref() != Some(previous_email.as_str())
    {
        return JobOutcome::Done;
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    if sqlx::query("UPDATE email_changes SET token_hash = $2 WHERE id = $1")
        .bind(change_id)
        .bind(token_hash(&token))
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }

    if !state.limits.allow_email(None, &new_email, true) {
        return JobOutcome::Retry {
            after: std::time::Duration::from_secs(60),
            reason: "security email rate limit".into(),
        };
    }
    let link = format!(
        "{}/change-email/{token}",
        state.config.public_url.trim_end_matches('/')
    );
    let message = EmailMessage {
        to: new_email,
        subject: "[GitHearth] Confirm your new email address".to_string(),
        body: format!(
            "Confirm this new address for your GitHearth account by opening:\n\n\
             {link}\n\n\
             Your current address keeps working until this link is opened. \
             This link works once and expires in {CHANGE_TTL_HOURS} hours. \
             If you did not request it, ignore this message.\n"
        ),
        id: change_id,
    };
    if let Err(error) = state.mailer.send(&message).await {
        tracing::warn!(%change_id, error = %error, "change-of-address email send failed; will retry");
        return JobOutcome::Retry {
            after: std::time::Duration::ZERO,
            reason: "mail transport failed".into(),
        };
    }
    if sqlx::query("UPDATE email_changes SET sent_at = clock_timestamp() WHERE id = $1")
        .bind(change_id)
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }
    JobOutcome::Done
}

// ---------------------------------------------------------------------------
// Change of address: redeem
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmChange {
    token: String,
}

async fn confirm_email_change(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ConfirmChange>,
) -> ApiResult<StatusCode> {
    let account = session_account(&state, &headers).await?;
    confirm_change_inner(&state, account.id, &input.token).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Single-use, expiring, presented by the account it belongs to, and bound to
/// the address the account still holds. Every refusal is one status and body.
pub(crate) async fn confirm_change_inner(
    state: &AppState,
    account_id: Uuid,
    token: &str,
) -> ApiResult<()> {
    if !valid_token(token) {
        return Err(change_refused());
    }
    let hash = token_hash(token);
    let mut tx = state.pool.begin().await?;
    let live: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT ec.id, ec.previous_email, ec.new_email \
         FROM email_changes ec JOIN accounts a ON a.id = ec.account_id \
         WHERE ec.token_hash = $1 AND ec.used_at IS NULL \
           AND ec.expires_at > clock_timestamp() AND ec.account_id = $2 \
           AND a.email = ec.previous_email AND a.email_verified_at IS NOT NULL \
           AND NOT a.suspended \
         FOR UPDATE OF ec, a",
    )
    .bind(&hash)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((change_id, previous_email, new_email)) = live else {
        return Err(change_refused());
    };
    // The lookup is the single-use guard: `used_at IS NULL` plus the row lock
    // means a replay waits, re-reads and is not found. This write records it.
    sqlx::query("UPDATE email_changes SET used_at = clock_timestamp() WHERE id = $1")
        .bind(change_id)
        .execute(&mut *tx)
        .await?;
    // `a.email = ec.previous_email` is checked again by the UPDATE, so an
    // address that moved between the lookup and this write cannot be replaced
    // by a stale request.
    let verified = match sqlx::query(
        "UPDATE accounts SET email = $2, email_verified_at = clock_timestamp() \
         WHERE id = $1 AND email = $3 AND email_verified_at IS NOT NULL",
    )
    .bind(account_id)
    .bind(&new_email)
    .bind(&previous_email)
    .execute(&mut *tx)
    .await
    {
        Ok(result) => result,
        Err(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            let _ = tx.rollback().await;
            return Err(conflict("email address is already in use"));
        }
        Err(error) => return Err(error.into()),
    };
    if verified.rows_affected() != 1 {
        return Err(change_refused());
    }
    event_with(
        &mut tx,
        None,
        account_id,
        "account.email_changed",
        serde_json::json!({"account_id": account_id, "change_id": change_id}),
    )
    .await
    .map_err(ApiError::from)?;
    tx.commit().await?;
    Ok(())
}

/// The outbox consumer for a completed change: tell the old address that it no
/// longer receives this account's mail. The message is addressed to the old
/// address, which is the point of the notice.
pub(crate) async fn send_change_notice(state: &AppState, change_id: Uuid) -> JobOutcome {
    let row = sqlx::query(
        "SELECT previous_email, new_email, notice_id, used_at IS NOT NULL AS used \
         FROM email_changes WHERE id = $1",
    )
    .bind(change_id)
    .fetch_optional(&state.pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return JobOutcome::Done,
        Err(_) => return db_retry(),
    };
    let used: bool = row.get("used");
    if !used {
        return JobOutcome::Done;
    }
    let previous_email: String = row.get("previous_email");
    let new_email: String = row.get("new_email");
    // The notice names its own row id, not the confirmation's: a receiving
    // server must not deduplicate "your address was changed" against the
    // confirmation message.
    let notice_id: Uuid = row.get("notice_id");
    if !state.limits.allow_email(None, &previous_email, true) {
        return JobOutcome::Retry {
            after: std::time::Duration::from_secs(60),
            reason: "security email rate limit".into(),
        };
    }
    let message = EmailMessage {
        to: previous_email,
        subject: "[GitHearth] Your email address was changed".to_string(),
        body: format!(
            "The email address on your GitHearth account was changed to {new_email}.\n\n\
             This address will no longer receive mail for the account. \
             If you did not make this change, contact your installation administrator immediately.\n"
        ),
        id: notice_id,
    };
    if let Err(error) = state.mailer.send(&message).await {
        tracing::warn!(%change_id, error = %error, "change-of-address notice send failed; will retry");
        return JobOutcome::Retry {
            after: std::time::Duration::ZERO,
            reason: "mail transport failed".into(),
        };
    }
    JobOutcome::Done
}

// ---------------------------------------------------------------------------
// Self-service recovery: request
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverEmail {
    email: String,
}

/// Charges the anonymous per-client security-mail budget before anything
/// caller-supplied reaches a per-address map. An unresolved client (a test
/// router with no peer) is one shared key, exactly like the login and fetch
/// budgets; it is refused rather than allowed to bypass the budget.
fn spend_anonymous_security(state: &AppState, client: &ResolvedIp) -> ApiResult<()> {
    if !state
        .limits
        .allow_anonymous_security(client.0.map(|client| client.addr()))
    {
        return Err(crate::rate_limited());
    }
    Ok(())
}

/// Anonymous. Every valid address gets the same `202`: unknown, unverified and
/// disabled accounts write nothing and queue nothing, so the response cannot be
/// used to learn whether an account exists.
///
/// Two budgets are charged, in this order: the anonymous per-client security
/// budget (an hour, so one caller cannot key an unbounded address map), then
/// M4.2's per-destination-address security-mail budget,
/// [`crate::limits::Limits::allow_email`]. The address budget is charged for
/// every valid address, known or not, so an attacker cannot use the refusal
/// itself as an existence oracle.
async fn request_recovery(
    State(state): State<AppState>,
    client: ResolvedIp,
    Json(input): Json<RecoverEmail>,
) -> ApiResult<StatusCode> {
    let email = normalize_email(&input.email);
    if !valid_email(&email) {
        return Err(invalid());
    }
    spend_anonymous_security(&state, &client)?;
    if !state.limits.allow_email(None, &email, true) {
        return Err(crate::rate_limited());
    }
    let mut tx = state.pool.begin().await?;
    let account: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE email = $1 AND email_verified_at IS NOT NULL AND NOT suspended",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(account_id) = account {
        // A new request supersedes the account's earlier unused self-service
        // links. Administrator-issued codes are left alone: they are a
        // different purpose and a different delivery path.
        sqlx::query(
            "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
             WHERE account_id = $1 AND used_at IS NULL AND purpose = 'self_service'",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO password_reset_tokens \
                 (id, account_id, token_hash, expires_at, purpose, email) \
             VALUES ($1, $2, NULL, clock_timestamp() + $3 * interval '1 minute', \
                     'self_service', $4)",
        )
        .bind(id)
        .bind(account_id)
        .bind(RECOVERY_TTL_MINUTES as f64)
        .bind(&email)
        .execute(&mut *tx)
        .await?;
        event_with(
            &mut tx,
            None,
            account_id,
            "account.password_recovery_requested",
            // An anonymous caller asked for this; the actor id is the account
            // whose address matched, not a principal. The flag lets an
            // investigation tell an owner-requested reset from an
            // attacker-requested one until `actor_kind` grows an anonymous
            // principal.
            serde_json::json!({
                "account_id": account_id,
                "token_id": id,
                "anonymous": true,
            }),
        )
        .await
        .map_err(ApiError::from)?;
    }
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

/// The outbox consumer for a recovery request: mint the token into the row and
/// send it to the verified address. The request handler already charged the
/// per-address budget, so this path does not charge it a second time.
pub(crate) async fn send_recovery_requested(state: &AppState, token_id: Uuid) -> JobOutcome {
    let row = sqlx::query(
        "SELECT t.email, t.sent_at IS NOT NULL AS sent, \
                t.used_at IS NOT NULL AS used, \
                t.expires_at <= clock_timestamp() AS expired, \
                a.email AS account_email, a.email_verified_at IS NOT NULL AS verified, \
                a.suspended \
         FROM password_reset_tokens t JOIN accounts a ON a.id = t.account_id \
         WHERE t.id = $1 AND t.purpose = 'self_service' AND t.email IS NOT NULL",
    )
    .bind(token_id)
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
    let suspended: bool = row.get("suspended");
    if already_sent
        || used
        || expired
        || suspended
        || !verified
        || account_email.as_deref() != Some(email.as_str())
    {
        return JobOutcome::Done;
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    if sqlx::query("UPDATE password_reset_tokens SET token_hash = $2 WHERE id = $1")
        .bind(token_id)
        .bind(token_hash(&token))
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }
    let link = format!(
        "{}/reset-password/{token}",
        state.config.public_url.trim_end_matches('/')
    );
    let message = EmailMessage {
        to: email,
        subject: "[GitHearth] Reset your password".to_string(),
        body: format!(
            "A password reset was requested for your GitHearth account. Open this link to choose a new password:\n\n\
             {link}\n\n\
             This link works once and expires in {RECOVERY_TTL_MINUTES} minutes. \
             If you did not request it, ignore this message; your password has not changed.\n"
        ),
        id: token_id,
    };
    if let Err(error) = state.mailer.send(&message).await {
        tracing::warn!(%token_id, error = %error, "recovery email send failed; will retry");
        return JobOutcome::Retry {
            after: std::time::Duration::ZERO,
            reason: "mail transport failed".into(),
        };
    }
    if sqlx::query("UPDATE password_reset_tokens SET sent_at = clock_timestamp() WHERE id = $1")
        .bind(token_id)
        .execute(&state.pool)
        .await
        .is_err()
    {
        return db_retry();
    }
    JobOutcome::Done
}

// ---------------------------------------------------------------------------
// Self-service recovery: redeem
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmRecovery {
    email: String,
    token: String,
    new_password: crate::account_admin::Secret,
}

async fn confirm_recovery(
    State(state): State<AppState>,
    client: ResolvedIp,
    Json(input): Json<ConfirmRecovery>,
) -> ApiResult<StatusCode> {
    confirm_recovery_inner(
        &state,
        &client,
        &input.email,
        &input.token,
        input.new_password,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Single-use, expiring, and bound to the address it was mailed to. The email
/// in the request is the wrong-account guard: a token for another account, or
/// one whose account has since changed its address, is refused with the same
/// status and body as an unknown or expired token.
///
/// The anonymous per-client budget is charged first, before the token lookup
/// and before Argon2, so this public endpoint cannot be used to grow the
/// address map or to buy unbounded hashing work. The password policy is
/// checked only after a live token is found: a wrong token plus a weak
/// password still gets the uniform `401`, and the `400` is reachable only by
/// proving possession of a live token, which says nothing about an account.
pub(crate) async fn confirm_recovery_inner(
    state: &AppState,
    client: &ResolvedIp,
    email: &str,
    token: &str,
    new_password: crate::account_admin::Secret,
) -> ApiResult<()> {
    let email = normalize_email(email);
    if !valid_email(&email) || !valid_token(token) {
        return Err(recovery_refused());
    }
    spend_anonymous_security(state, client)?;
    let hash = token_hash(token);
    // Look the token up before hashing the password: this public endpoint must
    // not let a wrong token buy an Argon2 hash and starve the authentication
    // slot (the same rule as the administrator code path).
    let live: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM password_reset_tokens t JOIN accounts a ON a.id = t.account_id \
         WHERE t.token_hash = $1 AND t.used_at IS NULL \
           AND t.expires_at > clock_timestamp() \
           AND t.purpose = 'self_service' AND t.email = $2 \
           AND a.email = $2 AND a.email_verified_at IS NOT NULL AND NOT a.suspended",
    )
    .bind(&hash)
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?;
    if live.is_none() {
        return Err(recovery_refused());
    }
    // A live token is the only way to reach the policy refusal, so a weak
    // password cannot be told apart from a bad token without proving the
    // token.
    crate::account_admin::check_password(new_password.expose())?;
    let password_hash = crate::account_admin::hash_password_async(state, new_password).await?;
    let mut tx = state.pool.begin().await?;
    // Re-check and claim under the row lock: two concurrent redemptions of the
    // same token produce exactly one success.
    let token_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM password_reset_tokens t JOIN accounts a ON a.id = t.account_id \
         WHERE t.token_hash = $1 AND t.used_at IS NULL \
           AND t.expires_at > clock_timestamp() \
           AND t.purpose = 'self_service' AND t.email = $2 \
           AND a.email = $2 AND a.email_verified_at IS NOT NULL AND NOT a.suspended \
         FOR UPDATE OF t",
    )
    .bind(&hash)
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(token_id) = token_id else {
        return Err(recovery_refused());
    };
    let account_id: Uuid = sqlx::query_scalar(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE id = $1 RETURNING account_id",
    )
    .bind(token_id)
    .fetch_one(&mut *tx)
    .await?;
    let revoked =
        crate::account_admin::replace_password(&mut tx, account_id, &password_hash, None).await?;
    // One successful recovery retires the account's other self-service links.
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL AND purpose = 'self_service'",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        account_id,
        "account.password_recovered",
        serde_json::json!({
            "account_id": account_id,
            "sessions_revoked": revoked.sessions,
            "access_tokens_revoked": revoked.access_tokens,
            "ssh_keys_revoked": revoked.ssh_keys,
        }),
    )
    .await
    .map_err(ApiError::from)?;
    tx.commit().await?;
    Ok(())
}

fn db_retry() -> JobOutcome {
    JobOutcome::Retry {
        after: std::time::Duration::ZERO,
        reason: "database error".into(),
    }
}

// ---------------------------------------------------------------------------
// Browser links
// ---------------------------------------------------------------------------

fn refusal_page(title: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{title}</title></head><body>\
         <main><h1>{title}</h1>\
         <p>This link is invalid, expired or already used.</p>\
         <p><a href=\"/login\">Sign in</a></p></main></body></html>"
    )
}

async fn change_email_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !valid_token(&token) {
        return (StatusCode::NOT_FOUND, Html(refusal_page("Change email"))).into_response();
    }
    let account = match crate::ui::browser_auth(&state, &headers).await {
        Ok((account, _)) => account,
        Err(_) => {
            return Html(
                "<!doctype html><html><head><title>Change email</title></head><body>\
                 <main><h1>Change email</h1>\
                 <p>Sign in, then open this link again to confirm the new address.</p>\
                 <p><a href=\"/login\">Sign in</a></p></main></body></html>",
            )
            .into_response();
        }
    };
    let body = format!(
        "<h1>Change email address</h1><p>Signed in as {}.</p>\
         <form method=\"post\" action=\"/change-email/{token}\">\
         <button type=\"submit\">Confirm this address</button></form>",
        crate::ui::esc(&account.username)
    );
    Html(crate::ui::render_page(
        "Change email",
        Some(&account.username),
        None,
        account.is_admin,
        &body,
    ))
    .into_response()
}

async fn change_email_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    let account = match crate::ui::browser_auth(&state, &headers).await {
        Ok((account, _)) => account,
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, Html(refusal_page("Change email"))).into_response();
        }
    };
    match confirm_change_inner(&state, account.id, &token).await {
        Ok(()) => Redirect::to("/settings").into_response(),
        Err(_) => (StatusCode::UNAUTHORIZED, Html(refusal_page("Change email"))).into_response(),
    }
}

#[derive(Deserialize)]
struct ResetForm {
    email: String,
    new_password: String,
}

async fn reset_password_page(Path(token): Path<String>) -> Response {
    if !valid_token(&token) {
        return (StatusCode::NOT_FOUND, Html(refusal_page("Reset password"))).into_response();
    }
    let body = format!(
        "<h1>Reset password</h1>\
         <form method=\"post\" action=\"/reset-password/{token}\">\
         <label for=\"email\">Email address</label>\
         <input id=\"email\" name=\"email\" type=\"email\" autocomplete=\"email\" required>\
         <label for=\"new_password\">New password</label>\
         <input id=\"new_password\" name=\"new_password\" type=\"password\" \
                autocomplete=\"new-password\" required>\
         <button type=\"submit\">Set new password</button></form>"
    );
    Html(crate::ui::render_page(
        "Reset password",
        None,
        None,
        false,
        &body,
    ))
    .into_response()
}

async fn reset_password_form(
    State(state): State<AppState>,
    client: ResolvedIp,
    Path(token): Path<String>,
    Form(input): Form<ResetForm>,
) -> Response {
    let secret = crate::account_admin::Secret::new(input.new_password);
    match confirm_recovery_inner(&state, &client, &input.email, &token, secret).await {
        Ok(()) => Redirect::to("/login?reset=1").into_response(),
        Err(error) => (error.status_code(), Html(refusal_page("Reset password"))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Tests that need no database
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_addresses_are_validated_before_any_database_read() {
        assert!(valid_token(&"a".repeat(64)));
        assert!(!valid_token("a".repeat(63).as_str()));
        assert!(!valid_token(&"g".repeat(64)));
        assert!(valid_email("alice@example.com"));
        assert_eq!(normalize_email("  Alice@Example.COM "), "alice@example.com");
    }
}
