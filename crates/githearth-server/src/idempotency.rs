use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use std::future::Future;
use std::time::Duration;
use uuid::Uuid;

use crate::ApiError;

/// A `running` claim older than this is abandoned and may be re-claimed.
const RUNNING_LEASE: Duration = Duration::from_secs(60);

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

/// Handle to the current claim, passed to the handler so it writes the stored
/// response in the SAME transaction as its mutation. `key` is `None` when the
/// request carried no idempotency key, in which case every method is a no-op.
pub struct Idem {
    actor_id: Uuid,
    key: Option<String>,
    /// Set when an abandoned `running` claim was re-claimed: the operation id
    /// the previous attempt stored, for the create handler to reuse its row.
    pub prior_operation_id: Option<Uuid>,
}

impl Idem {
    pub async fn store(
        &self,
        conn: &mut PgConnection,
        status: StatusCode,
        body: &Value,
    ) -> Result<(), sqlx::Error> {
        let Some(key) = &self.key else {
            return Ok(());
        };
        sqlx::query(
            "UPDATE operations SET status = 'done', response_status = $1, response_body = $2 \
             WHERE actor_id = $3 AND key = $4 AND status = 'running'",
        )
        .bind(status.as_u16() as i32)
        .bind(body)
        .bind(self.actor_id)
        .bind(key)
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Persist the created object's operation id on the claim, in the same
    /// transaction that creates the object, so a retry can find it.
    pub async fn record_operation(
        &self,
        conn: &mut PgConnection,
        operation_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let Some(key) = &self.key else {
            return Ok(());
        };
        sqlx::query("UPDATE operations SET operation_id = $1 WHERE actor_id = $2 AND key = $3")
            .bind(operation_id)
            .bind(self.actor_id)
            .bind(key)
            .execute(conn)
            .await?;
        Ok(())
    }
}

async fn remove(conn: &mut PgConnection, actor_id: Uuid, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM operations WHERE actor_id = $1 AND key = $2")
        .bind(actor_id)
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// Try to claim the key atomically. Returns the stored reply, 409/422, or runs
/// the handler (optionally carrying the operation id of a re-claimed claim).
enum Outcome {
    Run { prior_operation_id: Option<Uuid> },
    Reply(StatusCode, Value),
    Conflict,
    Unprocessable,
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
    F: FnOnce(Idem) -> Fut,
    Fut: Future<Output = Result<(StatusCode, Value), ApiError>>,
{
    let Some(key) = key else {
        return f(Idem {
            actor_id,
            key: None,
            prior_operation_id: None,
        })
        .await;
    };
    let hash = request_hash(method, path, raw_body);
    let claimed = check(
        &mut *pool.acquire().await?,
        actor_id,
        key,
        &hash,
        RUNNING_LEASE.as_secs() as f64,
    )
    .await?;
    let prior_operation_id = match claimed {
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
        Outcome::Run { prior_operation_id } => prior_operation_id,
    };
    let idem = Idem {
        actor_id,
        key: Some(key.to_string()),
        prior_operation_id,
    };
    let result = f(idem).await;
    if result.is_err() {
        // Never leave the key `running`: the client could not retry for 24 hours.
        let mut conn = pool.acquire().await?;
        let _ = remove(&mut conn, actor_id, key).await;
    }
    result
}

/// request_hash, status, response_status, response_body, operation_id
type StoredRow = (Vec<u8>, String, Option<i32>, Option<Value>, Option<Uuid>);

async fn check(
    conn: &mut PgConnection,
    actor_id: Uuid,
    key: &str,
    hash: &[u8],
    lease_seconds: f64,
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
        return Ok(Outcome::Run {
            prior_operation_id: None,
        });
    }

    // An abandoned `running` claim (crash between commit and store) may be re-claimed.
    let reclaimed = sqlx::query(
        "UPDATE operations SET created_at = clock_timestamp() WHERE actor_id = $1 AND key = $2 AND status = 'running' AND created_at < clock_timestamp() - $3 * interval '1 second'",
    )
    .bind(actor_id)
    .bind(key)
    .bind(lease_seconds)
    .execute(&mut *conn)
    .await?;
    let reclaimed = reclaimed.rows_affected() > 0;

    let row: Option<StoredRow> = sqlx::query_as(
        "SELECT request_hash, status, response_status, response_body, operation_id FROM operations WHERE actor_id = $1 AND key = $2",
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
        if reclaimed {
            // We took over an abandoned attempt; only the same request may continue it.
            if row.0.as_slice() == hash {
                return Ok(Outcome::Run {
                    prior_operation_id: row.4,
                });
            }
            return Ok(Outcome::Unprocessable);
        }
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
