//! Read-only repository browsing API (trees, files, commits, diffs).
//!
//! Authorization matches every other repository read: an authenticated bearer
//! token, the repository loaded by name, and a `repository.read` decision.
//! Non-active repositories are unreadable (404 with no detail). Each Git
//! invocation set takes a `git_slots` permit and runs on the blocking pool.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use githearth_authz::Action;
use githearth_core::RepositoryStatus;
use githearth_git::{StorageError, browse};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;
use std::time::Duration;
use uuid::Uuid;

use crate::{ApiError, AppState, authenticate, invalid, repository_api, repository_api::Loaded};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/refs",
            get(refs),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/tree/{rev}",
            get(tree),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/blob/{rev}",
            get(blob),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/commits/{rev}",
            get(log),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/commit/{id}",
            get(commit),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/compare/{spec}",
            get(compare),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn revision_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "revision not found")
}

/// Authenticate, load the repository by name and require read access. Archived
/// repositories stay readable: `archived` is a flag, not a status — an
/// archived repository still has status `active`. Only non-`active` statuses
/// are treated like a missing repository (404).
async fn load_readable(
    state: &AppState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
) -> Result<Loaded, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, headers).await?;
    let loaded = repository_api::load_repo(state, actor.id, auth_age, org, repo).await?;
    repository_api::require(state, &loaded, Action::RepositoryRead)?;
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_found());
    }
    Ok(loaded)
}

async fn acquire_permit(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    state
        .git_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "git requests busy"))
}

/// Run one Git invocation set on the blocking pool, holding the permit for the
/// whole call so a cancelled request cannot release it while Git still runs.
pub(crate) async fn git_read<T>(
    state: &AppState,
    repo_id: Uuid,
    work: impl FnOnce(&FsPath, Duration) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    let permit = acquire_permit(state).await?;
    let storage = state.storage.clone();
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let path = storage.read_path(repo_id)?;
        work(&path, timeout)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage operation failed",
        )
    })?
    .map_err(|error| map_storage_error(repo_id, error))
}

/// Like [`git_read`], but also creates a scratch directory (removed when the
/// worker returns) for Git commands that must write objects off-repository.
async fn git_compare<T>(
    state: &AppState,
    repo_id: Uuid,
    work: impl FnOnce(&FsPath, &FsPath, Duration) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    let permit = acquire_permit(state).await?;
    let storage = state.storage.clone();
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let path = storage.read_path(repo_id)?;
        let scratch = storage.scratch_dir()?;
        work(&path, scratch.path(), timeout)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage operation failed",
        )
    })?
    .map_err(|error| map_storage_error(repo_id, error))
}

/// Split a `<base>...<head>` spec on the first `...`. Both sides must pass
/// `valid_rev`, which rejects `..`, so the split is unambiguous and a missing
/// or extra separator is `None`.
pub(crate) fn split_spec(spec: &str) -> Option<(&str, &str)> {
    let (base, head) = spec.split_once("...")?;
    (browse::valid_rev(base) && browse::valid_rev(head)).then_some((base, head))
}

fn map_storage_error(repo_id: Uuid, error: StorageError) -> ApiError {
    match error {
        StorageError::TooLarge { .. } => {
            ApiError(StatusCode::PAYLOAD_TOO_LARGE, "response too large")
        }
        StorageError::Timeout => {
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "git operation timed out")
        }
        _ => {
            // The error may embed Git's stderr (paths included); log only a category.
            tracing::error!(category = "git", repository_id = %repo_id, "repository read failed");
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage operation failed",
            )
        }
    }
}

async fn resolve(state: &AppState, repo_id: Uuid, rev: String) -> Result<String, ApiError> {
    git_read(state, repo_id, move |path, timeout| {
        browse::resolve(path, &rev, timeout)
    })
    .await?
    .ok_or_else(revision_not_found)
}

pub(crate) fn is_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Serialize)]
struct RefJson {
    name: String,
    target: String,
}

#[derive(Serialize)]
struct RefsJson {
    default_branch: Option<String>,
    branches: Vec<RefJson>,
    tags: Vec<RefJson>,
}

async fn refs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo)): Path<(String, String)>,
) -> Result<Json<RefsJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    let repo_id = loaded.repo.id;
    let result = git_read(&state, repo_id, browse::refs).await?;
    Ok(Json(RefsJson {
        default_branch: result.default_branch,
        branches: result
            .branches
            .into_iter()
            .map(|r| RefJson {
                name: r.name,
                target: r.target,
            })
            .collect(),
        tags: result
            .tags
            .into_iter()
            .map(|r| RefJson {
                name: r.name,
                target: r.target,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TreeQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct TreeEntryJson {
    name: String,
    kind: String,
    mode: String,
    id: String,
    size: Option<u64>,
}

#[derive(Serialize)]
struct TreeJson {
    id: String,
    entries: Vec<TreeEntryJson>,
}

async fn tree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, rev)): Path<(String, String, String)>,
    Query(query): Query<TreeQuery>,
) -> Result<Json<TreeJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    if !browse::valid_rev(&rev) {
        return Err(invalid());
    }
    if let Some(ref path) = query.path
        && !browse::valid_path(path)
    {
        return Err(invalid());
    }
    let repo_id = loaded.repo.id;
    let id = resolve(&state, repo_id, rev.clone()).await?;
    let path = query.path.clone();
    let id_for_git = id.clone();
    let entries = git_read(&state, repo_id, move |fs_path, timeout| {
        browse::tree(fs_path, &id_for_git, path.as_deref(), timeout)
    })
    .await?
    .ok_or_else(not_found)?;
    Ok(Json(TreeJson {
        id,
        entries: entries
            .into_iter()
            .map(|e| TreeEntryJson {
                kind: match e.kind {
                    browse::EntryKind::Blob => "blob".to_owned(),
                    browse::EntryKind::Tree => "tree".to_owned(),
                    browse::EntryKind::Commit => "commit".to_owned(),
                    browse::EntryKind::Symlink => "symlink".to_owned(),
                },
                name: e.name,
                mode: e.mode,
                id: e.id,
                size: e.size,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct BlobJson {
    id: String,
    size: u64,
    binary: bool,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base64: Option<String>,
}

async fn blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, rev)): Path<(String, String, String)>,
    Query(query): Query<BlobQuery>,
) -> Result<Json<BlobJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    if !browse::valid_rev(&rev) {
        return Err(invalid());
    }
    let path = query.path.ok_or_else(invalid)?;
    if !browse::valid_path(&path) {
        return Err(invalid());
    }
    let repo_id = loaded.repo.id;
    let id = resolve(&state, repo_id, rev.clone()).await?;
    let blob = git_read(&state, repo_id, move |fs_path, timeout| {
        browse::blob(fs_path, &id, &path, browse::MAX_BLOB_BYTES, timeout)
    })
    .await?
    .ok_or_else(not_found)?;

    let (text, base64) = if blob.binary {
        (None, Some(encode_base64(&blob.content)))
    } else {
        match String::from_utf8(blob.content.clone()) {
            Ok(text) => (Some(text), None),
            Err(_) => (None, Some(encode_base64(&blob.content))),
        }
    };
    Ok(Json(BlobJson {
        id: blob.id,
        size: blob.size,
        binary: blob.binary,
        truncated: blob.truncated,
        text,
        base64,
    }))
}

fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    path: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct AuthorJson {
    name: String,
    email: String,
    time: i64,
}

#[derive(Serialize)]
struct CommitSummaryJson {
    id: String,
    parents: Vec<String>,
    author: AuthorJson,
    committer: AuthorJson,
    summary: String,
}

#[derive(Serialize)]
struct LogJson {
    id: String,
    items: Vec<CommitSummaryJson>,
    next: Option<String>,
}

async fn log(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, rev)): Path<(String, String, String)>,
    Query(query): Query<LogQuery>,
) -> Result<Json<LogJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    if !browse::valid_rev(&rev) {
        return Err(invalid());
    }
    if let Some(ref path) = query.path
        && !browse::valid_path(path)
    {
        return Err(invalid());
    }
    let limit = query.limit.unwrap_or(30);
    if !(1..=browse::MAX_LOG_LIMIT as i64).contains(&limit) {
        return Err(invalid());
    }
    let after = match query.cursor.as_deref() {
        Some(cursor) => cursor.parse::<u64>().map_err(|_| invalid())?,
        None => 0,
    };

    let repo_id = loaded.repo.id;
    let id = resolve(&state, repo_id, rev.clone()).await?;
    let path = query.path.clone();
    let limit = limit as usize;
    let id_for_git = id.clone();
    let summaries = git_read(&state, repo_id, move |fs_path, timeout| {
        browse::log(
            fs_path,
            &id_for_git,
            path.as_deref(),
            Some(after),
            limit + 1,
            timeout,
        )
    })
    .await?;

    let has_more = summaries.len() > limit;
    let mut items = summaries;
    if has_more {
        items.truncate(limit);
    }
    let next = has_more.then(|| (after.saturating_add(limit as u64)).to_string());
    Ok(Json(LogJson {
        id,
        items: items
            .into_iter()
            .map(|c| CommitSummaryJson {
                author: AuthorJson {
                    name: c.author_name,
                    email: c.author_email,
                    time: c.author_time,
                },
                committer: AuthorJson {
                    name: c.committer_name,
                    email: c.committer_email,
                    time: c.committer_time,
                },
                id: c.id,
                parents: c.parents,
                summary: c.summary,
            })
            .collect(),
        next,
    }))
}

#[derive(Serialize)]
struct FileDiffJson {
    old_path: String,
    new_path: String,
    status: String,
    additions: u64,
    deletions: u64,
    binary: bool,
    patch: String,
}

#[derive(Serialize)]
struct CommitJson {
    id: String,
    parents: Vec<String>,
    author: AuthorJson,
    committer: AuthorJson,
    message: String,
    truncated: bool,
    files: Vec<FileDiffJson>,
}

async fn commit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, id)): Path<(String, String, String)>,
) -> Result<Json<CommitJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    if !is_commit_id(&id) {
        return Err(invalid());
    }
    let repo_id = loaded.repo.id;
    let detail = git_read(&state, repo_id, move |fs_path, timeout| {
        browse::commit(fs_path, &id, timeout)
    })
    .await?
    .ok_or_else(revision_not_found)?;
    Ok(Json(CommitJson {
        author: AuthorJson {
            name: detail.author_name,
            email: detail.author_email,
            time: detail.author_time,
        },
        committer: AuthorJson {
            name: detail.committer_name,
            email: detail.committer_email,
            time: detail.committer_time,
        },
        id: detail.id,
        parents: detail.parents,
        message: detail.message,
        truncated: detail.truncated,
        files: detail
            .files
            .into_iter()
            .map(|f| FileDiffJson {
                old_path: f.old_path,
                new_path: f.new_path,
                status: f.status,
                additions: f.additions,
                deletions: f.deletions,
                binary: f.binary,
                patch: f.patch,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
struct MergeableJson {
    status: &'static str,
    conflicts: Vec<String>,
}

#[derive(Serialize)]
struct CompareJson {
    base_id: String,
    head_id: String,
    merge_base: Option<String>,
    ahead_by: u64,
    behind_by: u64,
    commits_truncated: bool,
    mergeable: MergeableJson,
    commits: Vec<CommitSummaryJson>,
    truncated: bool,
    files: Vec<FileDiffJson>,
}

async fn compare(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org, repo, spec)): Path<(String, String, String)>,
) -> Result<Json<CompareJson>, ApiError> {
    let loaded = load_readable(&state, &headers, &org, &repo).await?;
    let Some((base, head)) = split_spec(&spec) else {
        return Err(invalid());
    };
    let (base, head) = (base.to_owned(), head.to_owned());
    let repo_id = loaded.repo.id;
    let comparison = git_compare(&state, repo_id, move |fs_path, scratch, timeout| {
        browse::compare(fs_path, scratch, &base, &head, timeout)
    })
    .await?
    .ok_or_else(revision_not_found)?;

    let (status, conflicts) = match comparison.mergeable {
        browse::Mergeability::Clean => ("clean", vec![]),
        browse::Mergeability::Conflicts(paths) => ("conflicts", paths),
        browse::Mergeability::Unrelated => ("unrelated", vec![]),
        browse::Mergeability::UpToDate => ("up_to_date", vec![]),
    };
    Ok(Json(CompareJson {
        base_id: comparison.base_id,
        head_id: comparison.head_id,
        merge_base: comparison.merge_base,
        ahead_by: comparison.ahead_by,
        behind_by: comparison.behind_by,
        commits_truncated: comparison.commits_truncated,
        mergeable: MergeableJson { status, conflicts },
        commits: comparison
            .commits
            .into_iter()
            .map(|c| CommitSummaryJson {
                author: AuthorJson {
                    name: c.author_name,
                    email: c.author_email,
                    time: c.author_time,
                },
                committer: AuthorJson {
                    name: c.committer_name,
                    email: c.committer_email,
                    time: c.committer_time,
                },
                id: c.id,
                parents: c.parents,
                summary: c.summary,
            })
            .collect(),
        truncated: comparison.truncated,
        files: comparison
            .files
            .into_iter()
            .map(|f| FileDiffJson {
                old_path: f.old_path,
                new_path: f.new_path,
                status: f.status,
                additions: f.additions,
                deletions: f.deletions,
                binary: f.binary,
                patch: f.patch,
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use githearth_authz::Engine;
    use githearth_git::layout::{ObjectFormat, Storage};
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[test]
    fn storage_errors_map_to_distinct_statuses() {
        let repo_id = Uuid::new_v4();
        assert_eq!(
            map_storage_error(repo_id, StorageError::TooLarge { size: 2, limit: 1 }).status_code(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            map_storage_error(repo_id, StorageError::Timeout).status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_storage_error(repo_id, StorageError::Git("boom".into())).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            map_storage_error(repo_id, StorageError::Invalid("boom".into())).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn split_spec_requires_exactly_one_valid_separator() {
        for (spec, expected) in [
            ("main...feature", Some(("main", "feature"))),
            ("v1.0...v2.0", Some(("v1.0", "v2.0"))),
            ("a...b...c", None),
            ("main..feature", None),
            ("main", None),
            ("...feature", None),
            ("main...", None),
            ("...", None),
            ("....", None),
            ("-foo...main", None),
            ("main...-bar", None),
            ("main...a..b", None),
        ] {
            assert_eq!(split_spec(spec), expected, "spec {spec:?}");
        }
    }

    #[tokio::test]
    async fn worker_panic_maps_to_500() {
        let root = std::env::temp_dir().join(format!("browse-panic-{}", Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        let repo_id = Uuid::new_v4();
        storage
            .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
            .unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        let state = AppState {
            pool,
            config: crate::config::Config {
                storage_root: root,
                database_url: "postgres://localhost/unused".into(),
                ..crate::config::Config::default()
            },
            auth_slots: Arc::new(Semaphore::new(2)),
            git_slots: Arc::new(Semaphore::new(2)),
            dummy_hash: Arc::new(String::new()),
            authz: Arc::new(Engine::new().unwrap()),
            storage: Arc::new(storage),
            hook_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            hook_socket_path: std::path::PathBuf::from("/nonexistent/hook.sock"),
            trusted_proxies: Vec::new(),
        };
        let error = git_read(&state, repo_id, |_, _| -> Result<Vec<u8>, StorageError> {
            panic!("worker panic")
        })
        .await
        .unwrap_err();
        assert_eq!(error.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        let _ = std::fs::remove_dir_all(&state.config.storage_root);
    }
}
