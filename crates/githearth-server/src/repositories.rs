use githearth_authz::{Repository, Visibility};
use githearth_core::RepositoryStatus;
use sqlx::{Connection, PgConnection, Row};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RepositoryRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub description: String,
    pub visibility: String,
    pub archived: bool,
    pub status: RepositoryStatus,
    pub operation_id: Uuid,
    pub created_by: Uuid,
    pub object_format: String,
    pub parent_repository_id: Option<Uuid>,
}

fn row_to_repository(row: &sqlx::postgres::PgRow) -> RepositoryRow {
    let raw: String = row.get("status");
    RepositoryRow {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        name: row.get("name"),
        description: row.get("description"),
        visibility: row.get("visibility"),
        archived: row.get("archived"),
        status: raw.parse().expect("database status is always valid"),
        operation_id: row.get("operation_id"),
        created_by: row.get("created_by"),
        object_format: row.get("object_format"),
        parent_repository_id: row.get("parent_repository_id"),
    }
}

const REPO_COLS: &str = "id, organization_id, name, description, visibility, archived, status, operation_id, created_by, object_format, parent_repository_id";

pub async fn insert_creating(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
    created_by: Uuid,
    operation_id: Uuid,
    visibility: &str,
    description: &str,
) -> Result<RepositoryRow, sqlx::Error> {
    insert_creating_impl(
        conn,
        organization_id,
        name,
        created_by,
        operation_id,
        visibility,
        description,
        "sha1",
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_creating_with_format(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
    created_by: Uuid,
    operation_id: Uuid,
    visibility: &str,
    description: &str,
    object_format: &str,
) -> Result<RepositoryRow, sqlx::Error> {
    insert_creating_impl(
        conn,
        organization_id,
        name,
        created_by,
        operation_id,
        visibility,
        description,
        object_format,
        None,
    )
    .await
}

/// Like [`insert_creating`], but records `parent_repository_id` (and the
/// parent's `object_format`) so the new repository is a fork of an existing one
/// (M3 fork contributions).
#[allow(clippy::too_many_arguments)]
pub async fn insert_creating_with_parent(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
    created_by: Uuid,
    operation_id: Uuid,
    visibility: &str,
    description: &str,
    object_format: &str,
    parent_repository_id: Option<Uuid>,
) -> Result<RepositoryRow, sqlx::Error> {
    insert_creating_impl(
        conn,
        organization_id,
        name,
        created_by,
        operation_id,
        visibility,
        description,
        object_format,
        parent_repository_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_creating_impl(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
    created_by: Uuid,
    operation_id: Uuid,
    visibility: &str,
    description: &str,
    object_format: &str,
    parent_repository_id: Option<Uuid>,
) -> Result<RepositoryRow, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "WITH retired AS (DELETE FROM repository_redirects WHERE organization_id=$2 AND lower(name)=lower($3)) \
         INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, description, object_format, parent_repository_id) VALUES ($1,$2,$3,$4,$5,'creating',$6,$7,$8,$9)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(name)
    .bind(created_by)
    .bind(operation_id)
    .bind(visibility)
    .bind(description)
    .bind(object_format)
    .bind(parent_repository_id)
    .execute(&mut *conn)
    .await?;
    find_by_operation(conn, operation_id)
        .await
        .map(|r| r.expect("just inserted"))
}

pub async fn set_status(
    conn: &mut PgConnection,
    id: Uuid,
    from: RepositoryStatus,
    to: RepositoryStatus,
) -> Result<bool, sqlx::Error> {
    let from_str = from.as_str();
    let to_str = to.as_str();
    let result = sqlx::query(
        "UPDATE repositories SET status=$1, updated_at=now(), deleted_at=CASE WHEN $1 = 'deleted' THEN now() ELSE deleted_at END WHERE id=$2 AND status=$3",
    )
    .bind(to_str)
    .bind(id)
    .bind(from_str)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn finish_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    actor: Uuid,
) -> Result<bool, sqlx::Error> {
    let changed = set_status(
        tx,
        id,
        RepositoryStatus::Deleting,
        RepositoryStatus::Deleted,
    )
    .await?;
    if !changed {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE repository_id = $2 AND revoked_at IS NULL",
    )
    .bind(actor)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    // A deleted fork can no longer be the source of an open pull request: close
    // those pulls with a recorded reason and the usual audit trail (M3).
    close_fork_pull_requests(tx, id, actor, "source repository deleted").await?;
    Ok(true)
}

/// Closes every open pull request whose head lives in `source_repository_id`,
/// recording `reason`, emitting a `pull_request.closed` audit event per pull and
/// cancelling any queued or validating merge intents, so a system close leaves
/// the same audit and intent state as an author close. Runs when a fork is
/// deleted.
pub async fn close_fork_pull_requests(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_repository_id: Uuid,
    actor: Uuid,
    reason: &str,
) -> Result<u64, sqlx::Error> {
    // The target repository and organization of each affected pull, so every
    // close is audited under the repository the pull request belongs to.
    let targets: Vec<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT pr.id, r.organization_id FROM pull_requests pr \
         JOIN repositories r ON r.id = pr.repository_id \
         WHERE pr.source_repository_id = $1 AND pr.state = 'open'",
    )
    .bind(source_repository_id)
    .fetch_all(&mut **tx)
    .await?;

    for (pull_id, organization_id) in targets {
        crate::event(
            &mut *tx,
            Some(organization_id),
            actor,
            "pull_request.closed",
        )
        .await?;
        sqlx::query(
            "UPDATE merge_intents SET state = 'cancelled', updated_at = now() \
             WHERE pull_request_id = $1 AND state IN ('queued', 'validating')",
        )
        .bind(pull_id)
        .execute(&mut **tx)
        .await?;
    }

    let result = sqlx::query(
        "UPDATE pull_requests SET state = 'closed', closed_at = now(), \
         closed_reason = $1, updated_at = now() \
         WHERE source_repository_id = $2 AND state = 'open'",
    )
    .bind(reason)
    .bind(source_repository_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

/// Bump `updated_at` so the lifecycle reconciler (which only visits rows older
/// than its grace) skips this row while a resumed create/delete is still doing
/// its storage stage.
pub async fn touch(conn: &mut PgConnection, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE repositories SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn find_by_operation(
    conn: &mut PgConnection,
    operation_id: Uuid,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE operation_id=$1"
    ))
    .bind(operation_id)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

pub async fn find_by_name(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2) AND status <> 'deleted'"
    ))
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?
    .as_ref()
    .map(row_to_repository);
    if row.is_some() {
        return Ok(row);
    }
    sqlx::query(
        "SELECT r.id, r.organization_id, r.name, r.description, r.visibility, r.archived, r.status, r.operation_id, r.created_by, r.object_format, r.parent_repository_id FROM repository_redirects d JOIN repositories r ON r.id = d.repository_id WHERE d.organization_id=$1 AND lower(d.name)=lower($2) AND r.status <> 'deleted'",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

pub async fn get(conn: &mut PgConnection, id: Uuid) -> Result<Option<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!("SELECT {REPO_COLS} FROM repositories WHERE id=$1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map(|r| r.as_ref().map(row_to_repository))
}

pub async fn pending(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    Ok(sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE status IN ('creating','deleting','failed') ORDER BY created_at LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(row_to_repository)
    .collect())
}

/// Locks the row if it is `active`, so a concurrent delete (which updates the
/// status) waits for the caller's transaction to commit. Call it inside the
/// mutation's transaction; false means the repository is no longer active.
/// `writes_row` must be true when the transaction goes on to update this row:
/// upgrading a share lock would deadlock two such requests, so they take
/// `FOR NO KEY UPDATE` and serialize instead.
pub async fn lock_active(
    conn: &mut PgConnection,
    id: Uuid,
    writes_row: bool,
) -> Result<bool, sqlx::Error> {
    let mode = if writes_row {
        "FOR NO KEY UPDATE"
    } else {
        "FOR SHARE"
    };
    Ok(sqlx::query(&format!(
        "SELECT 1 FROM repositories WHERE id = $1 AND status = 'active' {mode}"
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .is_some())
}

/// Starts its own transaction on `conn`, so inside a caller's transaction it acts as a savepoint.
pub async fn rename(
    conn: &mut PgConnection,
    id: Uuid,
    new_name: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = conn.begin().await?;
    let row = sqlx::query("SELECT organization_id, name FROM repositories WHERE id=$1 AND status <> 'deleted' FOR UPDATE",
    )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let organization_id: Uuid = row.get("organization_id");
    let old_name: String = row.get("name");
    sqlx::query(
        "DELETE FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(organization_id)
    .bind(&old_name)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO repository_redirects (organization_id, name, repository_id) VALUES ($1,$2,$3)",
    )
    .bind(organization_id)
    .bind(&old_name)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(organization_id)
    .bind(new_name)
    .execute(&mut *tx)
    .await?;
    let result = sqlx::query(
        "UPDATE repositories SET name=$1, updated_at=now() WHERE id=$2 AND status <> 'deleted'",
    )
    .bind(new_name)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Maps the stored `object_format` text to the storage object format, failing
/// closed to SHA-1 for any unknown value (the CHECK constraint admits only
/// 'sha1' and 'sha256').
pub fn object_format(value: &str) -> githearth_git::layout::ObjectFormat {
    match value {
        "sha256" => githearth_git::layout::ObjectFormat::Sha256,
        _ => githearth_git::layout::ObjectFormat::Sha1,
    }
}

pub fn authz_repository(row: &RepositoryRow) -> Repository {
    Repository {
        id: row.id,
        organization: row.organization_id,
        visibility: match row.visibility.as_str() {
            "public" => Visibility::Public,
            "internal" => Visibility::Internal,
            // The CHECK constraint leaves only 'private'; fail closed regardless.
            _ => Visibility::Private,
        },
        archived: row.archived,
    }
}

pub async fn list(
    conn: &mut PgConnection,
    organization_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE organization_id=$1 AND status <> 'deleted' AND ($2::uuid IS NULL OR id > $2) ORDER BY id LIMIT $3"
    ))
    .bind(organization_id)
    .bind(after)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map(|rows| rows.iter().map(row_to_repository).collect())
}

/// Repositories forked directly from `parent_id`, newest id order. Deleted
/// forks are omitted; callers still filter by read authorization.
pub async fn list_children(
    conn: &mut PgConnection,
    parent_id: Uuid,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE parent_repository_id=$1 AND status <> 'deleted' ORDER BY id"
    ))
    .bind(parent_id)
    .fetch_all(&mut *conn)
    .await
    .map(|rows| rows.iter().map(row_to_repository).collect())
}

pub async fn set_description(
    conn: &mut PgConnection,
    id: Uuid,
    description: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE repositories SET description=$1, updated_at=now() WHERE id=$2 AND status <> 'deleted'",
    )
    .bind(description)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn set_archived(
    conn: &mut PgConnection,
    id: Uuid,
    archived: bool,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE repositories SET archived=$1, updated_at=now() WHERE id=$2 AND status <> 'deleted'",
    )
    .bind(archived)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use githearth_core::RepositoryStatus;
    use sqlx::{ConnectOptions, PgPool};
    use std::str::FromStr;

    async fn repo_test_pool() -> PgPool {
        let cfg = sqlx::postgres::PgConnectOptions::from_str(
            &std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL"),
        )
        .unwrap()
        .disable_statement_logging();
        let pool = PgPool::connect_with(cfg).await.unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn object_format_maps_stored_text() {
        use githearth_git::layout::ObjectFormat;
        assert_eq!(object_format("sha256"), ObjectFormat::Sha256);
        assert_eq!(object_format("sha1"), ObjectFormat::Sha1);
        // Unknown values fail closed to SHA-1.
        assert_eq!(object_format("nonsense"), ObjectFormat::Sha1);
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn insert_and_find_by_name() {
        let pool = repo_test_pool().await;
        let org_id = Uuid::new_v4();
        let tag = format!("t-{}", Uuid::new_v4().simple());
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
            .bind(owner)
            .bind(&tag)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Test Org')")
            .bind(org_id)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let repo = insert_creating(
            &mut conn,
            org_id,
            "my-repo",
            owner,
            Uuid::new_v4(),
            "private",
            "",
        )
        .await
        .unwrap();
        let found = find_by_name(&mut conn, org_id, "my-repo")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, repo.id);
        assert_eq!(found.status, RepositoryStatus::Creating);

        let found2 = find_by_name(&mut conn, org_id, "MY-REPO")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found2.id, repo.id);

        assert!(
            find_by_name(&mut conn, org_id, "nonexistent")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn rename_and_redirect() {
        let pool = repo_test_pool().await;
        let org_id = Uuid::new_v4();
        let tag = format!("t-{}", Uuid::new_v4().simple());
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
            .bind(owner)
            .bind(&tag)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Test Org')")
            .bind(org_id)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let repo = insert_creating(
            &mut conn,
            org_id,
            "old-name",
            owner,
            Uuid::new_v4(),
            "private",
            "",
        )
        .await
        .unwrap();
        set_status(
            &mut conn,
            repo.id,
            RepositoryStatus::Creating,
            RepositoryStatus::Active,
        )
        .await
        .unwrap();
        assert!(rename(&mut conn, repo.id, "new-name").await.unwrap());
        let found = find_by_name(&mut conn, org_id, "new-name")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, repo.id);
        assert_eq!(found.name, "new-name");
        let redirect = find_by_name(&mut conn, org_id, "old-name")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(redirect.id, repo.id);
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn set_description_and_archived() {
        let pool = repo_test_pool().await;
        let org_id = Uuid::new_v4();
        let tag = format!("t-{}", Uuid::new_v4().simple());
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
            .bind(owner)
            .bind(&tag)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Test Org')")
            .bind(org_id)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let repo = insert_creating(
            &mut conn,
            org_id,
            "repo",
            owner,
            Uuid::new_v4(),
            "private",
            "",
        )
        .await
        .unwrap();
        set_status(
            &mut conn,
            repo.id,
            RepositoryStatus::Creating,
            RepositoryStatus::Active,
        )
        .await
        .unwrap();
        assert!(
            set_description(&mut conn, repo.id, "a description")
                .await
                .unwrap()
        );
        assert!(set_archived(&mut conn, repo.id, true).await.unwrap());
        let found = get(&mut conn, repo.id).await.unwrap().unwrap();
        assert_eq!(found.description, "a description");
        assert!(found.archived);
        assert!(set_archived(&mut conn, repo.id, false).await.unwrap());
        let found = get(&mut conn, repo.id).await.unwrap().unwrap();
        assert!(!found.archived);
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn list_pagination() {
        let pool = repo_test_pool().await;
        let org_id = Uuid::new_v4();
        let tag = format!("t-{}", Uuid::new_v4().simple());
        let owner = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
            .bind(owner)
            .bind(&tag)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Test Org')")
            .bind(org_id)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let mut ids = Vec::new();
        for i in 0..3 {
            let repo = insert_creating(
                &mut conn,
                org_id,
                &format!("repo-{i}"),
                owner,
                Uuid::new_v4(),
                "private",
                "",
            )
            .await
            .unwrap();
            ids.push(repo.id);
        }
        let all = list(&mut conn, org_id, None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        let mut sorted = ids;
        sorted.sort();
        let page = list(&mut conn, org_id, Some(sorted[0]), 10).await.unwrap();
        assert_eq!(page.len(), 2);
    }
}
