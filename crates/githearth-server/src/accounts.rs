//! Account administration (M4.1 U01).
//!
//! Site administration: only a site administrator may list, create, disable or
//! enable an account, and every decision goes through the authorization engine
//! as an `Instance` resource (see `docs/decisions/0002-authorization.md` and the
//! `account.*` actions). A disabled account keeps `suspended = true` plus
//! `disabled_at`/`disabled_by`, so the existing `suspended-account` forbid
//! denies it everywhere and re-enabling clears the bookkeeping.
//!
//! Passwords are created and reset by `crate::passwords`; this module never
//! generates or emails a plaintext password.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use githearth_authz::{Action, Resource};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::{ApiError, AppState, event_with, facts, has_nul, invalid};

type ApiResult<T> = Result<T, ApiError>;

/// Serializes concurrent account-administration writes so the "last
/// administrator" check cannot race another disable (distinct from the
/// bootstrap lock in `lib.rs`).
const ACCOUNT_ADMIN_LOCK: i64 = 706_918_232;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/admin/accounts", post(create).get(list))
        .route("/api/v1/admin/accounts/{id}/disable", post(disable))
        .route("/api/v1/admin/accounts/{id}/enable", post(enable))
}

const ACCOUNT_COLUMNS: &str = "id, username, is_admin, \
 (suspended OR disabled_at IS NOT NULL) AS disabled, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(password_changed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS password_changed_at";

#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct AccountSummary {
    pub(crate) id: Uuid,
    pub(crate) username: String,
    pub(crate) is_admin: bool,
    pub(crate) disabled: bool,
    pub(crate) created_at: String,
    pub(crate) password_changed_at: Option<String>,
}

/// Authorizes one instance-level action for `account_id`. A disabled account
/// (and a non-administrator) is refused by the built-in policies.
pub(crate) async fn authorize_instance(
    state: &AppState,
    account_id: Uuid,
    auth_age: u64,
    action: Action,
) -> ApiResult<()> {
    let mut conn = state.pool.acquire().await?;
    let facts = facts::load_site(&mut conn, account_id)
        .await?
        .ok_or_else(crate::unauthorized)?;
    drop(conn);
    let decision = state.authz.authorize(&facts.request(
        action,
        Resource::Instance,
        Some(Duration::from_secs(auth_age)),
    ));
    if decision.is_allowed() {
        Ok(())
    } else {
        Err(ApiError(StatusCode::FORBIDDEN, "forbidden"))
    }
}

/// Accepted password shape for every credential path: 12..=1024 bytes, no NUL.
pub(crate) fn password_ok(password: &str) -> bool {
    (12..=1024).contains(&password.len()) && !has_nul(password)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAccount {
    username: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewAccount>,
) -> ApiResult<(StatusCode, Json<AccountSummary>)> {
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    authorize_instance(&state, actor.id, auth_age, Action::AccountCreate).await?;
    let created = create_account(
        &state,
        actor.id,
        &input.username,
        &input.password,
        input.is_admin,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Shared by the JSON handler and the admin page: validation, hashing, insert
/// and audit stay in one place so both callers behave identically.
pub(crate) async fn create_account(
    state: &AppState,
    actor: Uuid,
    username: &str,
    password: &str,
    is_admin: bool,
) -> ApiResult<AccountSummary> {
    if !crate::valid_username(username) || !password_ok(password) {
        return Err(invalid());
    }
    let password = password.to_owned();
    let hash = tokio::task::spawn_blocking(move || crate::hash_password(&password))
        .await
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed"))?
        .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed"))?;
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin, password_changed_at) \
         VALUES ($1, $2, $3, $4, clock_timestamp())",
    )
    .bind(id)
    .bind(username)
    .bind(&hash)
    .bind(is_admin)
    .execute(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        actor,
        "account.created",
        serde_json::json!({"account_id": id, "is_admin": is_admin}),
    )
    .await?;
    let row = sqlx::query_as::<_, AccountSummary>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<AccountSummary>>> {
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    authorize_instance(&state, actor.id, auth_age, Action::AccountRead).await?;
    Ok(Json(list_accounts(&state).await?))
}

/// The account list in a stable order, shared by the JSON handler and the page.
pub(crate) async fn list_accounts(state: &AppState) -> ApiResult<Vec<AccountSummary>> {
    Ok(sqlx::query_as::<_, AccountSummary>(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY username"
    ))
    .fetch_all(&state.pool)
    .await?)
}

async fn disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    authorize_instance(&state, actor.id, auth_age, Action::AccountDisable).await?;
    set_disabled(&state, actor.id, id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    authorize_instance(&state, actor.id, auth_age, Action::AccountEnable).await?;
    set_disabled(&state, actor.id, id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Disables or re-enables an account. Idempotent: the second call is a no-op
/// with no audit event. Refuses to disable the last active administrator, so
/// administration cannot be locked out.
pub(crate) async fn set_disabled(
    state: &AppState,
    actor: Uuid,
    target: Uuid,
    disabled: bool,
) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ACCOUNT_ADMIN_LOCK)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT is_admin, (suspended OR disabled_at IS NOT NULL) AS disabled \
         FROM accounts WHERE id = $1 FOR UPDATE",
    )
    .bind(target)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError(StatusCode::NOT_FOUND, "account not found"))?;
    let is_admin: bool = row.get("is_admin");
    let already: bool = row.get("disabled");
    if disabled && !already && is_admin {
        let others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM accounts \
             WHERE is_admin AND NOT suspended AND disabled_at IS NULL AND id <> $1",
        )
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        if others == 0 {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "cannot disable the last administrator",
            ));
        }
    }
    if disabled == already {
        return Ok(());
    }
    if disabled {
        sqlx::query(
            "UPDATE accounts SET suspended = true, disabled_at = clock_timestamp(), \
             disabled_by = $2 WHERE id = $1",
        )
        .bind(target)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        event_with(
            &mut tx,
            None,
            actor,
            "account.disabled",
            serde_json::json!({"account_id": target}),
        )
        .await?;
    } else {
        sqlx::query(
            "UPDATE accounts SET suspended = false, disabled_at = NULL, disabled_by = NULL \
             WHERE id = $1",
        )
        .bind(target)
        .execute(&mut *tx)
        .await?;
        event_with(
            &mut tx,
            None,
            actor,
            "account.enabled",
            serde_json::json!({"account_id": target}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
