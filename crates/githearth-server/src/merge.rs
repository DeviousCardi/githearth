//! Merge-record replay (ADR 0004 "Lost metadata after the Git commit").

use githearth_git::journal::Record;
use sqlx::PgConnection;
use uuid::Uuid;

/// Applies a committed merge journal record to PostgreSQL metadata.
///
/// Person A's `record_merge_result` (ADR 0004) is idempotent per journal
/// sequence, so applying the same record twice changes nothing. This stub
/// returns `Ok` until that function exists; the reconciler still advances the
/// high-water mark past the record, exactly as it does for pushes.
pub(crate) async fn apply_merge_record(
    _conn: &mut PgConnection,
    _repository: Uuid,
    _prepared: &Record,
) -> Result<(), crate::pushes::ReplayError> {
    Ok(())
}
