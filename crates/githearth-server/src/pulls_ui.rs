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
use serde::Deserialize;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{AppState, idempotency, merge_intents, pulls, ui};

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
            "/organizations/{slug}/repositories/{name}/pulls/{number}/merge",
            axum::routing::post(merge_submit),
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
        rows.push_str(&format!(
            "<tr><td>#{n}</td><td><a href=\"{base}/pulls/{n}\">{}</a></td><td>{}</td><td>{} → {}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(&pull.title),
            ui::esc(&pull.state),
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
    source: &str,
    target: &str,
    title: &str,
    text: &str,
    key: &str,
    error: Option<&str>,
) -> String {
    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / <a href=\"{base}/pulls\">Pull requests</a> / New</nav>\
<h1>New pull request</h1>{error}<form class=\"card\" method=\"post\" action=\"{base}/pulls\">{}\
<label for=\"source_branch\">Source branch</label><input id=\"source_branch\" name=\"source_branch\" required value=\"{}\">\
<label for=\"target_branch\">Target branch</label><input id=\"target_branch\" name=\"target_branch\" required value=\"{}\">\
<label for=\"title\">Title</label><input id=\"title\" name=\"title\" required maxlength=\"256\" value=\"{}\">\
<label for=\"body\">Description</label><textarea id=\"body\" name=\"body\" rows=\"8\" style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<p><button type=\"submit\">Create pull request</button></p></form>",
        ui::esc(name),
        form_fields(s, key),
        ui::esc(source),
        ui::esc(target),
        ui::esc(title),
        ui::esc(text)
    )
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
    Ok(page(
        &s,
        StatusCode::OK,
        "New pull request",
        &new_pull_body(&s, &base, &name, "", "main", "", "", &new_key(), None),
    ))
}

#[derive(Deserialize)]
struct PullForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
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
    if !title_ok(&form.title) || !body_ok(&form.body) {
        return Ok(page(
            &s,
            StatusCode::BAD_REQUEST,
            "New pull request",
            &new_pull_body(
                &s,
                &base,
                &name,
                &form.source_branch,
                &form.target_branch,
                &form.title,
                &form.body,
                key,
                Some(
                    "A title of 1 to 256 characters is required; the description is limited to 65536 characters.",
                ),
            ),
        ));
    }
    let payload = serde_json::json!({
        "source_branch": &form.source_branch,
        "target_branch": &form.target_branch,
        "title": &form.title,
        "body": &form.body,
    })
    .to_string();
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
                return Ok(page(
                    &s,
                    e.status_code(),
                    "New pull request",
                    &new_pull_body(
                        &s,
                        &base,
                        &name,
                        &form.source_branch,
                        &form.target_branch,
                        &form.title,
                        &form.body,
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

    let mut review_list = String::new();
    for r in &reviews.items {
        review_list.push_str(&format!(
            "<li><p class=\"muted\">{} {} at {}</p>{}</li>",
            who(&names, &r.reviewer_id),
            ui::esc(&r.verdict),
            ui::esc(&r.created_at),
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

    let mut comment_list = String::new();
    for c in &comments.items {
        comment_list.push_str(&format!(
            "<li><article aria-label=\"Comment {seq}\"><p class=\"muted\">{} commented at {}</p><div style=\"white-space:pre-wrap\">{}</div></article></li>",
            who(&names, &c.author_id),
            ui::esc(&c.created_at),
            ui::esc(&c.body),
            seq = c.seq,
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
    let merge_options = MERGE_METHODS
        .iter()
        .map(|(value, label)| {
            let sel = if *value == merge.method {
                " selected"
            } else {
                ""
            };
            format!("<option value=\"{value}\"{sel}>{label}</option>")
        })
        .collect::<String>();
    let show_merge_form = detail.pull.state == "open" && !non_terminal && can_merge;
    let merge_form = if show_merge_form {
        format!(
            "<form class=\"card\" method=\"post\" action=\"{base}/pulls/{number}/merge\">{}\
<label for=\"method\">Merge method</label><select id=\"method\" name=\"method\">{}</select>\
<input type=\"hidden\" name=\"expected_head_oid\" value=\"{}\">\
<p><button type=\"submit\">Merge pull request</button></p></form>",
            form_fields(s, merge.key),
            merge_options,
            ui::esc(head_id),
        )
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
{intent_section}\
<h2>Reviews</h2>{review_list} \
<h2>Comments</h2>{comment_list}<p>{pager}</p>{error}{review_error}{merge_error}\
{merge_form}\
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

#[derive(Deserialize)]
struct CommentForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    body: String,
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
    let payload = serde_json::json!({ "body": &form.body }).to_string();
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
