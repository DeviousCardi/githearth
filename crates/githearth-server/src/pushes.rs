use githearth_git::journal::{Kind, Record};
use sqlx::{Connection, PgConnection};
use std::fmt;
use uuid::Uuid;

#[derive(Debug)]
pub enum PushError {
    Invalid(&'static str),
    Database(sqlx::Error),
}

impl fmt::Display for PushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PushError::Invalid(msg) => write!(f, "invalid push record: {msg}"),
            PushError::Database(err) => write!(f, "database error: {err}"),
        }
    }
}

impl std::error::Error for PushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PushError::Database(err) => Some(err),
            PushError::Invalid(_) => None,
        }
    }
}

impl From<sqlx::Error> for PushError {
    fn from(err: sqlx::Error) -> Self {
        PushError::Database(err)
    }
}

fn valid_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn valid_ref_name(s: &str) -> bool {
    if let Some(tail) = s.strip_prefix("refs/heads/") {
        !tail.is_empty()
    } else if let Some(tail) = s.strip_prefix("refs/tags/") {
        !tail.is_empty()
    } else {
        false
    }
}

/// Records a committed push. Idempotent, and it never moves `journal_applied`: sequences are
/// not one-per-push (markers, interleaved pushes), so only the reconciler's in-order pass may
/// call [`advance`].
pub async fn apply_committed(
    conn: &mut PgConnection,
    repository_id: Uuid,
    prepared: &Record,
) -> Result<bool, PushError> {
    if prepared.kind != Kind::Prepared {
        return Err(PushError::Invalid("kind must be Prepared"));
    }
    if prepared.updates.is_empty() {
        return Err(PushError::Invalid("updates must not be empty"));
    }
    let actor = prepared
        .actor
        .ok_or(PushError::Invalid("actor is required"))?;
    for u in &prepared.updates {
        if !valid_oid(&u.old) {
            return Err(PushError::Invalid("invalid old OID"));
        }
        if !valid_oid(&u.new) {
            return Err(PushError::Invalid("invalid new OID"));
        }
        if u.old.len() != u.new.len() {
            return Err(PushError::Invalid("old and new OIDs differ in length"));
        }
        if !valid_ref_name(&u.reference) {
            return Err(PushError::Invalid(
                "ref must start with refs/heads/ or refs/tags/",
            ));
        }
    }

    let mut tx = conn.begin().await?;
    let rows_affected = sqlx::query(
        "INSERT INTO repository_pushes (repository_id, journal_sequence, operation_id, actor_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(repository_id)
    .bind(prepared.sequence as i64)
    .bind(prepared.operation)
    .bind(actor)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let inserted = rows_affected > 0;

    if inserted {
        for u in &prepared.updates {
            sqlx::query(
                "INSERT INTO ref_updates (repository_id, journal_sequence, ref_name, old_oid, new_oid)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(repository_id)
            .bind(prepared.sequence as i64)
            .bind(&u.reference)
            .bind(&u.old)
            .bind(&u.new)
            .execute(&mut *tx)
            .await?;
        }
        let audit_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_events (id, organization_id, actor_id, kind)
             SELECT $1, r.organization_id, $2, 'repository.pushed'
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(actor)
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO outbox_events (id, organization_id, kind, payload)
             SELECT $1, r.organization_id, 'repository.pushed', $2
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(serde_json::json!({
            "repository_id": repository_id,
            "journal_sequence": prepared.sequence,
            "operation_id": prepared.operation,
            "actor_id": actor,
            "updates": prepared.updates.iter().map(|u| serde_json::json!({
                "ref": u.reference,
                "old": u.old,
                "new": u.new,
            })).collect::<Vec<_>>(),
        }))
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(inserted)
}

/// Declares every journal record up to `sequence` resolved. Reconciler only, in journal order.
pub async fn advance(
    conn: &mut PgConnection,
    repository_id: Uuid,
    sequence: u64,
) -> Result<(), PushError> {
    sqlx::query(
        "UPDATE repositories SET journal_applied = GREATEST(journal_applied, $1) WHERE id = $2",
    )
    .bind(sequence as i64)
    .bind(repository_id)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn applied_through(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let val: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id = $1")
        .bind(repository_id)
        .fetch_one(conn)
        .await?;
    Ok(val as u64)
}
