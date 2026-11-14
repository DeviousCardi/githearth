//! Server-rendered pull request, review and comment pages: list, view, create,
//! comment and review.
//!
//! Pages use the browser session cookie; every form carries the session's
//! CSRF token. All reads and writes go through the same functions as the JSON
//! API, so authorization, validation and audit events are identical.

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use githearth_authz::{Action, Resource};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AppState, auto_merge, browse_ui, facts, idempotency, merge_intents, pulls, repositories,
    repository_api, ui,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/repositories/{name}/pulls",
            get(list_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/new",
            get(new_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}",
            get(pull_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/comments",
            axum::routing::post(comment_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/reviews",
            axum::routing::post(review_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/comments/{comment_id}/resolve",
            axum::routing::post(resolve_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/comments/{comment_id}/unresolve",
            axum::routing::post(unresolve_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/comments/{comment_id}/apply",
            axum::routing::post(apply_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/merge",
            axum::routing::post(merge_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/merge/cancel",
            axum::routing::post(merge_cancel_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/auto_merge",
            axum::routing::post(auto_merge_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/auto_merge/cancel",
            axum::routing::post(auto_merge_cancel_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/ready_for_review",
            axum::routing::post(ready_for_review_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/pulls/{number}/convert_to_draft",
            axum::routing::post(convert_to_draft_submit),
        )
}

/// The merge form's method labels and the select the pull page renders.
const MERGE_METHODS: [(&str, &str); 4] = [
    ("merge", "Merge commit"),
    ("squash", "Squash and merge"),
    ("rebase", "Rebase and merge"),
    ("ff_only", "Fast-forward only"),
];

/// A merge intent state as a short, human-readable label.
fn intent_state_label(state: &str) -> &str {
    match state {
        "queued" => "Queued",
        "validating" => "Validating",
        "executing" => "Executing",
        "merged" => "Merged",
        "conflicted" => "Conflicted",
        "stale" => "Stale",
        "failed" => "Failed",
        "cancelled" => "Cancelled",
        _ => state,
    }
}

/// A mergeability status that means the selected method cannot run, with the
/// reason shown next to the disabled merge button. `clean` (and the un-checked
/// `ff_only` method) map to `None` — the form stays live.
fn merge_block_reason(status: &str) -> Option<&'static str> {
    match status {
        "conflicts" => Some("This branch has conflicts that must be resolved."),
        "unrelated" => Some("These branches have no common history."),
        "up_to_date" => Some("This branch is already up to date with the target."),
        "unmergeable" => Some("This branch cannot be rebased because it contains merge commits."),
        _ => None,
    }
}

/// The reason the selected merge method cannot run, if any. Computed from the
/// selected method only: a conflicting or unmergeable rebase must not disable
/// the form for a clean merge/squash, whose statuses agree with each other.
fn selected_block_reason(
    mergeability: &[crate::mergeability::Check],
    method: &str,
) -> Option<&'static str> {
    mergeability
        .iter()
        .find(|c| c.method == method)
        .and_then(|c| merge_block_reason(&c.status))
}

struct Session {
    account: crate::Account,
    auth_age: u64,
    csrf: String,
}

async fn session(state: &AppState, headers: &HeaderMap) -> Result<Session, ui::UiError> {
    let (account, auth_age) = match ui::browser_auth(state, headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(ui::auth_required()),
        Err(_) => return Err(ui::unavailable_page(None)),
    };
    let hash = ui::browser_token_hash(headers).unwrap_or_default();
    Ok(Session {
        account,
        auth_age,
        csrf: ui::csrf_expected(&hash),
    })
}

/// Checks the form's CSRF token before anything else touches the session.
async fn form_session(
    state: &AppState,
    headers: &HeaderMap,
    supplied: &str,
) -> Result<Session, ui::UiError> {
    let hash = ui::browser_token_hash(headers).ok_or_else(ui::auth_required)?;
    if !ui::csrf_matches(supplied, &ui::csrf_expected(&hash)) {
        return Err(ui::forbidden_page());
    }
    session(state, headers).await
}

fn page(s: &Session, status: StatusCode, title: &str, body: &str) -> Response {
    (
        status,
        Html(ui::render_page(
            title,
            Some(&s.account.username),
            Some(&s.csrf),
            body,
        )),
    )
        .into_response()
}

fn message_page(s: &Session, status: StatusCode, title: &str, message: &str) -> ui::UiError {
    ui::UiError::from(page(
        s,
        status,
        title,
        &format!("<h1>{}</h1><p>{}</p>", ui::esc(title), ui::esc(message)),
    ))
}

/// Maps an API error to the page a browser should see.
fn api_error(s: &Session, error: crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => message_page(
            s,
            StatusCode::FORBIDDEN,
            "Not permitted",
            "You do not have permission to do that.",
        ),
        StatusCode::CONFLICT => message_page(s, StatusCode::CONFLICT, "Conflict", error.message()),
        StatusCode::BAD_REQUEST => message_page(
            s,
            StatusCode::BAD_REQUEST,
            "Invalid request",
            "Check the form and try again.",
        ),
        StatusCode::UNPROCESSABLE_ENTITY => message_page(
            s,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Branch not found",
            error.message(),
        ),
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

fn repo_href(slug: &str, name: &str) -> String {
    format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(slug),
        ui::url_encode(name)
    )
}

/// The CSRF token plus a per-form idempotency key, so a double submit or a
/// resent POST replays the first result instead of creating a duplicate.
fn form_fields(s: &Session, key: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">",
        ui::esc(&s.csrf),
        ui::esc(key)
    )
}

fn new_key() -> String {
    Uuid::new_v4().simple().to_string()
}

fn checked_key<'a>(s: &Session, key: &'a str) -> Result<&'a str, ui::UiError> {
    if idempotency::valid_key(key) {
        Ok(key)
    } else {
        Err(message_page(
            s,
            StatusCode::BAD_REQUEST,
            "Invalid request",
            "Reload the page and submit the form again.",
        ))
    }
}

fn title_ok(title: &str) -> bool {
    !title.trim().is_empty() && title.chars().count() <= 256
}

fn body_ok(body: &str) -> bool {
    body.chars().count() <= 65536
}

fn comment_ok(body: &str) -> bool {
    !body.trim().is_empty() && body.chars().count() <= 65536
}

/// Usernames for the given account ids; unknown ids are simply absent.
async fn usernames(state: &AppState, ids: Vec<Uuid>) -> Result<HashMap<Uuid, String>, ()> {
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, username FROM accounts WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&state.pool)
            .await
            .map_err(|_| ())?;
    Ok(rows.into_iter().collect())
}

fn who(names: &HashMap<Uuid, String>, id: &Uuid) -> String {
    ui::esc(names.get(id).map(String::as_str).unwrap_or("unknown"))
}

#[derive(Deserialize)]
struct ListParams {
    state: Option<String>,
    after: Option<i32>,
}

async fn list_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Query(params): Query<ListParams>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let filter = params.state.unwrap_or_else(|| "open".to_owned());
    let pull_page = pulls::list_pulls(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        pulls::ListQuery {
            state: Some(filter.clone()),
            limit: None,
            after: params.after,
        },
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    let names = usernames(
        &state,
        pull_page.items.iter().map(|p| p.author_id).collect(),
    )
    .await
    .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = repo_href(&slug, &name);

    let tabs = ["open", "closed", "merged", "all"]
        .iter()
        .map(|f| {
            if *f == filter {
                format!("<strong aria-current=\"page\">{f}</strong>")
            } else {
                format!("<a href=\"{base}/pulls?state={f}\">{f}</a>")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let mut rows = String::new();
    for pull in &pull_page.items {
        let state = if pull.draft {
            format!("draft ({})", pull.state)
        } else {
            pull.state.clone()
        };
        rows.push_str(&format!(
            "<tr><td>#{n}</td><td><a href=\"{base}/pulls/{n}\">{}</a></td><td>{}</td><td>{} → {}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(&pull.title),
            ui::esc(&state),
            ui::esc(&pull.source_branch),
            ui::esc(&pull.target_branch),
            who(&names, &pull.author_id),
            ui::esc(&pull.created_at),
            n = pull.number,
        ));
    }
    let table = if rows.is_empty() {
        "<p class=\"muted\">No pull requests.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Pull requests, newest first</caption><thead><tr><th scope=\"col\">Number</th><th scope=\"col\">Title</th><th scope=\"col\">State</th><th scope=\"col\">Branches</th><th scope=\"col\">Author</th><th scope=\"col\">Opened</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    let mut pager = String::new();
    if params.after.is_some() {
        pager.push_str(&format!(
            "<a href=\"{base}/pulls?state={}\">First page</a> ",
            ui::url_encode(&filter)
        ));
    }
    if let Some(next) = pull_page.next {
        pager.push_str(&format!(
            "<a href=\"{base}/pulls?state={}&amp;after={next}\" rel=\"next\">Next page</a>",
            ui::url_encode(&filter)
        ));
    }
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / Pull requests</nav>\
<h1>Pull requests</h1><p><a href=\"{base}/pulls/new\">New pull request</a></p>\
<p aria-label=\"Filter by state\">{tabs}</p><div class=\"card\">{table}</div><p>{pager}</p>",
        ui::esc(&name)
    );
    Ok(page(&s, StatusCode::OK, "Pull requests", &body))
}

#[allow(clippy::too_many_arguments)]
fn new_pull_body(
    s: &Session,
    base: &str,
    name: &str,
    source_options: &str,
    source: &str,
    target: &str,
    title: &str,
    text: &str,
    draft: bool,
    key: &str,
    error: Option<&str>,
) -> String {
    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let checked = if draft { " checked" } else { "" };
    format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / <a href=\"{base}/pulls\">Pull requests</a> / New</nav>\
<h1>New pull request</h1>{error}<form class=\"card\" method=\"post\" action=\"{base}/pulls\">{}\
<label for=\"source_repository\">Source repository</label><select id=\"source_repository\" name=\"source_repository\">{source_options}</select>\
<label for=\"source_branch\">Source branch</label><input id=\"source_branch\" name=\"source_branch\" required value=\"{}\">\
<label for=\"target_branch\">Target branch</label><input id=\"target_branch\" name=\"target_branch\" required value=\"{}\">\
<label for=\"title\">Title</label><input id=\"title\" name=\"title\" required maxlength=\"256\" value=\"{}\">\
<label for=\"body\">Description</label><textarea id=\"body\" name=\"body\" rows=\"8\" style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<p><label><input type=\"checkbox\" name=\"draft\" value=\"on\"{checked}> Create as draft</label></p>\
<p><button type=\"submit\">Create pull request</button></p></form>",
        ui::esc(name),
        form_fields(s, key),
        ui::esc(source),
        ui::esc(target),
        ui::esc(title),
        ui::esc(text)
    )
}

/// Options for the source-repository selector: this repository plus every
/// active fork the viewer can read, so a fork contributor can open the pull
/// request from the same form.
async fn source_repository_options(
    state: &AppState,
    s: &Session,
    slug: &str,
    name: &str,
    selected: &str,
) -> Result<String, ui::UiError> {
    let loaded = repository_api::load_repo(state, s.account.id, s.auth_age, slug, name)
        .await
        .map_err(|e| api_error(s, e))?;
    let forks: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT o.slug, r.name, r.id FROM repositories r \
         JOIN organizations o ON o.id = r.organization_id \
         WHERE r.parent_repository_id = $1 AND r.status = 'active' \
         ORDER BY o.slug, r.name",
    )
    .bind(loaded.repo.id)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let mut options = format!(
        "<option value=\"\"{}>This repository ({})</option>",
        if selected.is_empty() { " selected" } else { "" },
        ui::esc(name)
    );
    for (org_slug, fork_name, fork_id) in forks {
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
        let fork = repositories::get(&mut conn, fork_id)
            .await
            .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
        let Some(fork) = fork else { continue };
        let Some(org_facts) = facts::load(&mut conn, s.account.id, fork.organization_id)
            .await
            .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?
        else {
            continue;
        };
        drop(conn);
        let read = state.authz.authorize(&org_facts.request(
            Action::RepositoryRead,
            Resource::Repository(&repositories::authz_repository(&fork)),
            Some(Duration::from_secs(s.auth_age)),
        ));
        if !read.is_allowed() {
            continue;
        }
        let spec = format!("{org_slug}/{fork_name}");
        let selected = if spec == selected { " selected" } else { "" };
        options.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            ui::esc(&spec),
            selected,
            ui::esc(&spec)
        ));
    }
    Ok(options)
}

async fn new_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    // Access probe only: the form is shown to exactly those who can list PRs.
    pulls::list_pulls(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        pulls::ListQuery {
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    let base = repo_href(&slug, &name);
    let options = source_repository_options(&state, &s, &slug, &name, "").await?;
    Ok(page(
        &s,
        StatusCode::OK,
        "New pull request",
        &new_pull_body(
            &s,
            &base,
            &name,
            &options,
            "",
            "main",
            "",
            "",
            false,
            &new_key(),
            None,
        ),
    ))
}

#[derive(Deserialize)]
struct PullForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    source_repository: String,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: Option<String>,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<PullForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let key = checked_key(&s, &form.idempotency_key)?;
    let base = repo_href(&slug, &name);
    let draft = form.draft.is_some();
    if !title_ok(&form.title) || !body_ok(&form.body) {
        let options =
            source_repository_options(&state, &s, &slug, &name, &form.source_repository).await?;
        return Ok(page(
            &s,
            StatusCode::BAD_REQUEST,
            "New pull request",
            &new_pull_body(
                &s,
                &base,
                &name,
                &options,
                &form.source_branch,
                &form.target_branch,
                &form.title,
                &form.body,
                draft,
                key,
                Some(
                    "A title of 1 to 256 characters is required; the description is limited to 65536 characters.",
                ),
            ),
        ));
    }
    let mut payload = serde_json::json!({
        "source_branch": &form.source_branch,
        "target_branch": &form.target_branch,
        "title": &form.title,
        "body": &form.body,
        "draft": draft,
    });
    let source_repository = form.source_repository.trim();
    if !source_repository.is_empty() {
        payload["source_repository"] = serde_json::Value::String(source_repository.to_owned());
    }
    let payload = payload.to_string();
    let (_, value) = match pulls::create_pull(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        payload.as_bytes(),
        Some(key),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            if let Some(message) = ui::idempotency_notice(&e) {
                let options =
                    source_repository_options(&state, &s, &slug, &name, &form.source_repository)
                        .await?;
                return Ok(page(
                    &s,
                    e.status_code(),
                    "New pull request",
                    &new_pull_body(
                        &s,
                        &base,
                        &name,
                        &options,
                        &form.source_branch,
                        &form.target_branch,
                        &form.title,
                        &form.body,
                        draft,
                        key,
                        Some(message),
                    ),
                ));
            }
            return Err(api_error(&s, e));
        }
    };
    let number = value["number"].as_i64().unwrap_or_default();
    Ok(Redirect::to(&format!("{base}/pulls/{number}")).into_response())
}

#[derive(Deserialize)]
struct PullParams {
    after: Option<i64>,
}

async fn pull_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Query(params): Query<PullParams>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let draft = Draft {
        text: "",
        key: &new_key(),
        error: None,
    };
    let review = ReviewDraft {
        verdict: "approved",
        body: "",
        key: &new_key(),
        error: None,
    };
    let merge = MergeDraft {
        method: "merge",
        key: &new_key(),
        error: None,
    };
    render_pull(
        &state,
        &s,
        &slug,
        &name,
        number,
        params.after,
        draft,
        review,
        merge,
        StatusCode::OK,
    )
    .await
}

/// A comment form's contents when the page is shown again after a rejected submit.
struct Draft<'a> {
    text: &'a str,
    key: &'a str,
    error: Option<&'a str>,
}

/// A review form's contents when the page is shown again after a rejected submit.
struct ReviewDraft<'a> {
    verdict: &'a str,
    body: &'a str,
    key: &'a str,
    error: Option<&'a str>,
}

/// A merge form's contents when the page is shown again after a rejected submit.
struct MergeDraft<'a> {
    method: &'a str,
    key: &'a str,
    error: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
async fn render_pull(
    state: &AppState,
    s: &Session,
    slug: &str,
    name: &str,
    number: i32,
    after: Option<i64>,
    draft: Draft<'_>,
    review: ReviewDraft<'_>,
    merge: MergeDraft<'_>,
    status: StatusCode,
) -> Result<Response, ui::UiError> {
    let detail = pulls::read_pull(state, s.account.id, s.auth_age, slug, name, number)
        .await
        .map_err(|e| api_error(s, e))?;
    let comments = pulls::list_pull_comments(
        state,
        s.account.id,
        s.auth_age,
        slug,
        name,
        number,
        pulls::SeqQuery { limit: None, after },
    )
    .await
    .map_err(|e| api_error(s, e))?;
    let reviews = pulls::list_pull_reviews(
        state,
        s.account.id,
        s.auth_age,
        slug,
        name,
        number,
        pulls::SeqQuery {
            limit: None,
            after: None,
        },
    )
    .await
    .map_err(|e| api_error(s, e))?;
    let mut ids: Vec<Uuid> = comments.items.iter().map(|c| c.author_id).collect();
    ids.extend(reviews.items.iter().map(|r| r.reviewer_id));
    ids.push(detail.pull.author_id);
    let names = usernames(state, ids)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = repo_href(slug, name);

    let head_id = detail.head_id.as_deref().unwrap_or("(deleted)");
    let base_id = detail.base_id.as_deref().unwrap_or("(deleted)");

    let state_label = if detail.pull.state == "merged" {
        "<span class=\"badge\">Merged</span>".to_owned()
    } else if detail.pull.draft {
        format!(
            "<span class=\"badge\">Draft</span> <span class=\"muted\">{}</span>",
            ui::esc(&detail.pull.state)
        )
    } else {
        ui::esc(&detail.pull.state)
    };

    let intended: Option<merge_intents::MergeIntentJson> = match state.pool.acquire().await {
        Ok(mut conn) => merge_intents::find_intent_json(&mut conn, detail.pull.id)
            .await
            .unwrap_or(None),
        Err(_) => None,
    };
    let non_terminal = intended
        .as_ref()
        .is_some_and(|i| matches!(i.state.as_str(), "queued" | "validating" | "executing"));
    let can_merge = merge_intents::may_merge(state, s.account.id, s.auth_age, slug, name, number)
        .await
        .unwrap_or(false);
    let abilities = pulls::viewer_abilities(state, s.account.id, s.auth_age, slug, name, number)
        .await
        .unwrap_or(pulls::ViewerAbilities {
            may_comment: false,
            may_resolve: false,
            may_apply: false,
        });

    let auto_merge_row: Option<auto_merge::AutoMergeJson> = match state.pool.acquire().await {
        Ok(mut conn) => auto_merge::find_auto_merge(&mut conn, detail.pull.id)
            .await
            .unwrap_or(None),
        Err(_) => None,
    };
    let can_auto_merge =
        auto_merge::may_enable(state, s.account.id, s.auth_age, slug, name, number)
            .await
            .unwrap_or(false);
    let can_close = pulls::may_close(state, s.account.id, s.auth_age, slug, name, number)
        .await
        .unwrap_or(false);
    // The author, or a viewer who may close (triage/write), may toggle draft
    // state; this mirrors `pulls::require_draft_permission`.
    let may_toggle_draft = detail.pull.author_id == s.account.id || can_close;
    let queue_position = if intended.as_ref().is_some_and(|i| {
        i.in_queue && matches!(i.state.as_str(), "queued" | "validating" | "executing")
    }) {
        match state.pool.acquire().await {
            Ok(mut conn) => merge_intents::queue_position(&mut conn, detail.pull.id)
                .await
                .unwrap_or(None),
            Err(_) => None,
        }
    } else {
        None
    };

    let mut conflict_paths: Vec<String> = Vec::new();
    for check in &detail.mergeability {
        if check.status == "conflicts" {
            for path in &check.conflicts {
                if !conflict_paths.iter().any(|p| p == path) {
                    conflict_paths.push(path.clone());
                }
            }
        }
    }
    let has_conflicts = !conflict_paths.is_empty();
    let conflict_section = if has_conflicts {
        let mut html = String::from(
            "<p class=\"error\" role=\"alert\">This branch has conflicts that must be resolved</p><ul>",
        );
        for path in &conflict_paths {
            html.push_str(&format!("<li>{}</li>", ui::esc(path)));
        }
        html.push_str("</ul>");
        html.push_str(&format!(
            "<h2>Resolve conflicts locally</h2>\
<pre><code>git fetch origin\ngit checkout {}\ngit merge origin/{}</code></pre>",
            ui::esc(&detail.pull.source_branch),
            ui::esc(&detail.pull.target_branch),
        ));
        html
    } else {
        String::new()
    };

    let mut review_list = String::new();
    for r in &reviews.items {
        let stale = if r.verdict == "approved"
            && detail.head_id.as_deref().is_some_and(|h| h != r.commit_id)
        {
            " · <span class=\"muted\">approved an older commit</span>"
        } else {
            ""
        };
        review_list.push_str(&format!(
            "<li><p class=\"muted\">{} {} at {}{}</p>{}</li>",
            who(&names, &r.reviewer_id),
            ui::esc(&r.verdict),
            ui::esc(&r.created_at),
            stale,
            if r.body.is_empty() {
                String::new()
            } else {
                format!(
                    "<div style=\"white-space:pre-wrap\">{}</div>",
                    ui::esc(&r.body)
                )
            },
        ));
    }
    let review_list = if review_list.is_empty() {
        "<p class=\"muted\">No reviews yet.</p>".to_owned()
    } else {
        format!("<ul>{review_list}</ul>")
    };

    // Group comments into threads: a root (no parent) with its direct replies.
    let mut roots: Vec<&pulls::CommentJson> = Vec::new();
    let mut replies: HashMap<Uuid, Vec<&pulls::CommentJson>> = HashMap::new();
    for c in &comments.items {
        match c.in_reply_to {
            Some(parent) => replies.entry(parent).or_default().push(c),
            None => roots.push(c),
        }
    }
    let mut comment_list = String::new();
    for c in &roots {
        comment_list.push_str(&render_comment(
            s,
            &names,
            &base,
            number,
            c,
            false,
            abilities.may_resolve,
            abilities.may_apply,
        ));
        if let Some(thread_replies) = replies.get(&c.id) {
            for reply in thread_replies {
                comment_list.push_str(&render_comment(
                    s,
                    &names,
                    &base,
                    number,
                    reply,
                    true,
                    abilities.may_resolve,
                    abilities.may_apply,
                ));
            }
        }
    }
    // A reply whose root is on an earlier comment page is orphaned here:
    // pagination is by seq and rendering iterates roots only, so without this
    // it would be silently dropped. Render it under a marker linking the root
    // on the page that holds it.
    let root_ids: HashSet<Uuid> = roots.iter().map(|c| c.id).collect();
    let orphan_roots: Vec<Uuid> = comments
        .items
        .iter()
        .filter_map(|c| c.in_reply_to)
        .filter(|parent| !root_ids.contains(parent))
        .collect();
    let orphan_seqs = pulls::comment_seqs(
        state,
        s.account.id,
        s.auth_age,
        slug,
        name,
        number,
        &orphan_roots,
    )
    .await
    .map_err(|e| api_error(s, e))?;
    for reply in &comments.items {
        let Some(parent) = reply.in_reply_to.filter(|p| !root_ids.contains(p)) else {
            continue;
        };
        // The list query filters `seq > after`, so `after = seq - 1` puts the
        // root first on its page; the anchor names the root element.
        let href = match orphan_seqs.get(&parent) {
            Some(&seq) if seq > 1 => {
                format!("{base}/pulls/{number}?after={}#comment-{parent}", seq - 1)
            }
            _ => format!("{base}/pulls/{number}#comment-{parent}"),
        };
        comment_list.push_str(&format!(
            "<li class=\"muted\">reply to <a href=\"{href}\">an earlier comment</a></li>"
        ));
        comment_list.push_str(&render_comment(
            s,
            &names,
            &base,
            number,
            reply,
            true,
            abilities.may_resolve,
            abilities.may_apply,
        ));
    }
    let comment_list = if comment_list.is_empty() {
        "<p class=\"muted\">No comments yet.</p>".to_owned()
    } else {
        format!("<ul>{comment_list}</ul>")
    };
    let error = draft
        .error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let review_error = review
        .error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let mut pager = String::new();
    if after.is_some() {
        pager.push_str(&format!(
            "<a href=\"{base}/pulls/{number}\">First comments</a> "
        ));
    }
    if let Some(next) = comments.next {
        pager.push_str(&format!(
            "<a href=\"{base}/pulls/{number}?after={next}\" rel=\"next\">More comments</a>"
        ));
    }
    let review_options = ["approved", "changes_requested", "commented"]
        .iter()
        .map(|v| {
            let sel = if *v == review.verdict {
                " selected"
            } else {
                ""
            };
            format!("<option value=\"{v}\"{sel}>{v}</option>")
        })
        .collect::<String>();
    let merge_error = merge
        .error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    // A merged pull request shows its badge instead; any other intent reports
    // its state, plus the failure reason when that is what ended it.
    let intent_section = match (&intended, detail.pull.state.as_str()) {
        (Some(_intent), "merged") => String::new(),
        (Some(intent), _) => {
            let mut html = format!(
                "<p class=\"muted\">Merge: {}</p>",
                ui::esc(intent_state_label(&intent.state))
            );
            if let Some(reason) = intent.failure_reason.as_deref() {
                html.push_str(&format!(
                    "<p class=\"error\" role=\"alert\">{}</p>",
                    ui::esc(reason)
                ));
            }
            html
        }
        (None, _) => String::new(),
    };
    // Methods whose mergeability says they cannot run are dropped from the
    // selector, and the selected method's own status disables the button with
    // its reason. `ff_only` has no mergeability entry, so it always stays live.
    let merge_options = MERGE_METHODS
        .iter()
        .map(|(value, label)| {
            let sel = if *value == merge.method {
                " selected"
            } else {
                ""
            };
            let dis = if detail
                .mergeability
                .iter()
                .find(|c| c.method == *value)
                .is_some_and(|c| merge_block_reason(&c.status).is_some())
            {
                " disabled"
            } else {
                ""
            };
            format!("<option value=\"{value}\"{sel}{dis}>{label}</option>")
        })
        .collect::<String>();
    let code_owners_section = if detail.required_code_owners.is_empty() {
        String::new()
    } else {
        let mut rows = String::new();
        for owner in &detail.required_code_owners {
            let status = if owner.approved {
                "approved"
            } else {
                "required"
            };
            rows.push_str(&format!(
                "<li>{} <span class=\"muted\">({status})</span></li>",
                ui::esc(&owner.name)
            ));
        }
        format!("<h2>Code owners</h2><div class=\"card\"><ul>{rows}</ul></div>")
    };
    let code_owner_errors_section = if detail.code_owner_errors.is_empty() {
        String::new()
    } else {
        let mut rows = String::new();
        for error in &detail.code_owner_errors {
            rows.push_str(&format!("<li>{}</li>", ui::esc(error)));
        }
        format!("<h2>Code owner errors</h2><div class=\"card\"><ul>{rows}</ul></div>")
    };
    let show_merge_form = detail.pull.state == "open" && !non_terminal && can_merge;
    let blocked_reason = selected_block_reason(&detail.mergeability, merge.method);
    let merge_form = if show_merge_form {
        if let Some(reason) = blocked_reason {
            format!(
                "<p><button type=\"submit\" disabled>Merge pull request</button> \
<span class=\"muted\">{reason}</span></p>"
            )
        } else {
            format!(
                "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/merge\">{}\
<label for=\"method\">Merge method</label><select id=\"method\" name=\"method\">{}</select>\
<input type=\"hidden\" name=\"expected_head_oid\" value=\"{}\">\
<p><button type=\"submit\">Merge pull request</button></p></form>",
                form_fields(s, merge.key),
                merge_options,
                ui::esc(head_id),
            )
        }
    } else {
        String::new()
    };
    let files_html = files_changed(state, s, slug, name, &detail).await;
    let inline_form = if detail.head_id.is_some() && abilities.may_comment {
        format!(
            "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/comments\">{}\
<label for=\"path\">File path</label><input id=\"path\" name=\"path\" required>\
<label for=\"line\">Line number</label><input id=\"line\" name=\"line\" type=\"number\" min=\"1\" required>\
<label for=\"side\">Side</label><select id=\"side\" name=\"side\"><option value=\"new\">New (head)</option><option value=\"old\">Old (base)</option></select>\
<label for=\"inline-comment\">Comment on this line</label><textarea id=\"inline-comment\" name=\"body\" rows=\"3\" required style=\"width:100%;box-sizing:border-box\"></textarea>\
<input type=\"hidden\" name=\"commit_id\" value=\"{}\">\
<p><button type=\"submit\">Add inline comment</button></p></form>",
            form_fields(s, &new_key()),
            ui::esc(head_id),
        )
    } else {
        String::new()
    };
    // The latest status per context at the current head, newest first.
    let mut status_rows = String::new();
    for st in &detail.statuses {
        let description = st
            .description
            .as_deref()
            .map(|d| format!(" — {}", ui::esc(d)))
            .unwrap_or_default();
        let target = st
            .target_url
            .as_deref()
            .filter(|u| crate::statuses::valid_target_url(u))
            .map(|u| format!(" <a href=\"{}\">details</a>", ui::esc(u)))
            .unwrap_or_default();
        status_rows.push_str(&format!(
            "<li><span class=\"badge\">{}</span> {} · {}{}{}</li>",
            ui::esc(&st.state),
            ui::esc(&st.context),
            ui::esc(&st.created_at),
            description,
            target,
        ));
    }
    let status_section = if status_rows.is_empty() {
        String::new()
    } else {
        format!("<h2>Checks</h2><ul>{status_rows}</ul>")
    };

    // Auto-merge: enabled state with a cancel button, or an enable form for a
    // viewer with merge permission. Queue position for a live queue entry.
    let queue_notice = match queue_position {
        Some(pos) => {
            // Cancelling a queued intent uses `PullRequestClose`; only viewers
            // who could submit it see the control.
            let cancel = if can_close {
                format!(
                    "<form method=\"post\" action=\"{base}/pulls/{number}/merge/cancel\">{}\
<p><button type=\"submit\">Cancel queued merge</button></p></form>",
                    form_fields(s, &new_key()),
                )
            } else {
                String::new()
            };
            format!("<p class=\"muted\">Merge queue position: {pos}</p>{cancel}")
        }
        None => String::new(),
    };
    let auto_merge_section = match (&auto_merge_row, detail.pull.state.as_str()) {
        (Some(am), "open") => {
            // The cancel submit checks `PullRequestClose`, so only closers see
            // the disable form; everyone else just sees that it is enabled.
            let disable = if can_close {
                format!(
                    "<form method=\"post\" action=\"{base}/pulls/{number}/auto_merge/cancel\">{}\
<p><button type=\"submit\">Disable auto-merge</button></p></form>",
                    form_fields(s, &new_key()),
                )
            } else {
                String::new()
            };
            format!(
                "<div class=\"card\"><p>Auto-merge enabled ({})</p>{disable}</div>",
                ui::esc(&am.method),
            )
        }
        (None, "open") if can_auto_merge && !non_terminal => format!(
            "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/auto_merge\">{}\
<label for=\"auto_method\">Auto-merge method</label><select id=\"auto_method\" name=\"method\">{}</select>\
<p><button type=\"submit\">Enable auto-merge</button></p></form>",
            form_fields(s, &new_key()),
            merge_options,
        ),
        _ => String::new(),
    };
    // Draft state controls, offered to the author or a viewer with triage/write
    // permission (mirroring `pulls::require_draft_permission`).
    let draft_section = if detail.pull.state == "open" && may_toggle_draft {
        if detail.pull.draft {
            format!(
                "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/ready_for_review\">{}\
<p><button type=\"submit\">Ready for review</button></p></form>",
                form_fields(s, &new_key()),
            )
        } else {
            format!(
                "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/convert_to_draft\">{}\
<p><button type=\"submit\">Convert to draft</button></p></form>",
                form_fields(s, &new_key()),
            )
        }
    } else {
        String::new()
    };
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / <a href=\"{base}/pulls\">Pull requests</a> / #{number}</nav>\
<h1>{} <span class=\"muted\">#{number}</span></h1>\
<p class=\"muted\">{} · {} → {} · by {} at {}</p>\
<div class=\"card\"><dl>\
<dt>Head</dt><dd>{}</dd>\
<dt>Base</dt><dd>{}</dd>\
<dt>Approvals</dt><dd>{}</dd>\
<dt>Changes requested</dt><dd>{}</dd>\
</dl></div>\
<div class=\"card\" style=\"white-space:pre-wrap\">{}</div>\
{code_owners_section}{code_owner_errors_section}{intent_section}\
{status_section}{queue_notice}\
{draft_section}\
{conflict_section}\
<h2>Reviews</h2>{review_list} \
<h2>Comments</h2>{comment_list}<p>{pager}</p>\
<h2>Files changed</h2><div class=\"card\">{files_html}</div>{inline_form}\
{error}{review_error}{merge_error}\
{merge_form}{auto_merge_section}\
<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/comments\">{}\
<label for=\"comment\">Add a comment</label><textarea id=\"comment\" name=\"body\" rows=\"5\" required style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<p><button type=\"submit\">Comment</button></p></form>\
<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/reviews\">{}\
<label for=\"verdict\">Verdict</label><select id=\"verdict\" name=\"verdict\">{}</select>\
<label for=\"review\">Review body</label><textarea id=\"review\" name=\"body\" rows=\"4\" style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<input type=\"hidden\" name=\"commit_id\" value=\"{}\">\
<p><button type=\"submit\">Submit review</button></p></form>",
        ui::esc(name),
        ui::esc(&detail.pull.title),
        state_label,
        ui::esc(&detail.pull.source_branch),
        ui::esc(&detail.pull.target_branch),
        who(&names, &detail.pull.author_id),
        ui::esc(&detail.pull.created_at),
        ui::esc(head_id),
        ui::esc(base_id),
        detail.approvals,
        detail.changes_requested,
        ui::esc(&detail.pull.body),
        form_fields(s, draft.key),
        ui::esc(draft.text),
        form_fields(s, review.key),
        review_options,
        ui::esc(review.body),
        ui::esc(head_id),
    );
    Ok(page(s, status, "Pull request", &body))
}

/// Renders one comment, with its anchor (path, line and side), an outdated
/// badge when the head moved, its suggestion block, and the resolve and apply
/// actions available to a viewer with the matching permission.
#[allow(clippy::too_many_arguments)]
fn render_comment(
    s: &Session,
    names: &HashMap<Uuid, String>,
    base: &str,
    number: i32,
    c: &pulls::CommentJson,
    is_reply: bool,
    may_resolve: bool,
    may_apply: bool,
) -> String {
    let mut anchor = String::new();
    if let (Some(path), Some(line)) = (&c.path, c.line) {
        let side = c.side.as_deref().unwrap_or("new");
        anchor.push_str(&format!(
            "<p class=\"muted\">on <code>{}</code> line {} ({})",
            ui::esc(path),
            line,
            ui::esc(side)
        ));
        if c.outdated {
            anchor.push_str(" · <span class=\"muted\">outdated</span>");
        }
        anchor.push_str("</p>");
    }
    let mut body = format!(
        "<div style=\"white-space:pre-wrap\">{}</div>",
        ui::esc(&c.body)
    );
    if let Some(suggestion) = &c.suggestion {
        body.push_str(&format!(
            "<pre class=\"card\">{}</pre>",
            ui::esc(suggestion)
        ));
    }
    let mut actions = String::new();
    if !is_reply && may_resolve {
        let resolve_action = if c.resolved {
            format!(
                "<form method=\"post\" action=\"{base}/pulls/{number}/comments/{id}/unresolve\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<button type=\"submit\">Unresolve</button></form>",
                id = c.id,
                csrf = ui::esc(&s.csrf)
            )
        } else {
            format!(
                "<form method=\"post\" action=\"{base}/pulls/{number}/comments/{id}/resolve\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<button type=\"submit\">Resolve</button></form>",
                id = c.id,
                csrf = ui::esc(&s.csrf)
            )
        };
        actions.push_str(&resolve_action);
    }
    if c.suggestion.is_some() && !c.outdated && may_apply {
        actions.push_str(&format!(
            "<form method=\"post\" action=\"{base}/pulls/{number}/comments/{id}/apply\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<button type=\"submit\">Apply suggestion</button></form>",
            id = c.id,
            csrf = ui::esc(&s.csrf)
        ));
    }
    let resolved_mark = if c.resolved && !is_reply {
        " <span class=\"muted\">(resolved)</span>"
    } else {
        ""
    };
    format!(
        "<li><article id=\"comment-{id}\" aria-label=\"Comment {seq}\"><p class=\"muted\">{} commented at {}{}</p>{anchor}{body}{actions}</article></li>",
        who(names, &c.author_id),
        ui::esc(&c.created_at),
        resolved_mark,
        seq = c.seq,
        id = c.id,
    )
}

/// The diff between the pull request's base and head, rendered exactly like the
/// compare page's "Files changed". Empty when either branch is missing or the
/// read fails.
async fn files_changed(
    state: &AppState,
    s: &Session,
    slug: &str,
    name: &str,
    detail: &pulls::PullDetail,
) -> String {
    let (Some(base), Some(head)) = (detail.base_id.as_deref(), detail.head_id.as_deref()) else {
        return "<p class=\"muted\">Files changed are unavailable while a branch is missing.</p>"
            .to_owned();
    };
    let Ok(loaded) = repository_api::load_repo(state, s.account.id, s.auth_age, slug, name).await
    else {
        return String::new();
    };
    let repo_id = loaded.repo.id;
    let base = base.to_owned();
    let head = head.to_owned();
    let Ok(permit) = state.git_slots.clone().try_acquire_owned() else {
        return String::new();
    };
    let storage = state.storage.clone();
    let timeout = Duration::from_secs(state.config.git_timeout_seconds);
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let path = storage.read_path(repo_id)?;
        let scratch = storage.scratch_dir()?;
        githearth_git::browse::compare(&path, scratch.path(), &base, &head, timeout)
    })
    .await;
    match result {
        Ok(Ok(Some(comparison))) => browse_ui::render_diff(&comparison.files, comparison.truncated),
        _ => String::new(),
    }
}

#[derive(Deserialize)]
struct ResolveForm {
    #[serde(default)]
    csrf_token: String,
}

async fn resolve_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number, comment_id)): Path<(String, String, String, Uuid)>,
    Form(form): Form<ResolveForm>,
) -> Result<Response, ui::UiError> {
    set_resolved(
        &state,
        &headers,
        &slug,
        &name,
        &number,
        comment_id,
        &form.csrf_token,
        true,
    )
    .await
}

async fn unresolve_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number, comment_id)): Path<(String, String, String, Uuid)>,
    Form(form): Form<ResolveForm>,
) -> Result<Response, ui::UiError> {
    set_resolved(
        &state,
        &headers,
        &slug,
        &name,
        &number,
        comment_id,
        &form.csrf_token,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn set_resolved(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    name: &str,
    number: &str,
    comment_id: Uuid,
    csrf_token: &str,
    resolved: bool,
) -> Result<Response, ui::UiError> {
    let s = form_session(state, headers, csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    if let Err(e) = pulls::set_thread_resolved(
        state,
        s.account.id,
        s.auth_age,
        slug,
        name,
        number,
        comment_id,
        resolved,
    )
    .await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(slug, name))).into_response())
}

async fn apply_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number, comment_id)): Path<(String, String, String, Uuid)>,
    Form(form): Form<ResolveForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    if let Err(e) = pulls::apply_pull_suggestion(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        comment_id,
    )
    .await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct CommentForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    commit_id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: Option<i32>,
    #[serde(default)]
    side: String,
}

async fn comment_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<CommentForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    if !comment_ok(&form.body) {
        let draft = Draft {
            text: &form.body,
            key,
            error: Some("A comment must be 1 to 65536 characters and not only spaces."),
        };
        let review = ReviewDraft {
            verdict: "approved",
            body: "",
            key: &new_key(),
            error: None,
        };
        let merge = MergeDraft {
            method: "merge",
            key: &new_key(),
            error: None,
        };
        return render_pull(
            &state,
            &s,
            &slug,
            &name,
            number,
            None,
            draft,
            review,
            merge,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let mut payload = serde_json::json!({ "body": &form.body });
    if !form.commit_id.is_empty() {
        payload["commit_id"] = serde_json::json!(&form.commit_id);
        payload["path"] = serde_json::json!(&form.path);
        payload["line"] = serde_json::json!(form.line);
        if !form.side.is_empty() {
            payload["side"] = serde_json::json!(&form.side);
        }
    }
    let payload = payload.to_string();
    if let Err(e) = pulls::create_pull_comment(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        payload.as_bytes(),
        Some(key),
    )
    .await
    {
        if let Some(message) = ui::idempotency_notice(&e) {
            let draft = Draft {
                text: &form.body,
                key,
                error: Some(message),
            };
            let review = ReviewDraft {
                verdict: "approved",
                body: "",
                key: &new_key(),
                error: None,
            };
            let merge = MergeDraft {
                method: "merge",
                key: &new_key(),
                error: None,
            };
            return render_pull(
                &state,
                &s,
                &slug,
                &name,
                number,
                None,
                draft,
                review,
                merge,
                e.status_code(),
            )
            .await;
        }
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct ReviewForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    commit_id: String,
}

async fn review_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<ReviewForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    let valid_verdict = match form.verdict.as_str() {
        "approved" | "changes_requested" => true,
        "commented" => !form.body.trim().is_empty(),
        _ => false,
    };
    if !valid_verdict || !body_ok(&form.body) {
        let draft = Draft {
            text: "",
            key: &new_key(),
            error: None,
        };
        let review = ReviewDraft {
            verdict: &form.verdict,
            body: &form.body,
            key,
            error: Some("A reviewer verdict is required; the body is limited to 65536 characters."),
        };
        let merge = MergeDraft {
            method: "merge",
            key: &new_key(),
            error: None,
        };
        return render_pull(
            &state,
            &s,
            &slug,
            &name,
            number,
            None,
            draft,
            review,
            merge,
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let payload = serde_json::json!({
        "verdict": &form.verdict,
        "commit_id": &form.commit_id,
        "body": &form.body,
    })
    .to_string();
    if let Err(e) = pulls::create_pull_review(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        payload.as_bytes(),
        Some(key),
    )
    .await
    {
        if let Some(message) = ui::idempotency_notice(&e) {
            let draft = Draft {
                text: "",
                key: &new_key(),
                error: None,
            };
            let review = ReviewDraft {
                verdict: &form.verdict,
                body: &form.body,
                key,
                error: Some(message),
            };
            let merge = MergeDraft {
                method: "merge",
                key: &new_key(),
                error: None,
            };
            return render_pull(
                &state,
                &s,
                &slug,
                &name,
                number,
                None,
                draft,
                review,
                merge,
                e.status_code(),
            )
            .await;
        }
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct MergeForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    expected_head_oid: String,
}

async fn merge_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<MergeForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    let payload = serde_json::json!({
        "method": &form.method,
        "expected_head_oid": &form.expected_head_oid,
    })
    .to_string();
    let input = match merge_intents::new_merge(payload.as_bytes()) {
        Ok(input) => input,
        Err(_) => {
            let keep = MergeDraft {
                method: &form.method,
                key,
                error: Some("Choose a merge method and reload the page if the branch has changed."),
            };
            return render_pull(
                &state,
                &s,
                &slug,
                &name,
                number,
                None,
                Draft {
                    text: "",
                    key: &new_key(),
                    error: None,
                },
                ReviewDraft {
                    verdict: "approved",
                    body: "",
                    key: &new_key(),
                    error: None,
                },
                keep,
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    };
    let result = merge_intents::create_intent(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        payload.as_bytes(),
        Some(key),
        &input,
    )
    .await;
    match result {
        Ok((_, _)) => {}
        Err(e) => {
            let keep = MergeDraft {
                method: &form.method,
                key,
                error: Some(merge_notice(&e)),
            };
            return render_pull(
                &state,
                &s,
                &slug,
                &name,
                number,
                None,
                Draft {
                    text: "",
                    key: &new_key(),
                    error: None,
                },
                ReviewDraft {
                    verdict: "approved",
                    body: "",
                    key: &new_key(),
                    error: None,
                },
                keep,
                e.status_code(),
            )
            .await;
        }
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

/// The notice a rejected merge submit shows, keeping the chosen method. A 409
/// or 403 from `create` becomes a readable message; an idempotency conflict has
/// its own friendly text.
fn merge_notice(error: &crate::ApiError) -> &'static str {
    if let Some(message) = ui::idempotency_notice(error) {
        return message;
    }
    match error.status_code() {
        StatusCode::NOT_FOUND => "This pull request no longer exists.",
        StatusCode::FORBIDDEN => "You may not merge this pull request.",
        StatusCode::CONFLICT => "This pull request cannot be merged right now.",
        StatusCode::UNPROCESSABLE_ENTITY => "One of the branches is missing.",
        _ => "The merge could not be created. Try again.",
    }
}

#[derive(Deserialize)]
struct MergeCancelForm {
    #[serde(default)]
    csrf_token: String,
}

async fn merge_cancel_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<MergeCancelForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    if let Err(e) =
        merge_intents::cancel_intent(&state, s.account.id, s.auth_age, &slug, &name, number).await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct AutoMergeForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    method: String,
}

async fn auto_merge_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<AutoMergeForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    let payload = serde_json::json!({ "method": &form.method }).to_string();
    if let Err(e) = auto_merge::enable_auto_merge(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        payload.as_bytes(),
        Some(key),
    )
    .await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct AutoMergeCancelForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn auto_merge_cancel_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<AutoMergeCancelForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    if let Err(e) = auto_merge::cancel_auto_merge(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        Some(key),
    )
    .await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(&slug, &name))).into_response())
}

#[derive(Deserialize)]
struct DraftForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn ready_for_review_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<DraftForm>,
) -> Result<Response, ui::UiError> {
    set_draft(&state, &headers, &slug, &name, &number, &form, false).await
}

async fn convert_to_draft_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Form(form): Form<DraftForm>,
) -> Result<Response, ui::UiError> {
    set_draft(&state, &headers, &slug, &name, &number, &form, true).await
}

async fn set_draft(
    state: &AppState,
    headers: &HeaderMap,
    slug: &str,
    name: &str,
    number: &str,
    form: &DraftForm,
    draft: bool,
) -> Result<Response, ui::UiError> {
    let s = form_session(state, headers, &form.csrf_token).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let key = checked_key(&s, &form.idempotency_key)?;
    if let Err(e) = pulls::set_draft(
        state,
        s.account.id,
        s.auth_age,
        slug,
        name,
        number,
        draft,
        Some(key),
    )
    .await
    {
        return Err(api_error(&s, e));
    }
    Ok(Redirect::to(&format!("{}/pulls/{number}", repo_href(slug, name))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_notice_reports_a_generic_conflict_and_forbidden() {
        // An unrelated conflict keeps its generic text.
        let other = crate::ApiError(StatusCode::CONFLICT, "an open merge intent already exists");
        assert_eq!(
            merge_notice(&other),
            "This pull request cannot be merged right now."
        );
        // A forbidden merge reports the permission message.
        let forbidden = crate::ApiError(StatusCode::FORBIDDEN, "not permitted");
        assert_eq!(
            merge_notice(&forbidden),
            "You may not merge this pull request."
        );
    }

    #[test]
    fn selected_block_reason_uses_only_the_selected_method() {
        let checks = vec![
            crate::mergeability::Check {
                method: "merge".into(),
                status: "clean".into(),
                conflicts: vec![],
            },
            crate::mergeability::Check {
                method: "squash".into(),
                status: "clean".into(),
                conflicts: vec![],
            },
            crate::mergeability::Check {
                method: "rebase".into(),
                status: "conflicts".into(),
                conflicts: vec!["a.txt".into()],
            },
        ];
        // A conflicting rebase must not block the clean merge and squash methods.
        assert_eq!(selected_block_reason(&checks, "merge"), None);
        assert_eq!(selected_block_reason(&checks, "squash"), None);
        assert_eq!(
            selected_block_reason(&checks, "rebase"),
            Some("This branch has conflicts that must be resolved.")
        );
        // A method with no mergeability entry is always live.
        assert_eq!(selected_block_reason(&checks, "ff_only"), None);
    }
}
