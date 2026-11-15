//! Pinned commits: the object ID half of the housekeeping keep-set (ADR 0013
//! "What is live", 7; U11).
//!
//! The keep-set is every ref plus every object ID PostgreSQL references. A
//! pin is a row that names one commit the live-set walk must start from even
//! when no ref reaches it. It is never a Git ref and is never advertised:
//! `upload-pack` sees no new namespace.
//!
//! One pin per subject row, written in the same transaction as the row that
//! references it, so a row can never exist without its pin:
//!
//! | Subject | Pinned while | After it ends |
//! | --- | --- | --- |
//! | pull-request head | the request exists | `housekeeping_pin_retention_days` after close/merge |
//! | pull-request base | the request exists | the same window after close/merge |
//! | review comment's `commit_id` | the comment exists | nothing: the pin cascades with the comment |
//! | status's `commit_id` | it is the latest result of its context | the same window after it is superseded |
//!
//! `retain_until IS NULL` means "while the subject row exists". Only the head
//! of an *open* request is re-pinned by a housekeeping run
//! ([`crate::housekeeping`]), because only it has to follow a force push;
//! every other live pin is live by its subject and needs no refresh.
//!
//! A fork pull request's head is pinned in the source repository (the fork),
//! which holds the authoritative objects. The **base** pin is written in the
//! target repository, because the base is a commit of the target branch.

use sqlx::PgConnection;
use uuid::Uuid;

/// Why a pin exists.
pub const REASON_PULL_REQUEST: &str = "pull_request_head";
/// A pin created by the 0045 backfill for a pull request that predates pins.
pub const REASON_BACKFILL: &str = "backfill";
/// The pull request's base commit.
pub const REASON_BASE: &str = "pull_request_base";
/// A review comment's commit.
pub const REASON_REVIEW_COMMENT: &str = "review_comment";
/// A reviewed commit.
pub const REASON_REVIEW: &str = "review_commit";
/// A commit status's commit.
pub const REASON_STATUS: &str = "commit_status";

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

/// Pins the **target-side copy** of a fork pull request's head, when the
/// target holds it. The pin lives in the target repository under the same
/// `pull_request_head` reason, so it is refreshed with the open request and
/// extended on close like the source pin; the two rows differ by
/// `repository_id`. A target that does not hold the object yet keeps no row
/// (nothing to keep), and the next refresh pins the copy once a read or merge
/// has fetched it.
pub async fn pin_target_copy(
    conn: &mut PgConnection,
    target_repository_id: Uuid,
    pull_request_id: Uuid,
    commit_oid: &str,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pinned_commits \
         (repository_id, pull_request_id, commit_oid, reason, retain_until) \
         VALUES ($1, $2, $3, $4, now() + make_interval(days => $5)) \
         ON CONFLICT (repository_id, pull_request_id, commit_oid) DO NOTHING",
    )
    .bind(target_repository_id)
    .bind(pull_request_id)
    .bind(commit_oid)
    .bind(REASON_PULL_REQUEST)
    .bind(days(retention_days))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Pins a pull request's base commit while the request exists, for
/// `retention_days` after it closes. In the target repository: the base is a
/// commit of the target branch.
pub async fn pin_base(
    conn: &mut PgConnection,
    repository_id: Uuid,
    pull_request_id: Uuid,
    commit_oid: &str,
    state: &str,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    // Update first, insert if absent: the subject is unique on a *partial*
    // index, which `ON CONFLICT` cannot infer without restating the predicate.
    // Also the conflict target a single migration could change under us.
    let updated = sqlx::query(
        "UPDATE pinned_commits SET commit_oid = $3, \
           retain_until = CASE WHEN $5 = 'open' THEN NULL \
             ELSE GREATEST(COALESCE(retain_until, now() + make_interval(days => $6)), \
                           now() + make_interval(days => $6)) END \
         WHERE base_of_pull_request_id = $2 AND repository_id = $1",
    )
    .bind(repository_id)
    .bind(pull_request_id)
    .bind(commit_oid)
    .bind(REASON_BASE)
    .bind(state)
    .bind(days(retention_days))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pinned_commits \
             (repository_id, base_of_pull_request_id, commit_oid, reason, retain_until) \
             VALUES ($1, $2, $3, $4, \
                     CASE WHEN $5 = 'open' THEN NULL \
                          ELSE now() + make_interval(days => $6) END)",
        )
        .bind(repository_id)
        .bind(pull_request_id)
        .bind(commit_oid)
        .bind(REASON_BASE)
        .bind(state)
        .bind(days(retention_days))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Pins a review comment's commit while the comment exists. The comment is
/// written in the same transaction, so the pin cascades with it.
///
/// The pin lives in the repository that stores the head objects (the source
/// repository): an anchored comment names the pull request's current head,
/// which the comment route has already checked.
pub async fn pin_review_comment(
    conn: &mut PgConnection,
    repository_id: Uuid,
    comment_id: Uuid,
    commit_oid: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE pinned_commits SET commit_oid = $2 \
         WHERE review_comment_id = $1",
    )
    .bind(comment_id)
    .bind(commit_oid)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pinned_commits \
             (repository_id, review_comment_id, commit_oid, reason, retain_until) \
             VALUES ($1, $2, $3, $4, NULL)",
        )
        .bind(repository_id)
        .bind(comment_id)
        .bind(commit_oid)
        .bind(REASON_REVIEW_COMMENT)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Pins a status's commit while it is the latest result for its context, and
/// supersedes the pin of the previous latest status of the same
/// (repository, commit, context): that one now expires `retention_days` from
/// now instead of living with the row. This is the bound that stops a status
/// on a since-unreachable commit from pinning its ancestry forever.
///
/// Statuses are append-only, so "superseded" is derived here rather than
/// stored: the superseded row's pin keeps its own `commit_status_id` and only
/// gains a timestamp.
pub async fn pin_status(
    conn: &mut PgConnection,
    repository_id: Uuid,
    commit_oid: &str,
    context: &str,
    status_id: Uuid,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    // The previous latest statuses of this context on this commit: every
    // status of the context except the one just inserted.
    sqlx::query(
        "UPDATE pinned_commits p SET retain_until = now() + make_interval(days => $4) \
         FROM commit_statuses s \
         WHERE s.id = p.commit_status_id AND p.commit_status_id IS DISTINCT FROM $3 \
           AND p.retain_until IS NULL \
           AND s.repository_id = $1 AND s.commit_id = $2 AND s.context = $5",
    )
    .bind(repository_id)
    .bind(commit_oid)
    .bind(status_id)
    .bind(days(retention_days))
    .bind(context)
    .execute(&mut *conn)
    .await?;
    let updated =
        sqlx::query("UPDATE pinned_commits SET commit_oid = $2 WHERE commit_status_id = $1")
            .bind(status_id)
            .bind(commit_oid)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pinned_commits \
             (repository_id, commit_status_id, commit_oid, reason, retain_until) \
             VALUES ($1, $2, $3, $4, NULL)",
        )
        .bind(repository_id)
        .bind(status_id)
        .bind(commit_oid)
        .bind(REASON_STATUS)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Pins a reviewed commit while the review exists. A review has no subject
/// row of its own to cascade from, so the pin lives exactly as long as the
/// pull request the review belongs to: the review rows go with it
/// (`ON DELETE CASCADE`) and no code deletes a single review. `pull_request_id`
/// NULL keeps it out of 0045's head uniqueness (`(pull_request_id,
/// commit_oid)`).
pub async fn pin_review_commit(
    conn: &mut PgConnection,
    repository_id: Uuid,
    commit_oid: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE pinned_commits SET reason = reason \
         WHERE repository_id = $1 AND review_commit_oid = $2",
    )
    .bind(repository_id)
    .bind(commit_oid)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pinned_commits \
             (repository_id, review_commit_oid, commit_oid, reason, retain_until) \
             VALUES ($1, $2, $2, $3, NULL)",
        )
        .bind(repository_id)
        .bind(commit_oid)
        .bind(REASON_REVIEW)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Extends every live pin of a pull request that has just closed or merged,
/// so the retention window starts at the close rather than at the last
/// refresh. Covers the head pin (the source's and a fork copy in the target)
/// and the base pin; a comment pin lives with its comment and a status pin
/// with its status. Monotonic, like [`pin_head`]: it never shortens an
/// existing timestamp.
pub async fn extend_for_closed(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
    retention_days: u64,
) -> Result<(), sqlx::Error> {
    let until = "now() + make_interval(days => $2)";
    sqlx::query(&format!(
        "UPDATE pinned_commits SET \
           retain_until = GREATEST(COALESCE(retain_until, {until}), {until}) \
         WHERE pull_request_id = $1"
    ))
    .bind(pull_request_id)
    .bind(days(retention_days))
    .execute(&mut *conn)
    .await?;
    // A base pin carries no `pull_request_id`: its subject is its own column.
    sqlx::query(&format!(
        "UPDATE pinned_commits SET \
           retain_until = GREATEST(COALESCE(retain_until, {until}), {until}) \
         WHERE base_of_pull_request_id = $1"
    ))
    .bind(pull_request_id)
    .bind(days(retention_days))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn days(retention_days: u64) -> i32 {
    i32::try_from(retention_days).unwrap_or(i32::MAX)
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
         ON CONFLICT ON CONSTRAINT pinned_commits_one_head_per_repository DO UPDATE SET \
           retain_until = GREATEST(pinned_commits.retain_until, EXCLUDED.retain_until), \
           reason = EXCLUDED.reason",
    )
    .bind(repository_id)
    .bind(pull_request_id)
    .bind(commit_oid)
    .bind(reason)
    .bind(days(retention_days))
    .execute(&mut *conn)
    .await?;
    if reason == REASON_PULL_REQUEST {
        // A force push follows the head: an old head of an open request is no
        // longer the one the pull renders. A closed request keeps every pin it
        // had, because no refresh runs for it and its last head must survive.
        sqlx::query(
            "DELETE FROM pinned_commits p USING pull_requests pr \
             WHERE p.pull_request_id = pr.id AND p.pull_request_id = $1 \
               AND p.commit_oid <> $2 AND pr.state = 'open' \
               AND p.base_of_pull_request_id IS NULL",
        )
        .bind(pull_request_id)
        .bind(commit_oid)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Commits a live-set walk must start from, of every kind: a pin that lives
/// while its subject exists (`retain_until IS NULL`) or whose retention has
/// not elapsed.
pub async fn keep_commits(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT DISTINCT commit_oid FROM pinned_commits \
         WHERE repository_id = $1 AND (retain_until IS NULL OR retain_until > now()) \
         ORDER BY commit_oid",
    )
    .bind(repository_id)
    .fetch_all(conn)
    .await
}
