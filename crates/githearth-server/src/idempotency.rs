use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use std::future::Future;
use uuid::Uuid;

use crate::ApiError;

pub fn extract_key(headers: &HeaderMap) -> Option<Result<&str, ApiError>> {
    let key = headers.get("Idempotency-Key")?;
    let key = match key.to_str() {
        Ok(s) => s,
        Err(_) => {
            return Some(Err(ApiError(
                StatusCode::BAD_REQUEST,
                "invalid idempotency key",
            )));
        }
    };
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| (33..=126).contains(&b)) {
        return Some(Err(ApiError(
            StatusCode::BAD_REQUEST,
            "invalid idempotency key",
        )));
    }
    Some(Ok(key))
}

pub fn request_hash(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(path.as_bytes());
    hasher.update(body);
    hasher.finalize().to_vec()
}

/// Try to claim the key atomically. Returns the stored reply, 409/422, or
/// lets the caller run the handler.
enum Outcome {
    Run,
    Reply(StatusCode, Value),
    Conflict,
    Unprocessable,
}

async fn store(
    conn: &mut PgConnection,
    actor_id: Uuid,
    key: &str,
    status: StatusCode,
    body: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE operations SET status = 'done', response_status = $1, response_body = $2 WHERE actor_id = $3 AND key = $4")
        .bind(status.as_u16() as i32)
        .bind(body)
        .bind(actor_id)
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

async fn remove(conn: &mut PgConnection, actor_id: Uuid, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM operations WHERE actor_id = $1 AND key = $2")
        .bind(actor_id)
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// Holds no connection while `f` runs, so a request never needs two at once.
pub async fn run<F, Fut>(
    pool: &PgPool,
    actor_id: Uuid,
    method: &str,
    path: &str,
    raw_body: &[u8],
    key: Option<&str>,
    f: F,
) -> Result<(StatusCode, Value), ApiError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(StatusCode, Value), ApiError>>,
{
    let Some(key) = key else {
        return f().await;
    };
    let hash = request_hash(method, path, raw_body);
    let claimed = check(&mut *pool.acquire().await?, actor_id, key, &hash).await?;
    match claimed {
        Outcome::Reply(status, body) => return Ok((status, body)),
        Outcome::Conflict => {
            return Err(ApiError(StatusCode::CONFLICT, "idempotency key conflict"));
        }
        Outcome::Unprocessable => {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency key reused with different request",
            ));
        }
        Outcome::Run => {}
    }
    let result = f().await;
    let mut conn = pool.acquire().await?;
    let stored = match &result {
        Ok((status, body)) => store(&mut conn, actor_id, key, *status, body).await.is_ok(),
        Err(_) => false,
    };
    // Never leave the key `running`: the client could not retry for 24 hours.
    if !stored {
        let _ = remove(&mut conn, actor_id, key).await;
    }
    result
}

/// request_hash, status, response_status, response_body
type StoredRow = (Vec<u8>, String, Option<i32>, Option<Value>);

async fn check(
    conn: &mut PgConnection,
    actor_id: Uuid,
    key: &str,
    hash: &[u8],
) -> Result<Outcome, ApiError> {
    sqlx::query(
        "DELETE FROM operations WHERE actor_id = $1 AND key = $2 AND expires_at < clock_timestamp()",
    )
    .bind(actor_id)
    .bind(key)
    .execute(&mut *conn)
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1, $2, $3, 'running', clock_timestamp() + interval '24 hours') ON CONFLICT (actor_id, key) DO NOTHING",
    )
    .bind(actor_id)
    .bind(key)
    .bind(hash)
    .execute(&mut *conn)
    .await?;

    if inserted.rows_affected() > 0 {
        return Ok(Outcome::Run);
    }

    let row: Option<StoredRow> = sqlx::query_as(
        "SELECT request_hash, status, response_status, response_body FROM operations WHERE actor_id = $1 AND key = $2",
    )
    .bind(actor_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    // Released by a failing concurrent attempt between our insert and read: still in flight.
    let Some(row) = row else {
        return Ok(Outcome::Conflict);
    };

    if row.1 == "running" {
        return Ok(Outcome::Conflict);
    }
    if row.0.as_slice() == hash {
        let status = row
            .2
            .and_then(|s| StatusCode::from_u16(s as u16).ok())
            .unwrap_or(StatusCode::OK);
        let body = row.3.unwrap_or(Value::Null);
        Ok(Outcome::Reply(status, body))
    } else {
        Ok(Outcome::Unprocessable)
    }
}
