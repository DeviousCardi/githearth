//! Server-rendered, accessible repository browsing pages.
//!
//! Every interpolated value is escaped and every link is URL-encoded. Binary
//! and truncated files render a notice instead of their content, and diffs
//! rely on the `+`/`-` text markers, not color alone.

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use githearth_authz::Action;
use githearth_core::RepositoryStatus;
use githearth_git::{StorageError, browse};
use serde::Deserialize;
use std::path::Path as FsPath;
use std::time::Duration;
use uuid::Uuid;

use crate::{AppState, repository_api, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/repositories/{name}/tree/{rev}",
            get(tree_root),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/tree/{rev}/{*path}",
            get(tree_path),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/blob/{rev}/{*path}",
            get(blob_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/commits/{rev}",
            get(commits_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/commit/{id}",
            get(commit_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/compare/{spec}",
            get(compare_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/compare",
            get(compare_form),
        )
}

fn encode_segments(path: &str) -> String {
    path.split('/')
        .map(ui::url_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn load_error(error: crate::ApiError, username: Option<&str>) -> ui::UiError {
    if error.status_code() == StatusCode::NOT_FOUND {
        ui::not_found_page(username)
    } else {
        ui::unavailable_page(username)
    }
}

/// Loads a repository for a UI page. A signed-in caller gets the normal
/// authorization path; an anonymous caller may browse a public repository only,
/// which is what the Git transport and the repository landing page already do.
async fn load_repo_ui(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    name: &str,
) -> Result<(Option<String>, Uuid), ui::UiError> {
    match ui::browser_auth(state, headers).await {
        Ok((account, auth_age)) => {
            let loaded = repository_api::load_repo(state, account.id, auth_age, slug, name)
                .await
                .map_err(|error| load_error(error, Some(&account.username)))?;
            if repository_api::require(state, &loaded, Action::RepositoryRead).is_err() {
                return Err(ui::not_found_page(Some(&account.username)));
            }
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(ui::not_found_page(Some(&account.username)));
            }
            Ok((Some(account.username), loaded.repo.id))
        }
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
            match repository_api::load_public_anonymous(state, slug, name).await {
                Some(repo) => Ok((None, repo.id)),
                None => Err(ui::auth_required()),
            }
        }
        Err(_) => Err(ui::unavailable_page(None)),
    }
}

async fn git_work_ui<T>(
    state: &AppState,
    repo_id: Uuid,
    username: Option<&str>,
    work: impl FnOnce(&FsPath, Duration) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, ui::UiError>
where
    T: Send + 'static,
{
    let permit = state
        .git_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ui::unavailable_page(username))?;
    let storage = state.storage.clone();
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let path = storage.read_path(repo_id)?;
        work(&path, timeout)
    })
    .await
    .map_err(|_| ui::unavailable_page(username))?
    .map_err(|error| map_work_error(repo_id, username, error))
}

fn map_work_error(repo_id: Uuid, username: Option<&str>, error: StorageError) -> ui::UiError {
    match error {
        StorageError::TooLarge { .. } | StorageError::Timeout => ui::unavailable_page(username),
        _ => {
            // The error may embed Git's stderr (paths included); log only a category.
            tracing::error!(category = "git", repository_id = %repo_id, "repository read failed");
            ui::unavailable_page(username)
        }
    }
}

/// Like [`git_work_ui`], but also creates a scratch directory (removed when
/// the worker returns) for Git commands that must write objects off-repository.
async fn git_compare_ui<T>(
    state: &AppState,
    repo_id: Uuid,
    username: Option<&str>,
    work: impl FnOnce(&FsPath, &FsPath, Duration) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, ui::UiError>
where
    T: Send + 'static,
{
    let permit = state
        .git_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ui::unavailable_page(username))?;
    let storage = state.storage.clone();
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let path = storage.read_path(repo_id)?;
        let scratch = storage.scratch_dir()?;
        work(&path, scratch.path(), timeout)
    })
    .await
    .map_err(|_| ui::unavailable_page(username))?
    .map_err(|error| map_work_error(repo_id, username, error))
}

async fn resolve_ui(
    state: &AppState,
    repo_id: Uuid,
    username: Option<&str>,
    rev: &str,
) -> Result<String, ui::UiError> {
    let rev = rev.to_owned();
    git_work_ui(state, repo_id, username, move |path, timeout| {
        browse::resolve(path, &rev, timeout)
    })
    .await?
    .ok_or_else(|| ui::not_found_page(username))
}

async fn tree_root(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, rev)): Path<(String, String, String)>,
) -> Result<Response, ui::UiError> {
    render_tree(&state, &headers, &slug, &name, &rev, None).await
}

async fn tree_path(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, rev, path)): Path<(String, String, String, String)>,
) -> Result<Response, ui::UiError> {
    render_tree(&state, &headers, &slug, &name, &rev, Some(&path)).await
}

async fn render_tree(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    name: &str,
    rev: &str,
    path: Option<&str>,
) -> Result<Response, ui::UiError> {
    let (username, repo_id) = load_repo_ui(state, headers, slug, name).await?;
    let username = username.as_deref();
    let path = path.filter(|p| !p.is_empty());
    if !browse::valid_rev(rev) {
        return Err(ui::not_found_page(username));
    }
    if let Some(path) = path
        && !browse::valid_path(path)
    {
        return Err(ui::not_found_page(username));
    }

    let id = resolve_ui(state, repo_id, username, rev).await?;
    let path_owned = path.map(str::to_owned);
    let entries = git_work_ui(state, repo_id, username, move |fs_path, timeout| {
        browse::tree(fs_path, &id, path_owned.as_deref(), timeout)
    })
    .await?
    .ok_or_else(|| ui::not_found_page(username))?;

    let repo_href = format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(slug),
        ui::url_encode(name)
    );
    let mut crumbs = format!(
        "<a href=\"{repo_href}/tree/{}\">{}</a>",
        ui::url_encode(rev),
        ui::esc(name)
    );
    if let Some(path) = path {
        let mut acc = String::new();
        for segment in path.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(segment);
            crumbs.push_str(&format!(
                " / <a href=\"{repo_href}/tree/{}/{}\">{}</a>",
                ui::url_encode(rev),
                encode_segments(&acc),
                ui::esc(segment)
            ));
        }
    }
    let nav = format!("<nav aria-label=\"Breadcrumb\">{crumbs}</nav>");

    let mut rows = String::new();
    for entry in &entries {
        let kind = match entry.kind {
            browse::EntryKind::Blob => "blob",
            browse::EntryKind::Tree => "tree",
            browse::EntryKind::Commit => "commit",
            browse::EntryKind::Symlink => "symlink",
        };
        let entry_path = match path {
            Some(parent) => format!("{parent}/{}", entry.name),
            None => entry.name.clone(),
        };
        // Submodules (`commit` kind) have no content to browse: render their
        // name as plain text, unlinked.
        let link = match entry.kind {
            browse::EntryKind::Tree => Some(format!(
                "{repo_href}/tree/{}/{}",
                ui::url_encode(rev),
                encode_segments(&entry_path)
            )),
            browse::EntryKind::Commit => None,
            _ => Some(format!(
                "{repo_href}/blob/{}/{}",
                ui::url_encode(rev),
                encode_segments(&entry_path)
            )),
        };
        let name_cell = match link {
            Some(link) => format!("<a href=\"{link}\">{}</a>", ui::esc(&entry.name)),
            None => ui::esc(&entry.name),
        };
        rows.push_str(&format!(
            "<tr><td>{name_cell}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(kind),
            entry.size.map(|s| s.to_string()).unwrap_or_default()
        ));
    }

    let body = format!(
        "<h1>{}</h1>{nav}<div class=\"card\"><table><thead><tr>\
<th scope=\"col\">Name</th><th scope=\"col\">Type</th><th scope=\"col\">Size</th>\
</tr></thead><tbody>{rows}</tbody></table></div>",
        ui::esc(name)
    );
    let csrf = Some(ui::csrf_expected(
        &ui::browser_token_hash(headers).unwrap_or_default(),
    ));
    Ok(Html(ui::render_page("Tree", username, csrf.as_deref(), &body)).into_response())
}

async fn blob_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, rev, path)): Path<(String, String, String, String)>,
) -> Result<Response, ui::UiError> {
    let (username, repo_id) = load_repo_ui(&state, &headers, &slug, &name).await?;
    let username = username.as_deref();
    if !browse::valid_rev(&rev) || !browse::valid_path(&path) {
        return Err(ui::not_found_page(username));
    }

    let id = resolve_ui(&state, repo_id, username, &rev).await?;
    let path_for_git = path.clone();
    let blob = git_work_ui(&state, repo_id, username, move |fs_path, timeout| {
        browse::blob(fs_path, &id, &path_for_git, browse::MAX_BLOB_BYTES, timeout)
    })
    .await?
    .ok_or_else(|| ui::not_found_page(username))?;

    let content = if blob.binary || blob.truncated {
        format!(
            "<p class=\"muted\">Binary or truncated file ({} bytes); content is not shown.</p>",
            blob.size
        )
    } else {
        let text = String::from_utf8_lossy(&blob.content);
        format!("<pre>{}</pre>", ui::esc(&text))
    };
    let body = format!(
        "<h1>{}</h1><p class=\"muted\">{} bytes</p><div class=\"card\">{}</div>",
        ui::esc(&path),
        blob.size,
        content
    );
    let csrf = Some(ui::csrf_expected(
        &ui::browser_token_hash(&headers).unwrap_or_default(),
    ));
    Ok(Html(ui::render_page("File", username, csrf.as_deref(), &body)).into_response())
}

async fn commits_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, rev)): Path<(String, String, String)>,
) -> Result<Response, ui::UiError> {
    let (username, repo_id) = load_repo_ui(&state, &headers, &slug, &name).await?;
    let username = username.as_deref();
    if !browse::valid_rev(&rev) {
        return Err(ui::not_found_page(username));
    }

    let id = resolve_ui(&state, repo_id, username, &rev).await?;
    let id_for_git = id.clone();
    let commits = git_work_ui(&state, repo_id, username, move |fs_path, timeout| {
        browse::log(
            fs_path,
            &id_for_git,
            None,
            Some(0),
            browse::MAX_LOG_LIMIT,
            timeout,
        )
    })
    .await?;

    let repo_href = format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(&slug),
        ui::url_encode(&name)
    );
    let mut items = String::new();
    for commit in &commits {
        items.push_str(&format!(
            "<li><a href=\"{repo_href}/commit/{}\">{}</a> <span class=\"muted\">{}</span></li>",
            ui::url_encode(&commit.id),
            ui::esc(&commit.summary),
            ui::esc(&commit.author_name)
        ));
    }
    let body = format!(
        "<h1>Commits</h1><p class=\"muted\">{} @ {}</p><div class=\"card\"><ul>{}</ul></div>",
        ui::esc(&name),
        ui::esc(&rev),
        items
    );
    let csrf = Some(ui::csrf_expected(
        &ui::browser_token_hash(&headers).unwrap_or_default(),
    ));
    Ok(Html(ui::render_page("Commits", username, csrf.as_deref(), &body)).into_response())
}

/// Render a diff exactly as the commit page does: binary notices, escaped
/// patch text, and a truncation notice.
pub(crate) fn render_diff(files: &[browse::FileDiff], truncated: bool) -> String {
    let mut out = String::new();
    for file in files {
        if file.binary {
            out.push_str(&format!(
                "<p class=\"muted\">Binary file: {} ({} / {} additions/deletions)</p>",
                ui::esc(&file.new_path),
                file.additions,
                file.deletions
            ));
        } else {
            out.push_str(&format!("<pre>{}</pre>", ui::esc(&file.patch)));
        }
    }
    if truncated {
        out.push_str("<p class=\"muted\">Diff output truncated.</p>");
    }
    out
}

async fn commit_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, id)): Path<(String, String, String)>,
) -> Result<Response, ui::UiError> {
    let (username, repo_id) = load_repo_ui(&state, &headers, &slug, &name).await?;
    let username = username.as_deref();
    if !crate::browse::is_commit_id(&id) {
        return Err(ui::not_found_page(username));
    }

    let detail = git_work_ui(&state, repo_id, username, move |fs_path, timeout| {
        browse::commit(fs_path, &id, timeout)
    })
    .await?
    .ok_or_else(|| ui::not_found_page(username))?;

    let body = format!(
        "<h1>{}</h1><p class=\"muted\">{} &lt;{}&gt;</p>\
<pre>{}</pre><div class=\"card\">{}</div>",
        ui::esc(&detail.id),
        ui::esc(&detail.author_name),
        ui::esc(&detail.author_email),
        ui::esc(&detail.message),
        render_diff(&detail.files, detail.truncated)
    );
    let csrf = Some(ui::csrf_expected(
        &ui::browser_token_hash(&headers).unwrap_or_default(),
    ));
    Ok(Html(ui::render_page("Commit", username, csrf.as_deref(), &body)).into_response())
}

#[derive(Deserialize)]
struct CompareQuery {
    #[serde(default)]
    base: String,
    #[serde(default)]
    head: String,
}

/// Redirect a `GET /compare?base=..&head=..` form submission to the canonical
/// `compare/<base>...<head>` path.
async fn compare_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Query(query): Query<CompareQuery>,
) -> Result<Response, ui::UiError> {
    let (username, _repo_id) = load_repo_ui(&state, &headers, &slug, &name).await?;
    let username = username.as_deref();
    let spec = format!("{}...{}", query.base, query.head);
    let Some((base, head)) = crate::browse::split_spec(&spec) else {
        return Err(ui::not_found_page(username));
    };
    Ok(Redirect::to(&format!(
        "/organizations/{}/repositories/{}/compare/{}...{}",
        ui::url_encode(&slug),
        ui::url_encode(&name),
        ui::url_encode(base),
        ui::url_encode(head)
    ))
    .into_response())
}

async fn compare_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, spec)): Path<(String, String, String)>,
) -> Result<Response, ui::UiError> {
    let (username, repo_id) = load_repo_ui(&state, &headers, &slug, &name).await?;
    let username = username.as_deref();
    let Some((base, head)) = crate::browse::split_spec(&spec) else {
        return Err(ui::not_found_page(username));
    };
    let (base_owned, head_owned) = (base.to_owned(), head.to_owned());
    let comparison = git_compare_ui(
        &state,
        repo_id,
        username,
        move |fs_path, scratch, timeout| {
            browse::compare(fs_path, scratch, &base_owned, &head_owned, timeout)
        },
    )
    .await?
    .ok_or_else(|| ui::not_found_page(username))?;

    let repo_href = format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(&slug),
        ui::url_encode(&name)
    );
    let (status_text, conflicts) = match &comparison.mergeable {
        browse::Mergeability::Clean => ("Clean", Vec::new()),
        browse::Mergeability::Conflicts(paths) => ("Conflicts", paths.clone()),
        browse::Mergeability::Unrelated => ("Unrelated histories", Vec::new()),
        browse::Mergeability::UpToDate => ("Up to date", Vec::new()),
    };
    let conflict_html = if conflicts.is_empty() {
        String::new()
    } else {
        let items = conflicts
            .iter()
            .map(|p| format!("<li>{}</li>", ui::esc(p)))
            .collect::<String>();
        format!("<ul>{items}</ul>")
    };
    let mut commits = String::new();
    for commit in &comparison.commits {
        commits.push_str(&format!(
            "<tr><td><a href=\"{repo_href}/commit/{}\">{}</a></td><td>{}</td><td>{}</td></tr>",
            ui::url_encode(&commit.id),
            ui::esc(&commit.id),
            ui::esc(&commit.summary),
            ui::esc(&commit.author_name)
        ));
    }
    if comparison.commits_truncated {
        commits.push_str("<tr><td colspan=\"3\" class=\"muted\">Commit list truncated.</td></tr>");
    }
    let body = format!(
        "<h1>Compare {}...{}</h1>\
<p class=\"muted\">{} ahead, {} behind &middot; {}</p>\
{conflict_html}\
<form method=\"get\" action=\"{repo_href}/compare\">\
<label for=\"base\">Base</label><input id=\"base\" name=\"base\" value=\"{}\">\
<label for=\"head\">Head</label><input id=\"head\" name=\"head\" value=\"{}\">\
<button type=\"submit\">Compare</button></form>\
<div class=\"card\"><table><thead><tr>\
<th scope=\"col\">Commit</th><th scope=\"col\">Summary</th><th scope=\"col\">Author</th>\
</tr></thead><tbody>{commits}</tbody></table></div>\
<h2>Files changed</h2><div class=\"card\">{}</div>",
        ui::esc(base),
        ui::esc(head),
        comparison.ahead_by,
        comparison.behind_by,
        ui::esc(status_text),
        ui::esc(base),
        ui::esc(head),
        render_diff(&comparison.files, comparison.truncated)
    );
    let csrf = Some(ui::csrf_expected(
        &ui::browser_token_hash(&headers).unwrap_or_default(),
    ));
    Ok(Html(ui::render_page("Compare", username, csrf.as_deref(), &body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn work_errors_render_the_unavailable_page() {
        let repo_id = Uuid::new_v4();
        for error in [
            StorageError::TooLarge { size: 2, limit: 1 },
            StorageError::Timeout,
            StorageError::Git("boom".into()),
            StorageError::Invalid("boom".into()),
        ] {
            let response = map_work_error(repo_id, Some("alice"), error).into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[test]
    fn load_error_distinguishes_not_found_from_unavailable() {
        let not_found = load_error(
            crate::ApiError(StatusCode::NOT_FOUND, "not found"),
            Some("alice"),
        )
        .into_response();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let unavailable = load_error(
            crate::ApiError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable"),
            Some("alice"),
        )
        .into_response();
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
