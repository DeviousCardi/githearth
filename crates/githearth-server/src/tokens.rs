//! Personal access tokens API. Session-authenticated only: a token authenticates
//! its account but can never create, list or revoke tokens. The plaintext is
//! shown exactly once, on creation; only its SHA-256 is stored.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Credential, authenticate_token_hash, bearer_credential, event_with, invalid,
};

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/user/tokens", post(create).get(list))
        .route("/api/v1/user/tokens/{id}", delete(revoke))
}

const SCOPES: [&str; 2] = ["repo:read", "repo:write"];

/// Authenticates a session only: an access token must not manage tokens.
async fn session_auth(
    pool: &sqlx::PgPool,
    headers: &HeaderMap,
) -> ApiResult<(crate::Account, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot manage access tokens",
        )),
    }
}

fn scope_ok(scopes: &[String]) -> bool {
    !scopes.is_empty()
        && scopes.len() <= SCOPES.len()
        && scopes.iter().all(|s| SCOPES.contains(&s.as_str()))
}

fn name_ok(name: &str) -> bool {
    (1..=100).contains(&name.chars().count()) && !name.trim().is_empty()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    name: String,
    scopes: Vec<String>,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct TokenMeta {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) revoked_at: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CreatedToken {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
    /// Shown once: the plaintext token string.
    pub(crate) token: String,
}

const TOKEN_COLUMNS: &str = "id, name, scopes, \
  to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
  to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at, \
  to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS last_used_at, \
  to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS revoked_at";

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    let created = create_token(
        &state,
        account.id,
        &input.name,
        &input.scopes,
        input.expires_in_days,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Shared by the JSON handler and the settings page's "Create token" form.
/// Validation stays here: a bad name, scope set or expiry fails both callers
/// identically. Returns the plaintext exactly once.
pub(crate) async fn create_token(
    state: &AppState,
    account_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_in_days: Option<i64>,
) -> ApiResult<CreatedToken> {
    if !name_ok(name) || !scope_ok(scopes) {
        return Err(invalid());
    }
    if let Some(days) = expires_in_days
        && !(1..=3650).contains(&days)
    {
        return Err(invalid());
    }

    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let plaintext = format!("gth_{}", hex::encode(bytes));
    let token_hash = Sha256::digest(plaintext.as_bytes()).to_vec();
    let id = Uuid::new_v4();

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO access_tokens (id, account_id, name, token_hash, scopes, expires_at) \
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6::bigint IS NULL THEN NULL \
         ELSE clock_timestamp() + $6 * interval '1 day' END)",
    )
    .bind(id)
    .bind(account_id)
    .bind(name)
    .bind(token_hash)
    .bind(scopes)
    .bind(expires_in_days)
    .execute(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        None,
        account_id,
        "token.created",
        serde_json::json!({"token_id": id}),
    )
    .await?;
    let row = sqlx::query(&format!(
        "SELECT {TOKEN_COLUMNS} FROM access_tokens WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(CreatedToken {
        id,
        name: name.to_owned(),
        scopes: scopes.to_vec(),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        token: plaintext,
    })
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<TokenMeta>>> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    Ok(Json(list_tokens(&state, account.id).await?))
}

/// Shared by the JSON handler and the settings page: the account's tokens in
/// a stable order.
pub(crate) async fn list_tokens(state: &AppState, account_id: Uuid) -> ApiResult<Vec<TokenMeta>> {
    let rows = sqlx::query_as::<_, TokenMeta>(&format!(
        "SELECT {TOKEN_COLUMNS} FROM access_tokens WHERE account_id = $1 ORDER BY created_at, id"
    ))
    .bind(account_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows)
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    revoke_token(&state, account.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shared by the JSON handler and the settings page's "Revoke" button. A
/// missing id or another account's token is a 404 for both callers.
pub(crate) async fn revoke_token(state: &AppState, account_id: Uuid, id: Uuid) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    let result = sqlx::query(
        "UPDATE access_tokens SET revoked_at = clock_timestamp() \
         WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "token not found"));
    }
    event_with(
        &mut tx,
        None,
        account_id,
        "token.revoked",
        serde_json::json!({"token_id": id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
