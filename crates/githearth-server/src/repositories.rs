use githearth_authz::{Repository, Visibility};
use githearth_core::RepositoryStatus;
use sqlx::{Connection, PgConnection, Row};
use uuid::Uuid;

#[derive(Debug)]
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
    }
}

const REPO_COLS: &str = "id, organization_id, name, description, visibility, archived, status, operation_id, created_by";

pub async fn insert_creating(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
    created_by: Uuid,
    operation_id: Uuid,
    visibility: &str,
    description: &str,
) -> Result<RepositoryRow, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "WITH retired AS (DELETE FROM repository_redirects WHERE organization_id=$2 AND lower(name)=lower($3)) \
         INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, description) VALUES ($1,$2,$3,$4,$5,'creating',$6,$7)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(name)
    .bind(created_by)
    .bind(operation_id)
    .bind(visibility)
    .bind(description)
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
        "SELECT r.id, r.organization_id, r.name, r.description, r.visibility, r.archived, r.status, r.operation_id, r.created_by FROM repository_redirects d JOIN repositories r ON r.id = d.repository_id WHERE d.organization_id=$1 AND lower(d.name)=lower($2) AND r.status <> 'deleted'",
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
