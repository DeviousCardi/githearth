use githearth_git::journal::{Kind, Origin, Record, classify};
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
        let credential: Option<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT credential_kind, credential_id FROM push_credentials WHERE operation_id = $1",
        )
        .bind(prepared.operation)
        .fetch_optional(&mut *tx)
        .await?;
        let (credential_kind, credential_id) = match credential {
            Some((kind, id)) => (Some(kind), id),
            None => (None, None),
        };
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
            "credential_kind": credential_kind,
            "credential_id": credential_id,
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

/// Deletes `push_credentials` rows older than one day whose push has already
/// been applied. Safety comes from the EXISTS(repository_pushes.operation_id =
/// push_credentials.operation_id) guard, which keeps any not-yet-applied
/// push's credential regardless of age. Rows without an applied push (a
/// refused or still-pending push) are kept.
pub async fn sweep_credentials(conn: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM push_credentials
         WHERE created_at < now() - interval '1 day'
           AND EXISTS (
             SELECT 1 FROM repository_pushes p
             WHERE p.operation_id = push_credentials.operation_id
           )",
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Journal replay (ADR 0003 "Recovery")
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ReplaySummary {
    pub committed: usize,
    pub aborted: usize,
}

#[derive(Debug)]
pub enum ReplayError {
    Diverged { repository: Uuid, sequence: u64 },
    Journal(githearth_git::journal::JournalError),
    Storage(githearth_git::StorageError),
    Database(sqlx::Error),
    Invalid(&'static str),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::Diverged {
                repository,
                sequence,
            } => write!(
                f,
                "repository {repository} diverged at journal sequence {sequence}; repair required"
            ),
            ReplayError::Journal(e) => write!(f, "journal error: {e}"),
            ReplayError::Storage(e) => write!(f, "storage error: {e}"),
            ReplayError::Database(e) => write!(f, "database error: {e}"),
            ReplayError::Invalid(m) => write!(f, "invalid journal record: {m}"),
        }
    }
}

impl std::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReplayError::Journal(e) => Some(e),
            ReplayError::Storage(e) => Some(e),
            ReplayError::Database(e) => Some(e),
            ReplayError::Diverged { .. } | ReplayError::Invalid(_) => None,
        }
    }
}

impl From<sqlx::Error> for ReplayError {
    fn from(e: sqlx::Error) -> Self {
        ReplayError::Database(e)
    }
}

impl From<PushError> for ReplayError {
    fn from(e: PushError) -> Self {
        match e {
            PushError::Database(e) => ReplayError::Database(e),
            PushError::Invalid(m) => ReplayError::Invalid(m),
        }
    }
}

/// Replays journal records newer than the repository's high-water mark into
/// PostgreSQL, exactly per ADR 0003.  The caller must already hold the
/// per-repository lock.
pub async fn replay(
    state: &crate::AppState,
    repository: Uuid,
) -> Result<ReplaySummary, ReplayError> {
    let mut conn = state.pool.acquire().await?;
    let applied = applied_through(&mut conn, repository).await?;
    drop(conn);

    let storage = state.storage.clone();
    let repo_path = storage.repository_path(repository);

    let (records, refs_now) = tokio::task::spawn_blocking(move || {
        let journal =
            githearth_git::journal::Journal::open(&repo_path).map_err(ReplayError::Journal)?;
        let records: Vec<Record> = journal.records_after(applied).cloned().collect();
        let refs_now =
            githearth_git::layout::read_refs(&repo_path).map_err(ReplayError::Storage)?;
        Ok::<_, ReplayError>((records, refs_now))
    })
    .await
    .map_err(|_| ReplayError::Invalid("journal task panicked"))??;

    let mut summary = ReplaySummary::default();
    let mut conn = state.pool.acquire().await?;
    let mut i = 0;
    while i < records.len() {
        let prepared = records[i].clone();
        if prepared.kind != Kind::Prepared {
            // A resolved marker with no preceding prepared record: skip past it.
            advance(&mut conn, repository, prepared.sequence).await?;
            i += 1;
            continue;
        }
        let marker = match records.get(i + 1) {
            Some(next) if next.kind != Kind::Prepared && next.operation == prepared.operation => {
                Some(next.kind)
            }
            _ => None,
        };
        match classify(&prepared, marker, &refs_now) {
            githearth_git::journal::Outcome::Committed => {
                if prepared.origin == Some(Origin::Merge) {
                    crate::merge::apply_merge_record(&mut conn, repository, &prepared).await?;
                } else {
                    apply_committed(&mut conn, repository, &prepared).await?;
                }
                let seq = marker
                    .map(|_| records[i + 1].sequence)
                    .unwrap_or(prepared.sequence);
                advance(&mut conn, repository, seq).await?;
                summary.committed += 1;
                i += if marker.is_some() { 2 } else { 1 };
            }
            githearth_git::journal::Outcome::CommittedMarkerLost => {
                // Append the marker, but advance only to the prepared record:
                // the marker lands at the end of the journal, past records this
                // pass has not applied yet.  It is skipped as a lone marker
                // on the next pass.
                append_marker(
                    &state.storage,
                    repository,
                    prepared.operation,
                    Kind::Committed,
                )
                .await?;
                if prepared.origin == Some(Origin::Merge) {
                    crate::merge::apply_merge_record(&mut conn, repository, &prepared).await?;
                } else {
                    apply_committed(&mut conn, repository, &prepared).await?;
                }
                advance(&mut conn, repository, prepared.sequence).await?;
                summary.committed += 1;
                i += 1;
            }
            githearth_git::journal::Outcome::Aborted => {
                advance(&mut conn, repository, records[i + 1].sequence).await?;
                summary.aborted += 1;
                i += 2;
            }
            githearth_git::journal::Outcome::AbortedMarkerLost => {
                append_marker(
                    &state.storage,
                    repository,
                    prepared.operation,
                    Kind::Aborted,
                )
                .await?;
                advance(&mut conn, repository, prepared.sequence).await?;
                summary.aborted += 1;
                i += 1;
            }
            githearth_git::journal::Outcome::Diverged => {
                tracing::error!(
                    repository_id = %repository,
                    sequence = prepared.sequence,
                    "journal replay diverged; repository needs repair"
                );
                return Err(ReplayError::Diverged {
                    repository,
                    sequence: prepared.sequence,
                });
            }
        }
    }
    Ok(summary)
}

async fn append_marker(
    storage: &std::sync::Arc<githearth_git::layout::Storage>,
    repository: Uuid,
    operation: Uuid,
    kind: Kind,
) -> Result<u64, ReplayError> {
    let storage = storage.clone();
    let repo_path = storage.repository_path(repository);
    tokio::task::spawn_blocking(move || {
        let mut journal =
            githearth_git::journal::Journal::open(&repo_path).map_err(ReplayError::Journal)?;
        journal
            .append_marker(operation, kind)
            .map_err(ReplayError::Journal)
    })
    .await
    .map_err(|_| ReplayError::Invalid("journal task panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn push_error_display_source_and_from() {
        assert_eq!(
            PushError::Invalid("bad").to_string(),
            "invalid push record: bad"
        );
        let db = PushError::Database(sqlx::Error::RowNotFound);
        assert!(db.to_string().contains("database error"));
        assert!(db.source().is_some());
        assert!(PushError::Invalid("x").source().is_none());
        assert!(matches!(
            PushError::from(sqlx::Error::RowNotFound),
            PushError::Database(_)
        ));
    }

    #[test]
    fn replay_error_display_source_and_from() {
        let id = Uuid::new_v4();
        let diverged = ReplayError::Diverged {
            repository: id,
            sequence: 7,
        };
        assert!(diverged.to_string().contains(&id.to_string()));
        assert!(diverged.source().is_none());

        let journal = ReplayError::Journal(githearth_git::journal::JournalError::Io(
            std::io::Error::other("io"),
        ));
        assert!(journal.to_string().contains("journal error"));
        assert!(journal.source().is_some());

        let storage = ReplayError::Storage(githearth_git::StorageError::Invalid("s".into()));
        assert!(storage.to_string().contains("storage error"));
        assert!(storage.source().is_some());

        let db = ReplayError::Database(sqlx::Error::RowNotFound);
        assert!(db.to_string().contains("database error"));
        assert!(db.source().is_some());

        let invalid = ReplayError::Invalid("m");
        assert!(invalid.to_string().contains("invalid journal record"));
        assert!(invalid.source().is_none());

        assert!(matches!(
            ReplayError::from(sqlx::Error::RowNotFound),
            ReplayError::Database(_)
        ));
        assert!(matches!(
            ReplayError::from(PushError::Invalid("m")),
            ReplayError::Invalid(_)
        ));
        assert!(matches!(
            ReplayError::from(PushError::Database(sqlx::Error::RowNotFound)),
            ReplayError::Database(_)
        ));
    }

    #[test]
    fn valid_oid_and_ref_name_rules() {
        assert!(valid_oid(&"a".repeat(40)));
        assert!(valid_oid(&"b".repeat(64)));
        assert!(!valid_oid("short"));
        assert!(!valid_oid(&"Z".repeat(40)));
        assert!(valid_ref_name("refs/heads/main"));
        assert!(valid_ref_name("refs/tags/v1"));
        assert!(!valid_ref_name("refs/heads/"));
        assert!(!valid_ref_name("refs/notes/x"));
        assert!(!valid_ref_name("HEAD"));
    }
}
