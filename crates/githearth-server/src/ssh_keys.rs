//! SSH public keys for Git transport (ADR 0005, Person A).
//!
//! Session-authenticated only: a personal access token authenticates its
//! account but can never create, list or revoke SSH keys. The server is built
//! without RSA (see the `russh` dependency comment), so only Ed25519 and ECDSA
//! (nistp256/384/521) keys are accepted; RSA, DSA, `sk-*` and certificates are
//! refused with 400.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    Account, ApiError, AppState, Credential, authenticate_token_hash, bearer_credential,
    event_with, invalid, ssh,
};

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/user/ssh-keys",
            axum::routing::post(create).get(list),
        )
        .route("/api/v1/user/ssh-keys/{id}", axum::routing::delete(revoke))
}

/// Cap the input OpenSSH line: real keys are a few hundred bytes, and 16 KiB is
/// far above any Ed25519/ECDSA public key while still bounding parse work.
const MAX_PUBLIC_KEY_LEN: usize = 16 * 1024;

fn name_ok(name: &str) -> bool {
    (1..=100).contains(&name.chars().count()) && !name.trim().is_empty()
}

/// Accept only Ed25519 and ECDSA (nistp256/384/521). RSA and DSA are refused
/// because the server is built without the `rsa` feature (RUSTSEC-2023-0071);
/// `sk-*` and certificates are out of scope for this phase.
fn algorithm_ok(algorithm: &Algorithm) -> bool {
    matches!(
        algorithm,
        Algorithm::Ed25519
            | Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256 | EcdsaCurve::NistP384 | EcdsaCurve::NistP521
            }
    )
}

/// Authenticates a session only: an access token must not manage SSH keys.
async fn session_auth(pool: &PgPool, headers: &axum::http::HeaderMap) -> ApiResult<(Account, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot manage ssh keys",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSshKey {
    name: String,
    public_key: String,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

const SSH_KEY_COLUMNS: &str = "id, name, algorithm, fingerprint, \
  to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
  to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at, \
  to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS last_used_at, \
  to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS revoked_at";

#[derive(Serialize, FromRow)]
pub(crate) struct SshKeyMeta {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) algorithm: String,
    pub(crate) fingerprint: String,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) revoked_at: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CreatedSshKey {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) algorithm: String,
    pub(crate) fingerprint: String,
    pub(crate) public_key: String,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
}

async fn create(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<NewSshKey>,
) -> ApiResult<(StatusCode, Json<CreatedSshKey>)> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    let created = create_ssh_key(
        &state,
        account.id,
        &input.name,
        &input.public_key,
        input.expires_in_days,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Shared by the JSON handler and the settings page's "Add SSH key" form.
/// Validation (name, expiry, key parsing and the Ed25519/ECDSA-only rule) stays
/// here, so both callers fail identically.
pub(crate) async fn create_ssh_key(
    state: &AppState,
    account_id: Uuid,
    name: &str,
    public_key: &str,
    expires_in_days: Option<i64>,
) -> ApiResult<CreatedSshKey> {
    if !name_ok(name) {
        return Err(invalid());
    }
    if let Some(days) = expires_in_days
        && !(1..=3650).contains(&days)
    {
        return Err(invalid());
    }
    if public_key.len() > MAX_PUBLIC_KEY_LEN {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "public key input too long",
        ));
    }

    // Parse first: the server is built without RSA, and only Ed25519/ECDSA are
    // accepted. Certificate lines fail to parse, so they become a 400 too.
    let key = PublicKey::from_openssh(public_key).map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "must be an Ed25519 or ECDSA (nistp256/384/521) public key; RSA and DSA are not supported",
        )
    })?;
    if !algorithm_ok(&key.algorithm()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "must be an Ed25519 or ECDSA (nistp256/384/521) public key; RSA and DSA are not supported",
        ));
    }
    let algorithm = match key.algorithm() {
        Algorithm::Ed25519 => "ed25519".to_owned(),
        Algorithm::Ecdsa { curve } => match curve {
            EcdsaCurve::NistP256 => "ecdsa-nistp256".to_owned(),
            EcdsaCurve::NistP384 => "ecdsa-nistp384".to_owned(),
            EcdsaCurve::NistP521 => "ecdsa-nistp521".to_owned(),
        },
        _ => unreachable!("algorithm_ok rejected everything else"),
    };
    let canonical_key = {
        let mut canonical = key.clone();
        canonical.set_comment("");
        canonical
            .to_openssh()
            .map_err(|_| invalid())?
            .trim()
            .to_owned()
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let id = Uuid::new_v4();

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO ssh_keys (id, account_id, name, algorithm, public_key, fingerprint, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7::bigint IS NULL THEN NULL \
         ELSE clock_timestamp() + $7 * interval '1 day' END)",
    )
    .bind(id)
    .bind(account_id)
    .bind(name)
    .bind(&algorithm)
    .bind(&canonical_key)
    .bind(&fingerprint)
    .bind(expires_in_days)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if e.as_database_error()
            .is_some_and(|db| db.is_unique_violation())
        {
            return ApiError(
                StatusCode::CONFLICT,
                "ssh key already registered",
            );
        }
        ApiError::from(e)
    })?;
    event_with(
        &mut tx,
        None,
        account_id,
        "ssh_key.created",
        serde_json::json!({"key_id": id, "fingerprint": fingerprint}),
    )
    .await?;
    let row = sqlx::query(&format!(
        "SELECT {SSH_KEY_COLUMNS} FROM ssh_keys WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(CreatedSshKey {
        id,
        name: name.to_owned(),
        algorithm,
        fingerprint,
        public_key: canonical_key,
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
    })
}

async fn list(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Vec<SshKeyMeta>>> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    Ok(Json(list_ssh_keys(&state, account.id).await?))
}

/// Shared by the JSON handler and the settings page: the account's keys in a
/// stable order.
pub(crate) async fn list_ssh_keys(
    state: &AppState,
    account_id: Uuid,
) -> ApiResult<Vec<SshKeyMeta>> {
    let rows = sqlx::query_as::<_, SshKeyMeta>(&format!(
        "SELECT {SSH_KEY_COLUMNS} FROM ssh_keys WHERE account_id = $1 ORDER BY created_at, id"
    ))
    .bind(account_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows)
}

async fn revoke(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    revoke_ssh_key(&state, account.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shared by the JSON handler and the settings page's "Revoke" button. A
/// missing id or another account's key is a 404 for both callers.
pub(crate) async fn revoke_ssh_key(state: &AppState, account_id: Uuid, id: Uuid) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    let fingerprint: String = sqlx::query_scalar(
        "UPDATE ssh_keys SET revoked_at = clock_timestamp() \
         WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL RETURNING fingerprint",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError(StatusCode::NOT_FOUND, "ssh key not found"))?;
    event_with(
        &mut tx,
        None,
        account_id,
        "ssh_key.revoked",
        serde_json::json!({"key_id": id, "fingerprint": fingerprint}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Authenticates an SSH connection by fingerprint (ADR 0005). Returns `None`
/// for an unknown, revoked or expired key, or a suspended account.
/// `last_used_at` is refreshed, throttled the same way as tokens (only when
/// stale by more than 60 seconds).
pub async fn authenticate(
    conn: &mut PgConnection,
    fingerprint: &str,
) -> Result<Option<ssh::SshIdentity>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, bool)>(
        "SELECT k.id, k.account_id, (k.expires_at IS NOT NULL AND k.expires_at <= clock_timestamp()) AS expired \
         FROM ssh_keys k JOIN accounts a ON a.id = k.account_id \
         WHERE k.fingerprint = $1 AND k.revoked_at IS NULL AND NOT a.suspended",
    )
    .bind(fingerprint)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((key_id, account_id, expired)) = row else {
        return Ok(None);
    };
    if expired {
        return Ok(None);
    }
    // Throttle the last_used_at refresh: touch only when stale by >60s.
    let _ = sqlx::query(
        "UPDATE ssh_keys SET last_used_at = clock_timestamp() \
         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < clock_timestamp() - interval '60 seconds')",
    )
    .bind(key_id)
    .execute(&mut *conn)
    .await?;
    Ok(Some(ssh::SshIdentity {
        account: account_id,
        key_id,
        read_only: false,
    }))
}

/// The injected closure the SSH listener calls per verified public key.
pub fn authenticator(pool: PgPool) -> ssh::Authenticate {
    Arc::new(move |fingerprint: String| {
        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool.acquire().await?;
            authenticate(&mut conn, &fingerprint).await
        })
    })
}
