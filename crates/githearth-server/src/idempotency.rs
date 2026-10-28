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

/// Internal marker distinguishing a lost claim from other handler errors.
const LOST_CLAIM: &str = "idempotency claim lost";

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

/// Owns the current claim so the handler writes the stored response in the SAME
/// transaction as its mutation. `key` is `None` when the request carried no
/// idempotency key, in which case every method is a no-op.
pub struct Idem {
    actor_id: Uuid,
    key: Option<String>,
    /// Ownership generation: set when the claim was (freshly or by re-claim)
    /// acquired. Every mutation of the claim row is fenced on it.
    claim_id: Uuid,
    /// Set when an abandoned `running` claim was re-claimed: the operation id
    /// the previous attempt stored, for the handler to reuse its row.
    pub prior_operation_id: Option<Uuid>,
}

/// The earliest error a handler can return when its claim was reclaimed from
/// under it; `run` resolves it against the new owner instead of releasing the key.
pub fn claim_lost() -> ApiError {
    ApiError(StatusCode::CONFLICT, LOST_CLAIM)
}

impl Idem {
    /// Fence the final response store on ownership; returns false (and the
    /// handler must roll back and fail) when the claim was re-claimed. Unkeyed
    /// requests have nothing to store, so it always reports success.
    pub async fn store(
        &self,
        conn: &mut PgConnection,
        status: StatusCode,
        body: &Value,
    ) -> Result<bool, sqlx::Error> {
        let Some(key) = &self.key else {
            return Ok(true);
        };
        let result = sqlx::query(
            "UPDATE operations SET status = 'done', response_status = $1, response_body = $2 \
             WHERE actor_id = $3 AND key = $4 AND claim_id = $5 AND status = 'running'",
        )
        .bind(status.as_u16() as i32)
        .bind(body)
        .bind(self.actor_id)
        .bind(key)
        .bind(self.claim_id)
        .execute(conn)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist the created object's operation id on the claim, in the same
    /// transaction that creates the object, so a retry can find it. Returns false
    /// when ownership was lost (re-claimed), aborting the mutation transaction.
    pub async fn record_operation(
        &self,
        conn: &mut PgConnection,
        operation_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let Some(key) = &self.key else {
            return Ok(true);
        };
        let result = sqlx::query(
            "UPDATE operations SET operation_id = $1 WHERE actor_id = $2 AND key = $3 AND claim_id = $4",
        )
        .bind(operation_id)
        .bind(self.actor_id)
        .bind(key)
        .bind(self.claim_id)
        .execute(conn)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Release a claim a handler failed to complete. Fenced by ownership so a stale
/// handler can never delete a re-claimed claim. A claim whose operation id was
/// already committed is kept `running` so a retry can resume that exact row
/// instead of starting over.
async fn release(
    conn: &mut PgConnection,
    actor_id: Uuid,
    key: &str,
    claim_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM operations WHERE actor_id = $1 AND key = $2 AND claim_id = $3 AND operation_id IS NULL",
    )
    .bind(actor_id)
    .bind(key)
    .bind(claim_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Try to claim the key atomically. Returns the stored reply, 409/422, or runs
/// the handler (carrying the claim ownership and the operation id of a re-claimed claim).
enum Outcome {
    Run {
        claim_id: Uuid,
        prior_operation_id: Option<Uuid>,
    },
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
            claim_id: Uuid::nil(),
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
    match claimed {
        Outcome::Reply(status, body) => Ok((status, body)),
        Outcome::Conflict => Err(ApiError(StatusCode::CONFLICT, "idempotency key conflict")),
        Outcome::Unprocessable => Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency key reused with different request",
        )),
        Outcome::Run {
            claim_id,
            prior_operation_id,
        } => {
            let idem = Idem {
                actor_id,
                key: Some(key.to_string()),
                claim_id,
                prior_operation_id,
            };
            let result = f(idem).await;
            match result {
                Err(ApiError(StatusCode::CONFLICT, LOST_CLAIM)) => {
                    // Re-claimed from under us: do not release, do not corrupt.
                    resolve_lost(pool, actor_id, key, &hash).await
                }
                Err(_) => {
                    // Failing handler still releases the key (fenced by ownership).
                    let mut conn = pool.acquire().await?;
                    let _ = release(&mut conn, actor_id, key, claim_id).await;
                    result
                }
                ok => ok,
            }
        }
    }
}

/// After losing ownership, return the new owner's stored response (if it has
/// finished this same request) or 409 (still running).
async fn resolve_lost(
    pool: &PgPool,
    actor_id: Uuid,
    key: &str,
    hash: &[u8],
) -> Result<(StatusCode, Value), ApiError> {
    let mut conn = pool.acquire().await?;
    let row: Option<StoredRow> = sqlx::query_as(
        "SELECT request_hash, status, response_status, response_body, operation_id FROM operations WHERE actor_id = $1 AND key = $2",
    )
    .bind(actor_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some((request_hash, status, Some(code), Some(body), _)) if status == "done" => {
            if request_hash.as_slice() == hash {
                let code = StatusCode::from_u16(code as u16).unwrap_or(StatusCode::OK);
                Ok((code, body))
            } else {
                Err(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "idempotency key reused with different request",
                ))
            }
        }
        _ => Err(ApiError(StatusCode::CONFLICT, "idempotency key conflict")),
    }
}

/// request_hash, status, response_status, response_body, operation_id
type StoredRow = (Vec<u8>, String, Option<i32>, Option<Value>, Option<Uuid>);

#[derive(sqlx::FromRow)]
struct ClaimRow {
    request_hash: Vec<u8>,
    status: String,
    response_status: Option<i32>,
    response_body: Option<Value>,
    claim_id: Uuid,
    age_seconds: f64,
}

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

    let claim_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, claim_id, expires_at) VALUES ($1, $2, $3, 'running', $4, clock_timestamp() + interval '24 hours') ON CONFLICT (actor_id, key) DO NOTHING",
    )
    .bind(actor_id)
    .bind(key)
    .bind(hash)
    .bind(claim_id)
    .execute(&mut *conn)
    .await?;

    if inserted.rows_affected() > 0 {
        return Ok(Outcome::Run {
            claim_id,
            prior_operation_id: None,
        });
    }

    let row: Option<ClaimRow> = sqlx::query_as(
        "SELECT request_hash, status, response_status, response_body, claim_id, \
                EXTRACT(EPOCH FROM (clock_timestamp() - created_at))::float8 AS age_seconds \
         FROM operations WHERE actor_id = $1 AND key = $2",
    )
    .bind(actor_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    // Released by a failing concurrent attempt between our insert and read: still in flight.
    let Some(row) = row else {
        return Ok(Outcome::Conflict);
    };

    if row.status == "done" {
        if row.request_hash.as_slice() == hash {
            let status = row
                .response_status
                .and_then(|s| StatusCode::from_u16(s as u16).ok())
                .unwrap_or(StatusCode::OK);
            let body = row.response_body.unwrap_or(Value::Null);
            return Ok(Outcome::Reply(status, body));
        }
        return Ok(Outcome::Unprocessable);
    }

    // Still running. Only the same request may re-claim an expired claim; a
    // different-body retry must not renew the lease or it blocks the rightful retry.
    if row.request_hash.as_slice() != hash || row.age_seconds < lease_seconds {
        return Ok(Outcome::Conflict);
    }

    // Abandoned by the same request: take it over (compare-and-swap on the old
    // ownership so two concurrent retries cannot both re-claim). The winning
    // UPDATE's operation id is authoritative: a stale handler may have recorded
    // one between our SELECT above and this UPDATE.
    let new_claim = Uuid::new_v4();
    let reclaimed: Option<(Option<Uuid>,)> = sqlx::query_as(
        "UPDATE operations SET claim_id = $1, created_at = clock_timestamp(), expires_at = clock_timestamp() + interval '24 hours' \
         WHERE actor_id = $2 AND key = $3 AND status = 'running' AND claim_id = $4 \
         RETURNING operation_id",
    )
    .bind(new_claim)
    .bind(actor_id)
    .bind(key)
    .bind(row.claim_id)
    .fetch_optional(&mut *conn)
    .await?;
    match reclaimed {
        Some((operation_id,)) => Ok(Outcome::Run {
            claim_id: new_claim,
            prior_operation_id: operation_id,
        }),
        None => Ok(Outcome::Conflict),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{ConnectOptions, postgres::PgConnectOptions};
    use std::str::FromStr;

    async fn test_pool() -> (PgPool, Uuid) {
        let options = PgConnectOptions::from_str(
            &std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL"),
        )
        .unwrap()
        .disable_statement_logging();
        let pool = PgPool::connect_with(options).await.unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();
        let actor = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
            .bind(actor)
            .bind(format!("idem-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        (pool, actor)
    }

    fn idem(actor: Uuid, key: &str, claim_id: Uuid) -> Idem {
        Idem {
            actor_id: actor,
            key: Some(key.to_string()),
            claim_id,
            prior_operation_id: None,
        }
    }

    async fn insert_abandoned(
        pool: &PgPool,
        actor: Uuid,
        key: &str,
        hash: &[u8],
        operation_id: Option<Uuid>,
    ) -> Uuid {
        let claim = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO operations (actor_id, key, request_hash, status, claim_id, operation_id, created_at, expires_at) \
             VALUES ($1,$2,$3,'running',$4,$5,clock_timestamp() - interval '120 seconds',clock_timestamp() + interval '24 hours')",
        )
        .bind(actor)
        .bind(key)
        .bind(hash)
        .bind(claim)
        .bind(operation_id)
        .execute(pool)
        .await
        .unwrap();
        claim
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn different_body_does_not_renew_lease_and_same_body_still_reclaims() {
        let (pool, actor) = test_pool().await;
        let key = "k-diff";
        let h1 = vec![1u8; 32];
        let h2 = vec![2u8; 32];
        insert_abandoned(&pool, actor, key, &h1, None).await;

        let mut conn = pool.acquire().await.unwrap();
        // A different-body retry of an abandoned claim must not renew the lease.
        let outcome = check(&mut conn, actor, key, &h2, 60.0).await.unwrap();
        assert!(matches!(outcome, Outcome::Conflict));
        let age: f64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (clock_timestamp() - created_at))::float8 FROM operations WHERE actor_id=$1 AND key=$2",
        )
        .bind(actor)
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert!(
            age >= 60.0,
            "different-body retry renewed the lease: age={age}"
        );

        // The rightful same-body retry still re-claims.
        let outcome = check(&mut conn, actor, key, &h1, 60.0).await.unwrap();
        assert!(matches!(outcome, Outcome::Run { .. }));
        drop(conn);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn stale_claim_cannot_store_or_release_a_reclaimed_claim() {
        let (pool, actor) = test_pool().await;
        let key = "k-fence";
        let hash = vec![1u8; 32];
        let operation_id = Uuid::new_v4();
        let old_claim = insert_abandoned(&pool, actor, key, &hash, Some(operation_id)).await;

        let mut conn = pool.acquire().await.unwrap();
        let outcome = check(&mut conn, actor, key, &hash, 60.0).await.unwrap();
        let Outcome::Run {
            claim_id: new_claim,
            prior_operation_id,
        } = outcome
        else {
            panic!("expected reclaim");
        };
        assert_ne!(new_claim, old_claim);
        assert_eq!(prior_operation_id, Some(operation_id));

        // The old handler (still running) stores its own response: it must fail.
        let stale = idem(actor, key, old_claim);
        let stored = stale
            .store(
                &mut conn,
                StatusCode::OK,
                &serde_json::json!({"stale": true}),
            )
            .await
            .unwrap();
        assert!(!stored, "stale handler stored over the reclaimed claim");

        // The new owner stores fine and the claim is marked done.
        let owner = Idem {
            actor_id: actor,
            key: Some(key.to_string()),
            claim_id: new_claim,
            prior_operation_id,
        };
        let stored = owner
            .store(
                &mut conn,
                StatusCode::CREATED,
                &serde_json::json!({"ok": true}),
            )
            .await
            .unwrap();
        assert!(stored);
        let status: String =
            sqlx::query_scalar("SELECT status FROM operations WHERE actor_id=$1 AND key=$2")
                .bind(actor)
                .bind(key)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(status, "done");
        drop(conn);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn release_is_fenced_and_preserves_resumable_claims() {
        let (pool, actor) = test_pool().await;
        let mut conn = pool.acquire().await.unwrap();

        // A claim whose operation id was committed is kept `running` for resume.
        let resumable_key = "k-resume";
        let resumable_claim = insert_abandoned(
            &pool,
            actor,
            resumable_key,
            &[1u8; 32],
            Some(Uuid::new_v4()),
        )
        .await;
        release(&mut conn, actor, resumable_key, resumable_claim)
            .await
            .unwrap();
        let kept: i64 =
            sqlx::query_scalar("SELECT count(*) FROM operations WHERE actor_id=$1 AND key=$2")
                .bind(actor)
                .bind(resumable_key)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(kept, 1, "a resumable claim must survive a failed handler");

        // A claim with no operation id is released, but only by its owner.
        let blank_key = "k-blank";
        let blank_claim = insert_abandoned(&pool, actor, blank_key, &[1u8; 32], None).await;
        let stale = Uuid::new_v4();
        release(&mut conn, actor, blank_key, stale).await.unwrap();
        let still: i64 =
            sqlx::query_scalar("SELECT count(*) FROM operations WHERE actor_id=$1 AND key=$2")
                .bind(actor)
                .bind(blank_key)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            still, 1,
            "a stale handler must not delete a re-claimed claim"
        );
        release(&mut conn, actor, blank_key, blank_claim)
            .await
            .unwrap();
        let gone: i64 =
            sqlx::query_scalar("SELECT count(*) FROM operations WHERE actor_id=$1 AND key=$2")
                .bind(actor)
                .bind(blank_key)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(gone, 0, "the owner releases a blank claim");

        drop(conn);
        pool.close().await;
    }
}
