//! Mergeability (M3 conflicts): whether a pull request head merges cleanly into
//! its target tip for each enabled merge method, cached by (head oid, target
//! oid) so a moved head or target recomputes rather than serving a stale answer.
//!
//! The check reuses [`githearth_git::merge::build_candidate`] — the same
//! candidate construction the merge worker runs — so a "clean" result here
//! always matches what an executed merge would produce. Objects written by the
//! candidate build are unreferenced and harmless; the cache means each unique
//! (head, target) pair is computed at most once.

use crate::{ApiError, AppState, browse};
use githearth_git::merge::{self, Candidate, Identity, Method, Request};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The merge methods mergeability is reported for.
pub(crate) const ENABLED: [(&str, Method); 3] = [
    ("merge", Method::Merge),
    ("squash", Method::Squash),
    ("rebase", Method::Rebase),
];

/// The mergeability of one method. `status` is `clean`, `conflicts`,
/// `unrelated` or `up_to_date`; `conflicts` lists the conflicted paths only
/// when `status` is `conflicts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) method: String,
    pub(crate) status: String,
    pub(crate) conflicts: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct CachedRow {
    status: String,
    conflicts: Vec<String>,
}

async fn cached(
    pool: &PgPool,
    repo_id: Uuid,
    head: &str,
    target: &str,
    method: &str,
) -> Result<Option<CachedRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT status, conflicts FROM merge_checks \
         WHERE repository_id = $1 AND head_oid = $2 AND target_oid = $3 AND method = $4",
    )
    .bind(repo_id)
    .bind(head)
    .bind(target)
    .bind(method)
    .fetch_optional(pool)
    .await
}

async fn store(
    pool: &PgPool,
    repo_id: Uuid,
    head: &str,
    target: &str,
    method: &str,
    status: &str,
    conflicts: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO merge_checks (repository_id, head_oid, target_oid, method, status, conflicts) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (repository_id, head_oid, target_oid, method) DO UPDATE \
         SET status = EXCLUDED.status, conflicts = EXCLUDED.conflicts, updated_at = now()",
    )
    .bind(repo_id)
    .bind(head)
    .bind(target)
    .bind(method)
    .bind(status)
    .bind(conflicts)
    .execute(pool)
    .await?;
    Ok(())
}

/// Builds a candidate for `head` into `target` and folds it into a
/// mergeability `(status, conflicts)`. Shared identities are enough: the
/// candidate commit is never referenced, only its outcome matters.
async fn compute(
    state: &AppState,
    repo_id: Uuid,
    method: Method,
    target: String,
    head: String,
) -> Result<(String, Vec<String>), ApiError> {
    browse::git_read(state, repo_id, move |path, timeout| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let identity = Identity {
            name: "GitHearth".into(),
            email: "noreply@githearth.invalid".into(),
            time: now,
            offset: "+0000".into(),
        };
        let message = String::new();
        let request = Request {
            method,
            target: &target,
            source: &head,
            message: &message,
            author: &identity,
            committer: &identity,
        };
        match merge::build_candidate(path, &request, timeout) {
            Ok(Candidate::Ready { .. }) => Ok(("clean".to_owned(), vec![])),
            Ok(Candidate::Conflicts(paths)) => Ok(("conflicts".to_owned(), paths)),
            Ok(Candidate::UpToDate) => Ok(("up_to_date".to_owned(), vec![])),
            Ok(Candidate::Unrelated) => Ok(("unrelated".to_owned(), vec![])),
            // Not a conflict: `ff_only` never reaches here, and a rebase that
            // refuses a merge commit still merges cleanly by merge/squash.
            Ok(Candidate::NotFastForward) | Ok(Candidate::RebaseMergeCommit) => {
                Ok(("clean".to_owned(), vec![]))
            }
            Err(error) => Err(error),
        }
    })
    .await
}

/// Compute (or fetch from the cache) the mergeability of one method.
pub(crate) async fn check_method(
    state: &AppState,
    repo_id: Uuid,
    method: Method,
    method_name: &str,
    target: &str,
    head: &str,
) -> Result<Check, ApiError> {
    if let Some(row) = cached(&state.pool, repo_id, head, target, method_name).await? {
        return Ok(Check {
            method: method_name.to_owned(),
            status: row.status,
            conflicts: row.conflicts,
        });
    }
    let (status, conflicts) =
        compute(state, repo_id, method, target.to_owned(), head.to_owned()).await?;
    store(
        &state.pool,
        repo_id,
        head,
        target,
        method_name,
        &status,
        &conflicts,
    )
    .await?;
    Ok(Check {
        method: method_name.to_owned(),
        status,
        conflicts,
    })
}

/// The mergeability of every enabled method for a pull request with the given
/// head and target tips.
pub(crate) async fn for_pull(
    state: &AppState,
    repo_id: Uuid,
    head: &str,
    target: &str,
) -> Result<Vec<Check>, ApiError> {
    let mut checks = Vec::with_capacity(ENABLED.len());
    for (name, method) in ENABLED {
        checks.push(check_method(state, repo_id, method, name, target, head).await?);
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_serializes_round_trip() {
        let check = Check {
            method: "merge".into(),
            status: "conflicts".into(),
            conflicts: vec!["a.txt".into(), "b/c.txt".into()],
        };
        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(value["method"], "merge");
        assert_eq!(value["status"], "conflicts");
        assert_eq!(value["conflicts"][0], "a.txt");
        let back: Check = serde_json::from_value(value).unwrap();
        assert_eq!(back, check);
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn cache_round_trips_and_is_keyed_by_head_target_and_method() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

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

        // Each assertion row is scoped to a fresh repository id so it cannot
        // collide with other tests sharing this database. The merge_checks
        // foreign key requires a live repository.
        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("mc-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
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

        let head = "a".repeat(40);
        let target = "b".repeat(40);

        // A miss returns nothing.
        assert!(
            cached(&pool, repo, &head, &target, "merge")
                .await
                .unwrap()
                .is_none()
        );

        // Storing then reading returns the stored row.
        store(
            &pool,
            repo,
            &head,
            &target,
            "merge",
            "conflicts",
            &["a.txt".into()],
        )
        .await
        .unwrap();
        let row = cached(&pool, repo, &head, &target, "merge")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "conflicts");
        assert_eq!(row.conflicts, vec!["a.txt".to_owned()]);

        // A different method (same head/target) is a separate cache entry.
        assert!(
            cached(&pool, repo, &head, &target, "squash")
                .await
                .unwrap()
                .is_none()
        );
        store(&pool, repo, &head, &target, "squash", "clean", &[])
            .await
            .unwrap();
        assert_eq!(
            cached(&pool, repo, &head, &target, "squash")
                .await
                .unwrap()
                .unwrap()
                .status,
            "clean"
        );

        // A new head under the same target/method recomputes: the merge row is
        // untouched and no stale "clean" is served for the new head.
        let new_head = "c".repeat(40);
        assert!(
            cached(&pool, repo, &new_head, &target, "merge")
                .await
                .unwrap()
                .is_none()
        );
        store(&pool, repo, &new_head, &target, "merge", "clean", &[])
            .await
            .unwrap();
        assert_eq!(
            cached(&pool, repo, &head, &target, "merge")
                .await
                .unwrap()
                .unwrap()
                .status,
            "conflicts"
        );
        assert_eq!(
            cached(&pool, repo, &new_head, &target, "merge")
                .await
                .unwrap()
                .unwrap()
                .status,
            "clean"
        );

        // Upserting the same key replaces rather than duplicating.
        store(&pool, repo, &head, &target, "merge", "unrelated", &[])
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM merge_checks WHERE repository_id = $1 AND head_oid = $2 \
             AND target_oid = $3 AND method = 'merge'",
        )
        .bind(repo)
        .bind(&head)
        .bind(&target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(
            cached(&pool, repo, &head, &target, "merge")
                .await
                .unwrap()
                .unwrap()
                .status,
            "unrelated"
        );

        // Clean up this test's rows.
        sqlx::query("DELETE FROM merge_checks WHERE repository_id = $1")
            .bind(repo)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM repositories WHERE id = $1")
            .bind(repo)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }
}
