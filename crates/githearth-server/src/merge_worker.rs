//! Merge worker (ADR 0004 "Execution", Person A side).
//!
//! Claims queued merge intents and drives them through `merge::execute_intent`.
//! A lease (holder, attempt, expiry) fences the claim so only one process runs
//! an intent at a time; the worker's compare-and-swap on the intent row and the
//! Git-side expected-old ref update together make a merge happen at most once.

use crate::AppState;
use crate::merge::{MergeError, MergeOutcome, execute_intent};
use sqlx::{PgConnection, Row};
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

/// A claim that reaches attempt 6 or more is failed without calling
/// `execute_intent`.
const MAX_ATTEMPTS: i32 = 5;

/// One lease holder per process, so a restart never fights its own lease.
fn holder() -> Uuid {
    static HOLDER: OnceLock<Uuid> = OnceLock::new();
    *HOLDER.get_or_init(Uuid::new_v4)
}

/// A claimed intent: the row identifiers needed to fence later steps.
#[derive(Debug)]
struct Claim {
    operation_id: Uuid,
    state: String,
    attempt: i32,
    actor_id: Uuid,
    organization_id: Uuid,
}

/// Claims one intent, atomically: `FOR UPDATE SKIP LOCKED`, oldest
/// `(created_at, operation_id)` first — the same ordering `queue_position`
/// shows, so a same-instant tie-breaks deterministically. Claimable means
/// `state = 'queued'`, or a `validating`/`executing`
/// intent whose `lease_expires_at` has passed. The claim sets a fresh holder,
/// increments `lease_attempt`, and sets `lease_expires_at` to now() plus
/// 6 x `git_timeout_seconds`. A `queued` intent becomes `validating`; a
/// reclaimed intent keeps its state.
async fn claim_intent(
    conn: &mut PgConnection,
    holder: Uuid,
    lease: Duration,
    repository: Option<Uuid>,
) -> Result<Option<Claim>, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE merge_intents SET lease_holder = $1, \
         lease_attempt = COALESCE(lease_attempt, 0) + 1, \
         lease_expires_at = now() + $2 * interval '1 second', \
         state = CASE WHEN state = 'queued' THEN 'validating' ELSE state END, \
         updated_at = now() \
         WHERE operation_id = ( \
           SELECT operation_id FROM merge_intents \
           WHERE ($3::uuid IS NULL OR repository_id = $3) \
              AND (state = 'queued' \
               OR (state IN ('validating','executing') AND lease_expires_at < now())) \
            ORDER BY created_at, operation_id \
            FOR UPDATE SKIP LOCKED \
           LIMIT 1) \
         RETURNING operation_id, repository_id, state, lease_attempt, actor_id",
    )
    .bind(holder)
    .bind(lease.as_secs_f64())
    .bind(repository)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let operation_id: Uuid = row.get("operation_id");
    let repository_id: Uuid = row.get("repository_id");
    let organization_id: Uuid =
        sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id = $1")
            .bind(repository_id)
            .fetch_one(&mut *conn)
            .await?;
    Ok(Some(Claim {
        operation_id,
        state: row.get("state"),
        attempt: row.get("lease_attempt"),
        actor_id: row.get("actor_id"),
        organization_id,
    }))
}

/// Moves a `validating` intent to `executing`, fenced with the operation id,
/// state, lease holder and attempt. Returns true when this holder moved it; a
/// false result (e.g. a cancel won the race) means skip. A reclaimed
/// `executing` intent needs no transition.
async fn move_to_executing(conn: &mut PgConnection, claim: &Claim) -> Result<bool, sqlx::Error> {
    if claim.state == "executing" {
        return Ok(true);
    }
    let moved = sqlx::query(
        "UPDATE merge_intents SET state = 'executing', updated_at = now() \
         WHERE operation_id = $1 AND state = 'validating' \
         AND lease_holder = $2 AND lease_attempt = $3",
    )
    .bind(claim.operation_id)
    .bind(holder())
    .bind(claim.attempt)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    Ok(moved)
}

/// Applies an outcome to the claimed intent, fenced with `state = 'executing'`,
/// `lease_holder` and `lease_attempt`. Emits the matching audit event in the
/// same transaction and sets `updated_at = now()`.
async fn apply_outcome(
    state: &AppState,
    claim: &Claim,
    outcome: MergeOutcome,
) -> Result<(), sqlx::Error> {
    // A merge-queue entry whose target moved is re-targeted to the new tip and
    // returned to `queued`, so the next claim re-validates and merges it. The
    // fence is the same as any other outcome apply; only the claim's holder may
    // move it. `lease_attempt` is preserved so the attempt cap still bounds a
    // pathological re-target loop.
    if let MergeOutcome::Retarget { new_target } = outcome {
        let mut tx = state.pool.begin().await?;
        sqlx::query(
            "UPDATE merge_intents SET expected_target_oid = $1, state = 'queued', \
             lease_holder = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE operation_id = $2 AND state = 'executing' \
             AND lease_holder = $3 AND lease_attempt = $4",
        )
        .bind(new_target)
        .bind(claim.operation_id)
        .bind(holder())
        .bind(claim.attempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let (new_state, failure_reason, kind) = match outcome {
        MergeOutcome::Merged { .. } | MergeOutcome::AlreadyMerged { .. } => {
            // No transition: `record_merge_result` owns `merged`. If replay
            // failed the row stays `executing` and a later reclaim converges.
            return Ok(());
        }
        MergeOutcome::Retarget { .. } => unreachable!("handled above"),
        MergeOutcome::Conflicted { paths } => (
            "conflicted",
            Some(paths.join(", ")),
            "pull_request.merge_intent_conflicted",
        ),
        MergeOutcome::Stale { reason } => {
            ("stale", Some(reason), "pull_request.merge_intent_stale")
        }
        MergeOutcome::Failed { reason } => {
            ("failed", Some(reason), "pull_request.merge_intent_failed")
        }
    };

    let mut tx = state.pool.begin().await?;
    let rows = sqlx::query(
        "UPDATE merge_intents SET state = $1, failure_reason = $2, updated_at = now() \
         WHERE operation_id = $3 AND state = 'executing' \
         AND lease_holder = $4 AND lease_attempt = $5",
    )
    .bind(new_state)
    .bind(failure_reason)
    .bind(claim.operation_id)
    .bind(holder())
    .bind(claim.attempt)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if rows > 0 {
        crate::event(&mut tx, Some(claim.organization_id), claim.actor_id, kind).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Runs one claimed intent through the ADR 0004 worker steps. Returns Ok on
/// any business outcome (including a lost fence); only infrastructure failures
/// return Err.
async fn run_intent(state: &AppState, claim: &Claim) -> Result<(), MergeError> {
    // 1. Move `validating` -> `executing`, fenced. A reclaimed `executing`
    // intent goes straight to step 2. If the fence lost (e.g. a cancel won),
    // skip.
    let mut conn = state.pool.acquire().await?;
    if !(move_to_executing(&mut conn, claim).await?) {
        drop(conn);
        return Ok(());
    }
    drop(conn);

    // 4. Attempt cap: a claim at attempt MAX_ATTEMPTS + 1 is failed without
    // executing. The fence is the same as an outcome apply (`state='executing'`).
    if claim.attempt > MAX_ATTEMPTS {
        let outcome = MergeOutcome::Failed {
            reason: "too many attempts".into(),
        };
        return apply_outcome(state, claim, outcome)
            .await
            .map_err(MergeError::Database);
    }

    // 2. Execute. `execute_intent` revalidates everything.
    let outcome = match execute_intent(state, claim.operation_id).await {
        Ok(outcome) => outcome,
        // 3. Err(MergeError): log and leave the row; the lease expires and it
        // is retried later. A deterministic storage/join failure does not
        // benefit from waiting out the lease, so release it to retry on the
        // next pass; attempts stay bounded by MAX_ATTEMPTS. A database error
        // keeps the lease so an outage does not burn attempts.
        Err(error) => {
            if !matches!(error, MergeError::Database(_)) {
                let _ = sqlx::query(
                    "UPDATE merge_intents SET lease_expires_at = now() - interval '1 second', \
                     updated_at = now() \
                     WHERE operation_id = $1 AND lease_holder = $2 AND lease_attempt = $3",
                )
                .bind(claim.operation_id)
                .bind(holder())
                .bind(claim.attempt)
                .execute(&state.pool)
                .await;
            }
            tracing::warn!(
                operation_id = %claim.operation_id,
                error = %error,
                "merge intent execution failed; lease will expire and retry"
            );
            return Ok(());
        }
    };

    apply_outcome(state, claim, outcome)
        .await
        .map_err(MergeError::Database)
}

/// Claims and runs intents until none is claimable, returning how many ran.
/// `repository` scopes the pass to one repository (tests pass their own);
/// `spawn` passes `None` to claim from every repository.
pub async fn run_once(state: &AppState, repository: Option<Uuid>) -> Result<usize, MergeError> {
    let lease = Duration::from_secs(state.config.git_timeout_seconds * 6);
    let holder = holder();
    let mut ran = 0usize;

    loop {
        let mut conn = state.pool.acquire().await?;
        let claim = claim_intent(&mut conn, holder, lease, repository).await?;
        drop(conn);

        let Some(claim) = claim else {
            break;
        };

        match run_intent(state, &claim).await {
            Ok(()) => ran += 1,
            Err(error) => {
                tracing::warn!(
                    operation_id = %claim.operation_id,
                    error = %error,
                    "merge worker error; will retry after lease expiry"
                );
            }
        }
    }

    Ok(ran)
}

/// Spawn the merge worker background task. Unlike the reconciler it does not
/// run a pass at startup; the first pass runs after one interval. Never panics
/// or exits the loop on errors.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(state.config.merge_worker_interval_seconds);
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;

        loop {
            tick.tick().await;
            match run_once(&state, None).await {
                Ok(ran) => {
                    if ran > 0 {
                        tracing::info!(ran, "merge worker ran intents");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "merge worker pass failed; will retry");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{ConnectOptions, postgres::PgPoolOptions};
    use std::str::FromStr;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn claim_intent_tie_breaks_same_timestamp_entries_by_operation_id() {
        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("cw-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(format!("o-{}", Uuid::new_v4().simple()))
        .execute(&pool)
        .await
        .unwrap();
        let repo = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
             VALUES ($1,$2,'r',$3,$4,'active','private')",
        )
        .bind(repo)
        .bind(org)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        async fn pull(pool: &sqlx::PgPool, repo: Uuid, account: Uuid, number: i32) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch,source_repository_id) \
                 VALUES ($1,$2,$3,'t','',$4,$5,'main',$2)",
            )
            .bind(id)
            .bind(repo)
            .bind(number)
            .bind(account)
            .bind(format!("src-{number}"))
            .execute(pool)
            .await
            .unwrap();
            id
        }

        // Two claimable intents created in the same instant, on two different
        // pull requests (the per-pull non-terminal index allows only one live
        // intent each). The larger `operation_id` is inserted first, so a
        // created_at-only claim (which falls back to insertion order for equal
        // keys) would pick it first; the tie-break must claim the smaller id
        // first instead. Postgres orders `uuid` by its 16-byte value, which is
        // exactly `as_bytes()` lexicographic order.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (smaller, larger) = if a.as_bytes() < b.as_bytes() {
            (a, b)
        } else {
            (b, a)
        };
        let pulls = [
            pull(&pool, repo, account, 1).await,
            pull(&pool, repo, account, 2).await,
        ];
        for (operation_id, pull) in [(larger, pulls[0]), (smaller, pulls[1])] {
            sqlx::query(
                "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
                 source_oid, target_ref, expected_target_oid, method, policy_generation, \
                 checks_approvals_digest, actor_id, state, in_queue, created_at) \
                 VALUES ($1,$2,$3,'src',$4,'main',$4,'merge',0,'d',$5,'queued',true, \
                         '2024-01-01T00:00:00Z'::timestamptz)",
            )
            .bind(operation_id)
            .bind(repo)
            .bind(pull)
            .bind("a".repeat(40))
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut conn = pool.acquire().await.unwrap();
        let lease = Duration::from_secs(60);
        let first = claim_intent(&mut conn, holder(), lease, Some(repo))
            .await
            .unwrap()
            .expect("first claim");
        assert_eq!(first.operation_id, smaller);
        let second = claim_intent(&mut conn, holder(), lease, Some(repo))
            .await
            .unwrap()
            .expect("second claim");
        assert_eq!(second.operation_id, larger);

        drop(conn);
        pool.close().await;
    }
}
