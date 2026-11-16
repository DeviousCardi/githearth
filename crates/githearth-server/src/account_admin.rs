//! Account administration (M4.1, acceptance cases U01–U03).
//!
//! Administrators create accounts with an initial password they supply,
//! disable and re-enable them; every account owner may change their own
//! password. Account state reuses `accounts.suspended`, which login, bearer
//! sessions, access tokens, SSH keys, the browser UI and Git admission already
//! consult, so a disabled account is refused by every path (see the migration
//! `0041_account_administration`).
//!
//! Passwords only ever travel in a request body, are hashed by the shared
//! [`crate::hash_password`] path with the existing Argon2 parameters, and are
//! never logged, echoed in an error or returned by any response. Every
//! mutation writes its audit and outbox records in the same transaction as the
//! change.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    Account, ApiError, AppState, Credential, authenticate_token_hash, bearer_credential,
    event_with, invalid, login_throttle,
};

type ApiResult<T> = Result<T, ApiError>;

/// Administrator-only endpoints plus the account owner's own password change.
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/admin/accounts",
            post(create_account).get(list_accounts),
        )
        .route("/api/v1/admin/accounts/{id}/disable", post(disable_account))
        .route("/api/v1/admin/accounts/{id}/enable", post(enable_account))
        .route("/api/v1/admin/accounts/{id}/promote", post(promote_account))
        .route("/api/v1/admin/accounts/{id}/demote", post(demote_account))
        .route(
            "/api/v1/admin/accounts/{id}/password-reset",
            post(reset_password),
        )
        .route("/api/v1/me/reauthenticate", post(reauthenticate))
        .route("/api/v1/password-reset/redeem", post(redeem_password_reset))
        .route("/api/v1/user/password", post(change_own_password))
}

/// A site-wide serializer for the last-enabled-administrator rule. Every
/// disable and demote takes it, so two concurrent requests each acting on a
/// different administrator take it in turn and the second sees the first
/// change and is refused.
const ENABLED_ADMINS_LOCK: i64 = 706_918_232;

/// Shortest accepted password, in bytes, as enforced by the operator bootstrap.
/// There is deliberately no default or generated password.
pub(crate) const PASSWORD_MIN_BYTES: usize = 12;
pub(crate) const PASSWORD_MAX_BYTES: usize = 1024;

/// The refusal every administrator-only action uses.
fn not_permitted() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "not permitted")
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}

/// Refuses a password outside the accepted range without repeating it.
pub(crate) fn check_password(password: &str) -> ApiResult<()> {
    if !(PASSWORD_MIN_BYTES..=PASSWORD_MAX_BYTES).contains(&password.len())
        || crate::has_nul(password)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "password must contain 12..1024 bytes without NUL",
        ));
    }
    Ok(())
}

/// Validates a username with the same rule as the operator bootstrap.
fn check_username(username: &str) -> ApiResult<()> {
    if !crate::valid_username(username) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "username must contain 3..64 lowercase ASCII letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}

/// A session credential, never an access token: managing accounts and changing
/// a password are session actions, like issuing access tokens (tokens.rs).
fn session_credential(headers: &HeaderMap) -> ApiResult<Vec<u8>> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => Ok(token_hash),
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot manage accounts",
        )),
    }
}

/// Authenticates the caller and requires an enabled administrator with fresh
/// authentication (sudo mode). A caller who is not an administrator gets the
/// refusal the codebase uses for every administrator-only action, `403 not
/// permitted`; a stale session gets a machine-readable re-authentication
/// denial so a client or page can prompt and retry.
pub(crate) async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<(Account, Vec<u8>)> {
    let token_hash = session_credential(headers)?;
    let (account, auth_age) = authenticate_token_hash(&state.pool, token_hash.clone()).await?;
    if !account.is_admin {
        return Err(not_permitted());
    }
    if auth_age > state.fresh_auth().as_secs() {
        return Err(crate::reauthentication_required());
    }
    Ok((account, token_hash))
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

/// A new account and its initial password, both supplied by the administrator.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAccount {
    username: String,
    password: Secret,
}

/// A password as received from a caller. Its `Debug` is redacted and it is
/// consumed by hashing, so no log line or panic message can print it.
#[derive(Default, PartialEq, Eq)]
pub struct Secret(String);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self(value))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\"[redacted]\")")
    }
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// One account as any caller sees it: no password hash, only the state a
/// client needs.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AccountView {
    pub(crate) id: Uuid,
    pub(crate) username: String,
    pub(crate) is_admin: bool,
    pub(crate) suspended: bool,
    /// An administrator reset is outstanding: the account cannot sign in until
    /// it redeems the code and chooses a new password.
    pub(crate) must_change_password: bool,
    /// The primary address and when it was verified. An unverified address is
    /// never a destination for notification, recovery or sign-up mail.
    pub(crate) email: Option<String>,
    pub(crate) email_verified_at: Option<String>,
    pub(crate) created_at: String,
}

const ACCOUNT_COLUMNS: &str = "id, username, is_admin, suspended, must_change_password, email, \
  to_char(email_verified_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS email_verified_at, \
  to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

async fn create_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewAccount>,
) -> ApiResult<(StatusCode, Json<AccountView>)> {
    let (actor, _) = administrator(&state, &headers).await?;
    let created = create_account_inner(&state, actor.id, &input.username, input.password).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Shared by the JSON handler and the administration page's "Create account"
/// form.
pub async fn create_account_inner(
    state: &AppState,
    actor: Uuid,
    username: &str,
    password: Secret,
) -> ApiResult<AccountView> {
    check_username(username)?;
    check_password(password.expose())?;
    let hash = hash_password_async(state, password).await?;
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin) VALUES ($1,$2,$3,false)",
    )
    .bind(id)
    .bind(username)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    // The event names the new account, never its credentials.
    event_with(
        &mut tx,
        None,
        actor,
        "account.created",
        serde_json::json!({"account_id": id}),
    )
    .await?;
    let view = load_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(view)
}

/// Hashes off the async runtime so the Argon2 work never blocks a request
/// task, and consumes the [`Secret`] so the plaintext is dropped immediately.
/// It takes an authentication slot so account creation and a password
/// redemption cannot start unbounded hashing work around the same limiter the
/// sign-in path uses.
pub(crate) async fn hash_password_async(state: &AppState, password: Secret) -> ApiResult<String> {
    let permit = crate::acquire_auth_slot(state).await?;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::hash_password(password.expose())
    })
    .await
    {
        Ok(Ok(hash)) => Ok(hash),
        _ => Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )),
    }
}

async fn load_view(tx: &mut sqlx::PgConnection, id: Uuid) -> ApiResult<AccountView> {
    sqlx::query_as::<_, AccountView>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(not_found)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListPage {
    after: Option<Uuid>,
    limit: Option<i64>,
}

async fn list_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<ListPage>,
) -> ApiResult<Json<Vec<AccountView>>> {
    let _ = administrator(&state, &headers).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(list(&state.pool, page.after, limit).await?))
}

/// The same keyset pagination the organization list uses: `id` ascending, with
/// the last id of a page passed back as `after`.
pub async fn list(pool: &PgPool, after: Option<Uuid>, limit: i64) -> ApiResult<Vec<AccountView>> {
    Ok(sqlx::query_as::<_, AccountView>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE ($1::uuid IS NULL OR id > $1) ORDER BY id LIMIT $2"
    ))
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

// ---------------------------------------------------------------------------
// Disable and enable
// ---------------------------------------------------------------------------

async fn disable_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AccountView>> {
    let (actor, _) = administrator(&state, &headers).await?;
    Ok(Json(disable_inner(&state, actor.id, id).await?))
}

/// Disables an account. It cannot be the caller's own account, and it cannot
/// be the last enabled administrator.
pub async fn disable_inner(state: &AppState, actor: Uuid, id: Uuid) -> ApiResult<AccountView> {
    if actor == id {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "an administrator cannot disable their own account",
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ENABLED_ADMINS_LOCK)
        .execute(&mut *tx)
        .await?;
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT is_admin, suspended FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((is_admin, suspended)) = row else {
        return Err(not_found());
    };
    if suspended {
        // Already disabled: the state the caller asked for, with no new record.
        return load_view(&mut tx, id).await;
    }
    if is_admin {
        // The target itself is still enabled here, so it must be excluded:
        // the question is whether any *other* enabled administrator remains.
        // A reset-pending account cannot sign in, so it cannot serve as the
        // usable administrator that survives another administrator's disable.
        let another: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM accounts \
             WHERE is_admin AND NOT suspended AND NOT must_change_password AND id <> $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if !another {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "the last enabled administrator cannot be disabled",
            ));
        }
    }
    sqlx::query("UPDATE accounts SET suspended = true WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    // Existing sessions of a disabled account are already refused at
    // authentication; removing them makes re-enabling start from signed out.
    let sessions_revoked = sqlx::query("DELETE FROM sessions WHERE account_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // A disable is a containment action: a code issued before it must not
    // become usable again when the account is re-enabled.
    let reset_codes_retired = sqlx::query(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    event_with(
        &mut tx,
        None,
        actor,
        "account.disabled",
        serde_json::json!({
            "account_id": id,
            "sessions_revoked": sessions_revoked,
            "reset_codes_retired": reset_codes_retired,
        }),
    )
    .await?;
    let view = load_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(view)
}

async fn enable_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AccountView>> {
    let (actor, _) = administrator(&state, &headers).await?;
    Ok(Json(enable_inner(&state, actor.id, id).await?))
}

/// Re-enables an account. Access resumes through sign-in only: the sessions,
/// access tokens and SSH keys it had are untouched, but it was signed out when
/// it was disabled, so nothing issued before the disable resumes by itself.
pub async fn enable_inner(state: &AppState, actor: Uuid, id: Uuid) -> ApiResult<AccountView> {
    let mut tx = state.pool.begin().await?;
    let row: Option<bool> =
        sqlx::query_scalar("SELECT suspended FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(suspended) = row else {
        return Err(not_found());
    };
    if suspended {
        sqlx::query("UPDATE accounts SET suspended = false WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        event_with(
            &mut tx,
            None,
            actor,
            "account.enabled",
            serde_json::json!({"account_id": id}),
        )
        .await?;
    }
    let view = load_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(view)
}

// ---------------------------------------------------------------------------
// Promote and demote site administrators
// ---------------------------------------------------------------------------

async fn promote_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AccountView>> {
    let (actor, _) = administrator(&state, &headers).await?;
    Ok(Json(promote_inner(&state, actor.id, id).await?))
}

/// Makes an enabled account a site administrator. Promoting an existing
/// administrator is the requested state and records nothing new.
pub async fn promote_inner(state: &AppState, actor: Uuid, id: Uuid) -> ApiResult<AccountView> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ENABLED_ADMINS_LOCK)
        .execute(&mut *tx)
        .await?;
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT is_admin, suspended FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((is_admin, suspended)) = row else {
        return Err(not_found());
    };
    if suspended {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "a disabled account cannot be promoted",
        ));
    }
    if !is_admin {
        sqlx::query("UPDATE accounts SET is_admin = true WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        event_with(
            &mut tx,
            None,
            actor,
            "account.promoted",
            serde_json::json!({"account_id": id}),
        )
        .await?;
    }
    let view = load_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(view)
}

async fn demote_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<AccountView>> {
    let (actor, _) = administrator(&state, &headers).await?;
    Ok(Json(demote_inner(&state, actor.id, id).await?))
}

/// Removes site administration. The last enabled administrator cannot be
/// demoted: taking the same advisory lock as disable makes that rule exact
/// under concurrency.
pub async fn demote_inner(state: &AppState, actor: Uuid, id: Uuid) -> ApiResult<AccountView> {
    if actor == id {
        // The page never offers "Demote" on one's own row; the API matches it
        // so a usable administrator cannot strand the site as a self-demote.
        return Err(ApiError(
            StatusCode::CONFLICT,
            "an administrator cannot demote their own account",
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ENABLED_ADMINS_LOCK)
        .execute(&mut *tx)
        .await?;
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT is_admin, suspended FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((is_admin, suspended)) = row else {
        return Err(not_found());
    };
    if !is_admin {
        // Already a member: the state the caller asked for, with no new record.
        return load_view(&mut tx, id).await;
    }
    if !suspended {
        // Count only administrators who can actually sign in: a reset-pending
        // one cannot, so it must not satisfy the last-administrator rule.
        let another: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM accounts \
             WHERE is_admin AND NOT suspended AND NOT must_change_password AND id <> $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if !another {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "the last enabled administrator cannot be demoted",
            ));
        }
    }
    sqlx::query("UPDATE accounts SET is_admin = false WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    event_with(
        &mut tx,
        None,
        actor,
        "account.demoted",
        serde_json::json!({"account_id": id}),
    )
    .await?;
    let view = load_view(&mut tx, id).await?;
    tx.commit().await?;
    Ok(view)
}

// ---------------------------------------------------------------------------
// Re-authentication (sudo mode)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reauthenticate {
    password: String,
}

/// Refreshes the current session's `authenticated_at` after the account
/// re-enters its password, restoring the fresh-auth window sensitive
/// account-administration actions require. A wrong password is charged to the
/// same bounds as a failed sign-in.
async fn reauthenticate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Reauthenticate>,
) -> ApiResult<StatusCode> {
    let token_hash = session_credential(&headers)?;
    let (account, _) = authenticate_token_hash(&state.pool, token_hash.clone()).await?;
    reauthenticate_session(&state, &account, &token_hash, &input.password).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shared by the JSON endpoint and the settings page. A wrong password is
/// charged to the same bounds as a failed sign-in.
pub(crate) async fn reauthenticate_session(
    state: &AppState,
    account: &Account,
    token_hash: &[u8],
    password: &str,
) -> ApiResult<()> {
    let request = login_throttle::Request::from_current();
    login_throttle::enforce(&state.pool, &state.config, request, &account.username).await?;
    if crate::verify_password(state, &account.username, password)
        .await?
        .is_none()
    {
        login_throttle::failed(&state.pool, request, &account.username).await?;
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "current password is incorrect",
        ));
    }
    login_throttle::succeeded(&state.pool, &account.username).await?;
    sqlx::query("UPDATE sessions SET authenticated_at = clock_timestamp() WHERE token_hash = $1")
        .bind(token_hash)
        .execute(&state.pool)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    current_password: String,
    new_password: Secret,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordReset {}

/// The one-time code returned to the administrator by a password reset. The
/// code is shown once, is never stored in plaintext and is never logged.
#[derive(Serialize)]
pub struct ResetCode {
    pub code: String,
    pub expires_at: String,
}

/// How long a reset code stays redeemable.
const PASSWORD_RESET_TTL_MINUTES: i64 = 60;

/// Issues a single-use, expiring password-reset code in the caller's
/// transaction and retires every earlier unredeemed code for the account.
/// Returns the plaintext code (shown once, never stored) and its expiry. Both
/// the administrator API and the host `reset-admin` command use this one
/// function, so both issue the same credential into the same table and redeem
/// it through the same path.
pub async fn issue_password_reset_code(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: Uuid,
) -> Result<(String, String), sqlx::Error> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let code = hex::encode(bytes);
    let token_hash = Sha256::digest(code.as_bytes()).to_vec();
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO password_reset_tokens (id, account_id, token_hash, expires_at) \
         VALUES ($1, $2, $3, clock_timestamp() + $4 * interval '1 minute') \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(&token_hash)
    .bind(PASSWORD_RESET_TTL_MINUTES as f64)
    .fetch_one(&mut **tx)
    .await?;
    Ok((code, expires_at))
}

/// The account's own password change; the current password is required and
/// checked through the same throttled sign-in as a login, so this endpoint
/// cannot be used to guess passwords faster than the sign-in bounds allow.
async fn change_own_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> ApiResult<StatusCode> {
    let token_hash = session_credential(&headers)?;
    let (account, _) = authenticate_token_hash(&state.pool, token_hash.clone()).await?;
    change_password(
        &state,
        account.id,
        Some(token_hash),
        &input.current_password,
        input.new_password,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shared by the JSON handler and the settings page's password form. Revokes
/// every session of the account except `keep` (the session that made the
/// change, if any), and every live access token and SSH key, in the same
/// transaction that stores the new hash. An own change therefore keeps the
/// session that made it (the existing product behaviour) but no other
/// credential survives.
pub async fn change_password(
    state: &AppState,
    account_id: Uuid,
    keep: Option<Vec<u8>>,
    current_password: &str,
    new_password: Secret,
) -> ApiResult<()> {
    check_password(new_password.expose())?;
    let username: Option<String> =
        sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1 AND NOT suspended")
            .bind(account_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(username) = username else {
        return Err(crate::unauthorized());
    };
    // A wrong current password is charged to the same failed-sign-in bounds as
    // a wrong login, and a throttled attempt is refused before hashing.
    let request = login_throttle::Request::from_current();
    login_throttle::enforce(&state.pool, &state.config, request, &username).await?;
    if crate::verify_password(state, &username, current_password)
        .await?
        .is_none()
    {
        login_throttle::failed(&state.pool, request, &username).await?;
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "current password is incorrect",
        ));
    }
    login_throttle::succeeded(&state.pool, &username).await?;
    let hash = hash_password_async(state, new_password).await?;
    let mut tx = state.pool.begin().await?;
    let revoked = replace_password(&mut tx, account_id, &hash, keep.as_deref()).await?;
    event_with(
        &mut tx,
        None,
        account_id,
        "account.password_changed",
        serde_json::json!({
            "account_id": account_id,
            "sessions_revoked": revoked.sessions,
            "access_tokens_revoked": revoked.access_tokens,
            "ssh_keys_revoked": revoked.ssh_keys,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// How many credentials of each class a password change or reset revoked.
pub(crate) struct Revoked {
    pub(crate) sessions: u64,
    pub(crate) access_tokens: u64,
    pub(crate) ssh_keys: u64,
}

/// Revokes every live credential of the account except the session `keep`, all
/// in the caller's transaction. Access tokens and SSH keys are revoked rather
/// than deleted so an operator can still see that they existed.
async fn revoke_credentials(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: Uuid,
    keep: Option<&[u8]>,
) -> ApiResult<Revoked> {
    let sessions = sqlx::query(
        "DELETE FROM sessions WHERE account_id = $1 AND token_hash <> COALESCE($2, '\\x'::bytea)",
    )
    .bind(account_id)
    .bind(keep)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let access_tokens = sqlx::query(
        "UPDATE access_tokens SET revoked_at = clock_timestamp() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let ssh_keys = sqlx::query(
        "UPDATE ssh_keys SET revoked_at = clock_timestamp() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(Revoked {
        sessions,
        access_tokens,
        ssh_keys,
    })
}

/// Stores a new hash, clears `must_change_password`, revokes every credential
/// of the account except `keep` and clears its failed-sign-in counter, all in
/// the caller's transaction. The counter is keyed by a hash of the username,
/// which this reads inside the transaction so the cleared rows match
/// [`login_throttle`].
pub(crate) async fn replace_password(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: Uuid,
    hash: &str,
    keep: Option<&[u8]>,
) -> ApiResult<Revoked> {
    let username: String = sqlx::query_scalar(
        "UPDATE accounts SET password_hash = $2, must_change_password = false \
         WHERE id = $1 RETURNING username",
    )
    .bind(account_id)
    .bind(hash)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(not_found)?;
    let revoked = revoke_credentials(tx, account_id, keep).await?;
    sqlx::query("DELETE FROM login_attempts WHERE scope = 'username' AND subject = $1")
        .bind(login_throttle::subject_username(&username))
        .execute(&mut **tx)
        .await?;
    Ok(revoked)
}

async fn reset_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    // An administrator-chosen password is refused: only the reset code is
    // accepted. `{}` or no body is the expected request.
    _body: Option<Json<PasswordReset>>,
) -> ApiResult<Json<ResetCode>> {
    let (actor, _) = administrator(&state, &headers).await?;
    Ok(Json(reset_password_inner(&state, actor.id, id).await?))
}

/// Shared by the JSON handler and the administration page's "Reset password"
/// form. The administrator never supplies a password: the account receives a
/// single-use, expiring code, its existing credentials are revoked and it may
/// do nothing until it redeems the code and sets its own password.
pub async fn reset_password_inner(state: &AppState, actor: Uuid, id: Uuid) -> ApiResult<ResetCode> {
    let mut tx = state.pool.begin().await?;
    let username: Option<String> =
        sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(username) = username else {
        return Err(not_found());
    };
    sqlx::query("UPDATE accounts SET must_change_password = true WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let revoked = revoke_credentials(&mut tx, id, None).await?;
    sqlx::query("DELETE FROM login_attempts WHERE scope = 'username' AND subject = $1")
        .bind(login_throttle::subject_username(&username))
        .execute(&mut *tx)
        .await?;
    // A new reset supersedes any earlier unredeemed code.
    let (code, expires_at) = issue_password_reset_code(&mut tx, id).await?;
    event_with(
        &mut tx,
        None,
        actor,
        "account.password_reset",
        // The event names the account and what was revoked; it never carries
        // the reset code or any password.
        serde_json::json!({
            "account_id": id,
            "sessions_revoked": revoked.sessions,
            "access_tokens_revoked": revoked.access_tokens,
            "ssh_keys_revoked": revoked.ssh_keys,
            "reset_expires_at": expires_at,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(ResetCode { code, expires_at })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemReset {
    username: String,
    code: String,
    new_password: Secret,
}

/// The only action a `must_change_password` account may take: redeem its
/// administrator-issued code and store a new password. The code is single-use
/// and expiring, and the plaintext is only ever held in memory.
async fn redeem_password_reset(
    State(state): State<AppState>,
    Json(input): Json<RedeemReset>,
) -> ApiResult<StatusCode> {
    redeem_password_reset_inner(&state, &input.username, &input.code, input.new_password).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn redeem_password_reset_inner(
    state: &AppState,
    username: &str,
    code: &str,
    new_password: Secret,
) -> ApiResult<()> {
    check_password(new_password.expose())?;
    if !crate::valid_username(username)
        || code.len() != 64
        || !code.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired reset code",
        ));
    }
    let token_hash = Sha256::digest(code.as_bytes()).to_vec();
    // Validate the code before hashing. This public endpoint has no throttle,
    // so a wrong code must cost one indexed read-only SELECT, not an Argon2
    // hash that would hold an authentication slot: otherwise one
    // unauthenticated client could starve every password path.
    let live: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM password_reset_tokens t JOIN accounts a ON a.id = t.account_id \
         WHERE a.username = $1 AND t.token_hash = $2 AND t.used_at IS NULL \
         AND t.expires_at > clock_timestamp() AND t.purpose = 'administrator'",
    )
    .bind(username)
    .bind(&token_hash)
    .fetch_optional(&state.pool)
    .await?;
    if live.is_none() {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired reset code",
        ));
    }
    // Only a live code pays for the hash.
    let hash = hash_password_async(state, new_password).await?;
    let mut tx = state.pool.begin().await?;
    // Re-check and claim under the row lock: two concurrent redemptions of the
    // same code produce exactly one success.
    let token_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM password_reset_tokens t JOIN accounts a ON a.id = t.account_id \
         WHERE a.username = $1 AND t.token_hash = $2 AND t.used_at IS NULL \
         AND t.expires_at > clock_timestamp() AND t.purpose = 'administrator' \
         FOR UPDATE OF t",
    )
    .bind(username)
    .bind(&token_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(token_id) = token_id else {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired reset code",
        ));
    };
    let account_id: Uuid = sqlx::query_scalar(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE id = $1 RETURNING account_id",
    )
    .bind(token_id)
    .fetch_one(&mut *tx)
    .await?;
    let revoked = replace_password(&mut tx, account_id, &hash, None).await?;
    // One code is enough: retire every other outstanding code for the account.
    sqlx::query(
        "UPDATE password_reset_tokens SET used_at = clock_timestamp() \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        account_id,
        "account.password_reset_redeemed",
        serde_json::json!({
            "account_id": account_id,
            "sessions_revoked": revoked.sessions,
            "access_tokens_revoked": revoked.access_tokens,
            "ssh_keys_revoked": revoked.ssh_keys,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print_and_deserialize_from_json() {
        let secret = Secret::new("correct horse battery staple");
        assert_eq!(format!("{secret:?}"), "Secret(\"[redacted]\")");
        assert!(!format!("{secret:?}").contains("correct"));
        let parsed: NewAccount =
            serde_json::from_str(r#"{"username":"alice","password":"a long enough password"}"#)
                .unwrap();
        assert_eq!(parsed.password.expose(), "a long enough password");
        assert_eq!(parsed.username, "alice");
        // Unknown fields are refused, so a typo cannot silently drop a password.
        assert!(
            serde_json::from_str::<NewAccount>(
                r#"{"username":"alice","password":"a long enough password","admn":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn password_bounds_match_the_operator_bootstrap() {
        assert!(check_password("").is_err());
        assert!(check_password("short").is_err());
        assert!(check_password(&"a".repeat(11)).is_err());
        assert!(check_password(&"a".repeat(12)).is_ok());
        assert!(check_password(&format!("{}\0", "a".repeat(12))).is_err());
        assert!(check_password(&"a".repeat(1024)).is_ok());
        assert!(check_password(&"a".repeat(1025)).is_err());
    }

    #[test]
    fn usernames_follow_the_bootstrap_rule() {
        assert!(check_username("alice").is_ok());
        assert!(check_username("a-b_9").is_ok());
        assert!(check_username("ab").is_err());
        assert!(check_username("Alice").is_err());
        assert!(check_username("alice smith").is_err());
        assert!(check_username("").is_err());
    }
}
