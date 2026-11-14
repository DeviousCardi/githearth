//! Pull-request head pins (ADR 0013 "What is live", 7).
//!
//! A pull request's head commit is written as a pin in the repository that
//! owns the head, in the same transaction that opens the pull request. The
//! pin is never a Git ref and is never advertised: it exists only so the
//! housekeeping live-set walk can start from it, which keeps a closed pull
//! request's diff readable after its source branch is deleted and a prune has
//! run.
//!
//! A pin outlives its pull request by `housekeeping_pin_retention_days`
//! (30 by default), so a closed or merged pull request keeps its diff for that
//! long; an open pull request is re-pinned by every housekeeping run
//! ([`crate::housekeeping`]), which follows a force push. Closing or merging a
//! pull request extends the pin from that moment, so a request that stayed open
//! longer than the window does not lose its diff the day it closes. Closed and
//! merged pins expire by timestamp instead of staying forever.

use sqlx::PgConnection;
use uuid::Uuid;

/// Why a pin exists.
pub const REASON_PULL_REQUEST: &str = "pull_request_head";
/// A pin created by the 0045 backfill for a pull request that predates pins.
pub const REASON_BACKFILL: &str = "backfill";

/// Creates or extends the pin for `commit_oid`. Idempotent: the retention
/// timestamp only moves forward, so re-pinning a head never shortens it.
pub async fn pin_head(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pull_request_id: Uuid,
    commit_oid: &str,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    pin(
        conn,
        repository_id,
        pull_request_id,
        commit_oid,
        REASON_PULL_REQUEST,
        retention_days,
    )
    .await
}

/// Extends every pin of a pull request that has just closed or merged, so the
/// retention window starts at the close rather than at the last refresh.
/// Monotonic, like [`pin_head`]: it never shortens an existing timestamp.
pub async fn extend_for_closed(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE pinned_commits SET \
           retain_until = GREATEST(retain_until, now() + make_interval(days => $2)) \
         WHERE pull_request_id = $1",
    )
    .bind(pull_request_id)
    .bind(i32::try_from(retention_days).unwrap_or(i32::MAX))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn pin(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pull_request_id: Uuid,
    commit_oid: &str,
    reason: &str,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pinned_commits \
         (repository_id, pull_request_id, commit_oid, reason, retain_until) \
         VALUES ($1, $2, $3, $4, now() + make_interval(days => $5)) \
         ON CONFLICT (pull_request_id, commit_oid) DO UPDATE SET \
           retain_until = GREATEST(pinned_commits.retain_until, EXCLUDED.retain_until), \
           reason = EXCLUDED.reason",
    )
    .bind(repository_id)
    .bind(pull_request_id)
    .bind(commit_oid)
    .bind(reason)
    .bind(i32::try_from(retention_days).unwrap_or(i32::MAX))
    .execute(&mut *conn)
    .await?;
    if reason == REASON_PULL_REQUEST {
        // A force push follows the head: an old head of an open request is no
        // longer the one the pull renders. A closed request keeps every pin it
        // had, because no refresh runs for it and its last head must survive.
        sqlx::query(
            "DELETE FROM pinned_commits p USING pull_requests pr \
             WHERE p.pull_request_id = pr.id AND p.pull_request_id = $1 \
               AND p.commit_oid <> $2 AND pr.state = 'open'",
        )
        .bind(pull_request_id)
        .bind(commit_oid)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Commits a live-set walk must start from: every pin of an open pull request,
/// and every pin whose retention has not elapsed.
pub async fn keep_commits(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT p.commit_oid FROM pinned_commits p \
         JOIN pull_requests pr ON pr.id = p.pull_request_id \
         WHERE p.repository_id = $1 AND (pr.state = 'open' OR p.retain_until > now()) \
         ORDER BY p.commit_oid",
    )
    .bind(repository_id)
    .fetch_all(conn)
    .await
}
