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

/// Serialize all uses of one `(organization, name)` for the rest of the
/// transaction. A transfer's claim row is uncommitted, so visibility alone
/// cannot reserve a name; the advisory lock is what makes a concurrent create
/// or rename wait for the move instead of taking the name underneath it. The
/// key is case-folded like the unique indexes.
async fn lock_name(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{organization_id}/{}", name.to_lowercase()))
        .execute(conn)
        .await?;
    Ok(())
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
    // Serialize name use with a transfer that is claiming the same name. The
    // claim row is uncommitted in the transfer's transaction, so a plain
    // existence check cannot see it; this transaction-scoped advisory lock is
    // taken in both paths and makes the create wait for the move to finish
    // (then fail on the repository unique index if it committed).
    lock_name(&mut *conn, organization_id, name).await?;
    // A create must not take a name a transfer is about to land on at this
    // organization: the claim is an intent, and the insert below would take the
    // name silently, so it is refused first, with the same error a taken name
    // gives (`23505`).
    let claimed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_claims WHERE organization_id=$1 AND lower(name)=lower($2) AND expires_at > clock_timestamp())",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if claimed {
        return Err(sqlx::Error::RowNotFound);
    }
    // A name a repository left by a transfer is a tombstone (ADR 0003): the old
    // path answers `410` and must never resolve to a repository that later takes
    // the name, so the name cannot be created again at the source. The same
    // `RowNotFound` reports it as a taken name (`409`), revealing nothing.
    let tombstoned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2) AND transferred_at IS NOT NULL)",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if tombstoned {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query(
        "WITH retired AS (DELETE FROM repository_redirects WHERE organization_id=$2 AND lower(name)=lower($3) AND transferred_at IS NULL) \
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
    // ADR 0003: a path a repository left by a transfer is gone, not a redirect.
    // It must be checked before the redirect fallback, and a name taken by a new
    // repository retires the row (see `insert_creating_impl`), so the lookup can
    // never answer a transferred name with a repository.
    let retired: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_redirects d WHERE d.organization_id=$1 \
         AND lower(d.name)=lower($2) AND d.transferred_at IS NOT NULL)",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if retired {
        return Ok(None);
    }
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
        "SELECT r.id, r.organization_id, r.name, r.description, r.visibility, r.archived, r.status, r.operation_id, r.created_by, r.object_format, r.parent_repository_id \
         FROM repository_redirects d JOIN repositories r ON r.id = d.repository_id WHERE d.organization_id=$1 AND lower(d.name)=lower($2) AND d.transferred_at IS NULL AND r.status <> 'deleted'",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

/// Find a repository by its current name whatever its status, and then any
/// redirected name. Deleted and restoring rows are included so a caller can
/// answer `410 Gone` for a path that still names one, instead of the `404` a
/// never-existing name gets. Redirects stay a read convenience for a rename;
/// the `410` rule is decided by the caller from the returned row.
///
/// A redirect row that represents a **transfer** tombstone
/// (`transferred_at IS NOT NULL`) is never followed here, and its target lives
/// in another organization by construction: resolving it would hand the caller
/// a row from the target organization while the caller is authorized against
/// the source one. Callers that must answer `410` for a moved-away path check
/// [`path_moved_away`] **before** this lookup, so the tombstone wins over both
/// the exact row and the redirect fallback; this filter is what keeps the
/// fallback from resurrecting one.
pub async fn find_by_name_any_status(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    if let Some(row) = find_exact_any_status(conn, organization_id, name).await? {
        return Ok(Some(row));
    }
    sqlx::query(
        "SELECT r.id, r.organization_id, r.name, r.description, r.visibility, r.archived, r.status, r.operation_id, r.created_by, r.object_format, r.parent_repository_id FROM repository_redirects d JOIN repositories r ON r.id = d.repository_id WHERE d.organization_id=$1 AND lower(d.name)=lower($2) AND d.transferred_at IS NULL",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

/// The row with exactly this name, whatever its status, without following
/// redirects. A caller that must not let a live replacement inherit an older
/// row's answers uses this to tell the two apart.
pub async fn find_exact_any_status(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2)"
    ))
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

/// Whether `owner/name` used to be a repository that has since moved to another
/// organization. The API, the browser and the Git transports answer such a path
/// `410 Gone`, never a redirect and never whatever else may hold the name now.
///
/// This is the single transfer-tombstone test: a restore refuses to take the
/// name back under it, `retire_redirect` leaves it alone, and the lookups in
/// this module run it before their redirect fallback.
pub async fn path_moved_away(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2) AND transferred_at IS NOT NULL)",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_one(&mut *conn)
    .await
}

/// Retire the rename redirect that still claims `name` for another repository
/// in this organization.
///
/// `rename` keeps the old name as a redirect; after a restore puts the
/// original repository back at that name, a stale redirect would resolve the
/// restored path to the renamed repository instead of answering `410`. Only
/// rename redirects (`transferred_at IS NULL`) are retired: a transfer
/// tombstone is not a redirect and must survive, so the transferred path keeps
/// answering `410`.
pub async fn retire_redirect(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM repository_redirects \
         WHERE organization_id=$1 AND lower(name)=lower($2) AND transferred_at IS NULL",
    )
    .bind(organization_id)
    .bind(name)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// A deleted or in-progress restore row under its exact current name. A live
/// repository that reused the name is deliberately not returned: restore is
/// about the trashed row, and a name collision must be refused rather than
/// resolving to the new owner.
pub async fn find_restorable(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<Option<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2) AND status IN ('deleted','restoring') ORDER BY (status = 'restoring') DESC, deleted_at DESC NULLS LAST, id LIMIT 1"
    ))
    .bind(organization_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .map(|r| r.as_ref().map(row_to_repository))
}

/// Deleted repositories of an organization, newest first, for the operator
/// page that offers a restore form. Callers still authorize each row.
pub async fn list_deleted(
    conn: &mut PgConnection,
    organization_id: Uuid,
    limit: i64,
) -> Result<Vec<RepositoryRow>, sqlx::Error> {
    sqlx::query(&format!(
        "SELECT {REPO_COLS} FROM repositories WHERE organization_id=$1 AND status='deleted' ORDER BY deleted_at DESC NULLS LAST, id LIMIT $2"
    ))
    .bind(organization_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map(|rows| rows.iter().map(row_to_repository).collect())
}

/// Complete a restore in the same transaction as its audit event: the row
/// returns to `active` with no deletion timestamp. The CAS keeps a concurrent
/// delete or a lost restore claim from making a row active twice.
pub async fn finish_restore(conn: &mut PgConnection, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE repositories SET status='active', deleted_at=NULL, updated_at=now() \
         WHERE id=$1 AND status='restoring'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Go back from `restoring` to `deleted` after a failed move or a lost trash
/// entry, **preserving** the original `deleted_at`.
///
/// [`set_status`] stamps `deleted_at=now()` whenever the target is `deleted`,
/// which would restart the retention clock (and reorder the operator's
/// deleted list) after an attempt that changed nothing. The row was deleted
/// before it entered `restoring`, so its timestamp is still the right one.
pub async fn revert_restore(conn: &mut PgConnection, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE repositories SET status='deleted', updated_at=now() \
         WHERE id=$1 AND status='restoring'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
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
        "SELECT {REPO_COLS} FROM repositories WHERE status IN ('creating','deleting','restoring','failed') ORDER BY created_at LIMIT $1"
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
    // Take the name lock before the row lock so a transfer claiming this name
    // and a rename to it cannot deadlock: both take the name first.
    let org_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT organization_id FROM repositories WHERE id=$1 AND status <> 'deleted'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(organization_id) = org_id else {
        return Ok(false);
    };
    lock_name(&mut tx, organization_id, new_name).await?;
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
    // A name a transfer retired stays retired: renaming onto it would make the
    // old path resolve again, which ADR 0003 forbids. The caller reports the
    // same taken name a unique violation does.
    if path_moved_away(&mut tx, organization_id, new_name).await? {
        return Err(sqlx::Error::RowNotFound);
    }
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
        "DELETE FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2) AND transferred_at IS NULL",
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
    // A rename only writes the same-organization redirect above; the row it
    // leaves behind after a move between organizations is a transfer's.
    Ok(result.rows_affected() > 0)
}

/// Record a transfer's intent to take a name at the target and take the
/// transaction-scoped name lock that actually reserves it. The lock is what a
/// concurrent create or rename waits on; the row is deleted in the same
/// transaction on success and rolled back on a refusal, so it is only ever an
/// in-transaction intent marker, never a committed reservation. The create path
/// also refuses a claim it can see, which is a second line of defence if a
/// future protocol commits claims first. Returns false when a repository
/// already holds the name, in which case no claim is written.
///
/// Call it inside the transfer's transaction, before the source row is locked,
/// so the name lock is held for the rest of the move.
pub async fn claim_name(
    conn: &mut PgConnection,
    organization_id: Uuid,
    name: &str,
) -> Result<bool, sqlx::Error> {
    // Held until the transfer's transaction ends, so a create or rename that
    // reaches the same name waits for the move's outcome.
    lock_name(conn, organization_id, name).await?;
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2) AND status <> 'deleted')",
    )
    .bind(organization_id)
    .bind(name)
    .fetch_one(&mut *conn)
    .await?;
    if taken {
        return Ok(false);
    }
    // The row commits with the caller's transaction. That is the point: until
    // the transfer commits, the claim exists for every other transaction, so a
    // concurrent create sees it; if the transfer refuses, it is gone.
    sqlx::query(
        "INSERT INTO repository_claims (organization_id, name, expires_at) VALUES ($1,$2,clock_timestamp() + interval '5 minutes') \
         ON CONFLICT (organization_id, lower(name)) DO UPDATE SET expires_at = EXCLUDED.expires_at \
         WHERE repository_claims.expires_at <= clock_timestamp()",
    )
    .bind(organization_id)
    .bind(name)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// Whether `name` is already used at the organization: by a repository, by a
/// transfer claim visible to this transaction, or by a name a transfer retired
/// (a tombstone, which may never be reused).
async fn name_taken(
    conn: &mut PgConnection,
    target_org: Uuid,
    name: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2) AND status <> 'deleted') \
         OR EXISTS(SELECT 1 FROM repository_claims WHERE organization_id=$1 AND lower(name)=lower($2) \
                    AND expires_at > clock_timestamp()) \
         OR EXISTS(SELECT 1 FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2) \
                    AND transferred_at IS NOT NULL)",
    )
    .bind(target_org)
    .bind(name)
    .fetch_one(conn)
    .await
}

/// Whether a transferred repository may land at `target_org` under the name it
/// already has. A transfer keeps the name and never picks another (U07, ADR
/// 0003: a name collision at the target is refused), so a caller that wants a
/// different name renames first and transfers afterwards. Call it inside the
/// transfer transaction; the name lock taken by `claim_name` (and the unique
/// index the move lands on) closes the race between this check and the update.
pub async fn transfer_name_free(
    conn: &mut PgConnection,
    target_org: Uuid,
    requested: &str,
) -> Result<bool, sqlx::Error> {
    Ok(!name_taken(conn, target_org, requested).await?)
}

/// Finish a transfer inside its transaction. The source row is assumed locked.
pub struct TransferRows {
    /// The name the repository had at the source, recorded as gone.
    pub old_name: String,
    pub new_name: String,
    pub target_org: Uuid,
    pub source_org: Uuid,
    pub repository_id: Uuid,
    pub actor: Uuid,
}

impl TransferRows {
    pub async fn apply(
        self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<i64, sqlx::Error> {
        // The old owner/name is gone for good: no repository answers to it and
        // the lookup reports it as such instead of falling through to whatever
        // else may later hold that name (ADR 0003). A name already retired by an
        // earlier rename of this repository is replaced, not duplicated.
        sqlx::query(
            "DELETE FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2)",
        )
        .bind(self.source_org)
        .bind(&self.old_name)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO repository_redirects (organization_id, name, repository_id, transferred_at) \
             VALUES ($1,$2,$3, now())",
        )
        .bind(self.source_org)
        .bind(&self.old_name)
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?;
        // Every name this repository left behind by a rename *inside* the
        // source is an alias of the same repository. Renaming first and then
        // transferring is the documented way to land under another name, so
        // those rows are retired with the move too: an alias left live would
        // keep resolving across organizations and would be reusable by a later
        // create, making the old path answer a new repository (U07, ADR 0003).
        sqlx::query(
            "UPDATE repository_redirects SET transferred_at = now() \
             WHERE organization_id = $1 AND repository_id = $2 AND transferred_at IS NULL",
        )
        .bind(self.source_org)
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?;
        // A name claimed for the target by a create that has not landed yet.
        sqlx::query(
            "DELETE FROM repository_claims WHERE organization_id=$1 AND lower(name)=lower($2)",
        )
        .bind(self.target_org)
        .bind(&self.new_name)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE repositories SET organization_id=$1, name=$2, updated_at=now() WHERE id=$3",
        )
        .bind(self.target_org)
        .bind(&self.new_name)
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?;
        // Every repository-scoped grant on the moved repository is revoked: a
        // grant's subject must belong to the organization that owns the
        // repository (ADR 0002), and after the move the source subjects no
        // longer do. No organization-scoped grant is touched. A source
        // organization's grant reaches a repository only while the repository
        // belongs to that organization, so after the move the set of
        // organization-scoped grants that still reach it is empty; revoking
        // them would strip the source organization's own authority (and its
        // last owner) for no benefit.
        let revoked = sqlx::query(
            "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
             WHERE revoked_at IS NULL AND repository_id = $2 AND scope_kind = 'repository'",
        )
        .bind(self.actor)
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?
        .rows_affected() as i64;
        // A fork's parent is the repository it was forked from, and a fork
        // belongs to its parent's organization. The moved repository is no
        // longer reachable as a parent from where it was forked, so the link is
        // dropped; the fork keeps its own organization, visibility and grants.
        sqlx::query(
            "UPDATE repositories SET parent_repository_id = NULL, updated_at = now() \
             WHERE parent_repository_id = $1",
        )
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?;
        // Queued merges cannot run against a repository that moved: their
        // approval and check digest is computed for this organization.
        sqlx::query(
            "UPDATE merge_intents SET state = 'cancelled', updated_at = now() \
             WHERE repository_id = $1 AND state IN ('queued', 'validating')",
        )
        .bind(self.repository_id)
        .execute(&mut **tx)
        .await?;
        Ok(revoked)
    }
}

/// Open pull requests, with the number they were given, so a close can report it.
#[derive(sqlx::FromRow)]
pub struct OpenPull {
    pub id: Uuid,
    pub number: i32,
    pub source_repository_id: Uuid,
    pub organization_id: Uuid,
}

/// Every open pull request on the repository: same-repository ones (their head
/// is the repository's own branch) and cross-repository ones from a fork.
pub async fn open_pulls(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Vec<OpenPull>, sqlx::Error> {
    sqlx::query_as::<_, OpenPull>(
        "SELECT pr.id, pr.number, pr.source_repository_id, r.organization_id \
         FROM pull_requests pr JOIN repositories r ON r.id = pr.repository_id \
         WHERE pr.repository_id = $1 AND pr.state = 'open' ORDER BY pr.number",
    )
    .bind(repository_id)
    .fetch_all(conn)
    .await
}

/// Close one open pull request with a stated reason, cancel its queued merge
/// intents, and audit the close under the organization that owns it.
pub async fn close_pull(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    pull: &OpenPull,
    actor: Uuid,
    reason: &str,
    policy: &str,
) -> Result<(), sqlx::Error> {
    crate::event_with(
        tx,
        Some(pull.organization_id),
        actor,
        "pull_request.closed",
        serde_json::json!({
            "number": pull.number,
            "reason": reason,
            "system": true,
            "policy": policy,
        }),
    )
    .await?;
    sqlx::query(
        "UPDATE merge_intents SET state = 'cancelled', updated_at = now() \
         WHERE pull_request_id = $1 AND state IN ('queued', 'validating')",
    )
    .bind(pull.id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE pull_requests SET state = 'closed', closed_at = now(), closed_reason = $1, updated_at = now() \
         WHERE id = $2 AND state = 'open'",
    )
    .bind(reason)
    .bind(pull.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
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
