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
) -> Result<RepositoryRow, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        // Reusing a name retires its redirect, so the redirect cannot revive if this repository is deleted.
        "WITH retired AS (DELETE FROM repository_redirects WHERE organization_id=$2 AND lower(name)=lower($3)) \
         INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status) VALUES ($1,$2,$3,$4,$5,'creating')",
    )
    .bind(id)
    .bind(organization_id)
    .bind(name)
    .bind(created_by)
    .bind(operation_id)
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
        "SELECT {REPO_COLS} FROM repositories WHERE status IN ('creating','deleting') ORDER BY created_at LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(row_to_repository)
    .collect())
}

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
