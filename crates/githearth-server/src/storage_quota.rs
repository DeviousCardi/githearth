//! Git repository storage accounting for the published quotas.
//!
//! The object store has its own per-organization ledger (ADR 0014 §1); this
//! module is the second half of the published "50 GiB per organization" row:
//! it sums the `objects/` tree of every non-deleted repository in the
//! organization so a push or a server-side merge can be measured against the
//! tenant total. `docs/operations/quota.md` says what is counted and what the
//! walk costs.
//!
//! Both figures are bytes of regular files under `objects/`; symlinks are not
//! followed and `refs/`, `packed-refs` and journal files are not counted, which
//! is the same definition `repository_quota_bytes` uses. A repository whose
//! directory does not exist yet (just created, still `creating`) contributes
//! zero.
//!
//! ## Cost
//!
//! One walk of the organization's whole `objects/` set per push and per merge.
//! The walk is blocking and runs on the blocking pool while the caller holds
//! the per-repository lock, so a tenant with many repositories or many loose
//! objects pays a measurable delay before `receive-pack` starts. That is
//! deliberate: a cached counter can drift from the disk after a crash, a
//! restore or a hand-run `gc`, and the quota is a refusal, not an estimate.

use uuid::Uuid;

use crate::AppState;

/// Bytes of the repository being written and of every other non-deleted
/// repository in the same organization, measured over each `objects/` tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrganizationStorage {
    pub repository: u64,
    pub other_repositories: u64,
}

impl OrganizationStorage {
    pub fn organization_total(self) -> u64 {
        self.repository + self.other_repositories
    }
}

/// Measures [`OrganizationStorage`] for `repository` in `organization`.
///
/// Returns `Err` when the repository list cannot be read or a tree cannot be
/// walked; every caller refuses the write on an error, so an unmeasurable
/// organization cannot spend free quota.
pub(crate) async fn measure(
    state: &AppState,
    organization: Uuid,
    repository: Uuid,
) -> Result<OrganizationStorage, String> {
    measure_inner(state, organization, repository, false).await
}

/// Bytes of every repository in the organization except `repository`, for the
/// push hook: the hook measures the target's own `objects/` (and quarantine)
/// itself, so walking it twice would only slow the push down.
pub(crate) async fn other_repositories(
    state: &AppState,
    organization: Uuid,
    repository: Uuid,
) -> Result<u64, String> {
    measure_inner(state, organization, repository, true)
        .await
        .map(|storage| storage.other_repositories)
}

async fn measure_inner(
    state: &AppState,
    organization: Uuid,
    repository: Uuid,
    skip_repository: bool,
) -> Result<OrganizationStorage, String> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE organization_id = $1 AND status <> 'deleted'",
    )
    .bind(organization)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| format!("cannot list repositories for storage accounting: {e}"))?;

    let storage = state.storage.clone();
    tokio::task::spawn_blocking(move || {
        let mut repository_bytes = 0;
        let mut other_repositories = 0;
        for id in ids {
            if skip_repository && id == repository {
                continue;
            }
            let bytes = objects_bytes(&storage.repository_path(id))?;
            if id == repository {
                repository_bytes = bytes;
            } else {
                other_repositories += bytes;
            }
        }
        Ok(OrganizationStorage {
            repository: repository_bytes,
            other_repositories,
        })
    })
    .await
    .map_err(|_| "storage accounting task panicked".to_owned())?
}

/// Size of one repository's `objects/` tree. A repository that does not exist
/// yet is zero; any other filesystem error is returned so the caller refuses
/// the write instead of treating unmeasurable bytes as free.
fn objects_bytes(repository: &std::path::Path) -> Result<u64, String> {
    let objects = repository.join("objects");
    match std::fs::metadata(&objects) {
        Ok(meta) if meta.is_dir() => githearth_git::layout::dir_size(&objects, None)
            .map_err(|e| format!("cannot measure repository storage: {e}")),
        Ok(_) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("cannot measure repository storage: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_repository_directory_is_zero_and_a_present_one_is_summed() {
        let root = std::env::temp_dir().join(format!("githearth-quota-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("objects/a/b")).unwrap();
        assert_eq!(objects_bytes(&root).unwrap(), 0, "empty tree has no bytes");
        std::fs::write(root.join("objects/a/b/blob"), vec![0u8; 4096]).unwrap();
        assert_eq!(objects_bytes(&root).unwrap(), 4096);
        std::fs::write(root.join("objects/top"), vec![0u8; 10]).unwrap();
        assert_eq!(objects_bytes(&root).unwrap(), 4106);
        let _ = std::fs::remove_dir_all(&root);

        let absent = std::env::temp_dir().join(format!("githearth-quota-{}", Uuid::new_v4()));
        assert_eq!(
            objects_bytes(&absent).unwrap(),
            0,
            "a missing repository counts zero"
        );
    }

    #[test]
    fn the_organization_total_adds_both_figures() {
        let storage = OrganizationStorage {
            repository: 10,
            other_repositories: 25,
        };
        assert_eq!(storage.organization_total(), 35);
    }
}
