//! Server-rendered issue and comment pages: list, view and create.
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

use crate::{AppState, issues, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/repositories/{name}/issues",
            get(list_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/issues/new",
            get(new_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/issues/{number}",
            get(issue_page),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/issues/{number}/comments",
            axum::routing::post(comment_submit),
        )
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
        StatusCode::CONFLICT => message_page(
            s,
            StatusCode::CONFLICT,
            "Repository not active",
            "This repository is not accepting changes.",
        ),
        StatusCode::BAD_REQUEST => message_page(
            s,
            StatusCode::BAD_REQUEST,
            "Invalid request",
            "Check the form and try again.",
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

fn csrf_field(s: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">",
        ui::esc(&s.csrf)
    )
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
    let issue_page = issues::list_issues(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        issues::ListQuery {
            state: Some(filter.clone()),
            limit: None,
            after: params.after,
        },
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    let names = usernames(
        &state,
        issue_page.items.iter().map(|i| i.author_id).collect(),
    )
    .await
    .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = repo_href(&slug, &name);

    let tabs = ["open", "closed", "all"]
        .iter()
        .map(|f| {
            if *f == filter {
                format!("<strong aria-current=\"page\">{f}</strong>")
            } else {
                format!("<a href=\"{base}/issues?state={f}\">{f}</a>")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let mut rows = String::new();
    for issue in &issue_page.items {
        rows.push_str(&format!(
            "<tr><td>#{n}</td><td><a href=\"{base}/issues/{n}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(&issue.title),
            ui::esc(&issue.state),
            who(&names, &issue.author_id),
            ui::esc(&issue.created_at),
            n = issue.number,
        ));
    }
    let table = if rows.is_empty() {
        "<p class=\"muted\">No issues.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Issues, newest first</caption><thead><tr><th scope=\"col\">Number</th><th scope=\"col\">Title</th><th scope=\"col\">State</th><th scope=\"col\">Author</th><th scope=\"col\">Opened</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    let mut pager = String::new();
    if params.after.is_some() {
        pager.push_str(&format!(
            "<a href=\"{base}/issues?state={}\">First page</a> ",
            ui::url_encode(&filter)
        ));
    }
    if let Some(next) = issue_page.next {
        pager.push_str(&format!(
            "<a href=\"{base}/issues?state={}&amp;after={next}\" rel=\"next\">Next page</a>",
            ui::url_encode(&filter)
        ));
    }
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / Issues</nav>\
<h1>Issues</h1><p><a href=\"{base}/issues/new\">New issue</a></p>\
<p aria-label=\"Filter by state\">{tabs}</p><div class=\"card\">{table}</div><p>{pager}</p>",
        ui::esc(&name)
    );
    Ok(page(&s, StatusCode::OK, "Issues", &body))
}

fn new_issue_body(
    s: &Session,
    base: &str,
    name: &str,
    title: &str,
    text: &str,
    error: Option<&str>,
) -> String {
    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / <a href=\"{base}/issues\">Issues</a> / New</nav>\
<h1>New issue</h1>{error}<form class=\"card\" method=\"post\" action=\"{base}/issues\">{}\
<label for=\"title\">Title</label><input id=\"title\" name=\"title\" required maxlength=\"256\" value=\"{}\">\
<label for=\"body\">Description</label><textarea id=\"body\" name=\"body\" rows=\"8\" style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<p><button type=\"submit\">Create issue</button></p></form>",
        ui::esc(name),
        csrf_field(s),
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
    // Same visibility as the list: a repository the viewer cannot read is 404.
    issues::list_issues(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        issues::ListQuery {
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
        "New issue",
        &new_issue_body(&s, &base, &name, "", "", None),
    ))
}

#[derive(Deserialize)]
struct IssueForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<IssueForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let base = repo_href(&slug, &name);
    let title = form.title.trim();
    if !(1..=256).contains(&title.chars().count()) || form.body.chars().count() > 65536 {
        return Ok(page(
            &s,
            StatusCode::BAD_REQUEST,
            "New issue",
            &new_issue_body(
                &s,
                &base,
                &name,
                &form.title,
                &form.body,
                Some(
                    "A title of 1 to 256 characters is required; the description is limited to 65536 characters.",
                ),
            ),
        ));
    }
    let payload = serde_json::json!({ "title": title, "body": form.body }).to_string();
    let (_, value) = issues::create_issue(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        payload.as_bytes(),
        None,
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    let number = value["number"].as_i64().unwrap_or_default();
    Ok(Redirect::to(&format!("{base}/issues/{number}")).into_response())
}

#[derive(Deserialize)]
struct IssueParams {
    after: Option<i32>,
}

async fn issue_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, number)): Path<(String, String, String)>,
    Query(params): Query<IssueParams>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let Ok(number) = number.parse::<i32>() else {
        return Err(ui::not_found_page(Some(&s.account.username)));
    };
    let issue = issues::read_issue(&state, s.account.id, s.auth_age, &slug, &name, number)
        .await
        .map_err(|e| api_error(&s, e))?;
    let comments = issues::list_issue_comments(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        issues::CommentListQuery {
            limit: None,
            after: params.after,
        },
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    let mut ids: Vec<Uuid> = comments.items.iter().map(|c| c.author_id).collect();
    ids.push(issue.author_id);
    let names = usernames(&state, ids)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = repo_href(&slug, &name);

    let mut list = String::new();
    for c in &comments.items {
        list.push_str(&format!(
            "<li><article aria-label=\"Comment {seq}\"><p class=\"muted\">{} commented at {}</p><div style=\"white-space:pre-wrap\">{}</div></article></li>",
            who(&names, &c.author_id),
            ui::esc(&c.created_at),
            ui::esc(&c.body),
            seq = c.seq,
        ));
    }
    let list = if list.is_empty() {
        "<p class=\"muted\">No comments yet.</p>".to_owned()
    } else {
        format!("<ul>{list}</ul>")
    };
    let mut pager = String::new();
    if params.after.is_some() {
        pager.push_str(&format!(
            "<a href=\"{base}/issues/{number}\">First comments</a> "
        ));
    }
    if let Some(next) = comments.next {
        pager.push_str(&format!(
            "<a href=\"{base}/issues/{number}?after={next}\" rel=\"next\">More comments</a>"
        ));
    }
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"{base}\">{}</a> / <a href=\"{base}/issues\">Issues</a> / #{number}</nav>\
<h1>{} <span class=\"muted\">#{number}</span></h1>\
<p class=\"muted\">{} · opened by {} at {}</p>\
<div class=\"card\" style=\"white-space:pre-wrap\">{}</div>\
<h2>Comments</h2>{list}<p>{pager}</p>\
<form class=\"card\" method=\"post\" action=\"{base}/issues/{number}/comments\">{}\
<label for=\"comment\">Add a comment</label><textarea id=\"comment\" name=\"body\" rows=\"5\" required style=\"width:100%;box-sizing:border-box\"></textarea>\
<p><button type=\"submit\">Comment</button></p></form>",
        ui::esc(&name),
        ui::esc(&issue.title),
        ui::esc(&issue.state),
        who(&names, &issue.author_id),
        ui::esc(&issue.created_at),
        ui::esc(&issue.body),
        csrf_field(&s),
    );
    Ok(page(&s, StatusCode::OK, "Issue", &body))
}

#[derive(Deserialize)]
struct CommentForm {
    #[serde(default)]
    csrf_token: String,
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
    if !(1..=65536).contains(&form.body.trim().chars().count()) {
        return Err(message_page(
            &s,
            StatusCode::BAD_REQUEST,
            "Invalid comment",
            "A comment must be 1 to 65536 characters.",
        ));
    }
    let payload = serde_json::json!({ "body": form.body }).to_string();
    issues::create_issue_comment(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        number,
        payload.as_bytes(),
        None,
    )
    .await
    .map_err(|e| api_error(&s, e))?;
    Ok(Redirect::to(&format!("{}/issues/{number}", repo_href(&slug, &name))).into_response())
}
