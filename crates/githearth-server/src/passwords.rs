//! Password change and administrator reset (M4.1 U02).
//!
//! An account may change its own password by presenting the current one; an
//! administrator may reset another account's password through the same
//! `account.reset_password` instance action used by the admin page. Both paths
//! re-hash the password, bump `password_changed_at` and delete *every* session
//! of the account, so a stolen session cannot survive a credential change. No
//! path generates or emails a plaintext password.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use githearth_authz::Action;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    Account, ApiError, AppState, Credential, accounts, authenticate_token_hash, bearer_credential,
    event_with, invalid,
};

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/admin/accounts/{id}/password", post(reset_password))
        .route("/api/v1/user/password", post(change_own_password))
}

/// Authenticates a session only: a personal access token may not change or
/// reset passwords.
async fn session_auth(pool: &sqlx::PgPool, headers: &HeaderMap) -> ApiResult<(Account, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot change passwords",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetPassword {
    new_password: String,
}

async fn reset_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ResetPassword>,
) -> ApiResult<StatusCode> {
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    accounts::authorize_instance(&state, actor.id, auth_age, Action::AccountResetPassword).await?;
    if !accounts::password_ok(&input.new_password) {
        return Err(invalid());
    }
    set_password(
        &state,
        actor.id,
        id,
        input.new_password,
        "account.password_reset",
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}

async fn change_own_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ChangePassword>,
) -> ApiResult<StatusCode> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    if !accounts::password_ok(&input.new_password) {
        return Err(invalid());
    }
    change_password(&state, account, &input.current_password, input.new_password).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Verifies the current password and, on success, replaces it. Shared by the
/// JSON handler so the browser path reaches the same behaviour.
pub(crate) async fn change_password(
    state: &AppState,
    account: Account,
    current_password: &str,
    new_password: String,
) -> ApiResult<()> {
    // The current password is the authorization for changing it; a mismatch is
    // a definite refusal, not an outage. A throttled account is reported as 429
    // by `verify_password`.
    match crate::verify_password(state, &account.username, current_password).await? {
        Some(id) if id == account.id => {}
        _ => {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "current password is incorrect",
            ));
        }
    }
    set_password(
        state,
        account.id,
        account.id,
        new_password,
        "account.password_changed",
    )
    .await
}

/// Re-hashes a password, records `password_changed_at`, revokes every session
/// of the account and clears its failed-login history, all in one transaction.
/// `kind` is the audit/outbox event (`account.password_changed` or
/// `account.password_reset`).
pub(crate) async fn set_password(
    state: &AppState,
    actor: Uuid,
    target: Uuid,
    new_password: String,
    kind: &str,
) -> ApiResult<()> {
    let hash = tokio::task::spawn_blocking(move || crate::hash_password(&new_password))
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed"))?
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed"))?;
    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE accounts SET password_hash = $2, password_changed_at = clock_timestamp() \
         WHERE id = $1",
    )
    .bind(target)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "account not found"));
    }
    sqlx::query("DELETE FROM sessions WHERE account_id = $1")
        .bind(target)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM login_failures WHERE account_id = $1")
        .bind(target)
        .execute(&mut *tx)
        .await?;
    event_with(
        &mut tx,
        None,
        actor,
        kind,
        serde_json::json!({"account_id": target}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
