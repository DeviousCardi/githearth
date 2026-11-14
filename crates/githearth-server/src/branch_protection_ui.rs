//! Server-rendered branch-protection settings page: list, create and delete
//! rules.
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and the create form a per-form idempotency key. Reads and writes go
//! through the same `pub(crate)` functions as the JSON API, so authorization,
//! validation and audit events are identical to tag protection.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, idempotency, protection, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/repositories/{name}/settings/branches",
            get(settings_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/settings/branches/{id}/delete",
            axum::routing::post(delete_submit),
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

fn new_key() -> String {
    Uuid::new_v4().simple().to_string()
}

fn repo_href(slug: &str, name: &str) -> String {
    format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(slug),
        ui::url_encode(name)
    )
}

/// Maps a branch-protection failure to the page a browser should see.
fn api_error(s: &Session, slug: &str, name: &str, error: crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => {
            let body = format!(
                "<h1>Not permitted</h1><p>You do not have permission to manage branch protection.</p><p><a href=\"{}\">Back to the repository</a></p>",
                ui::esc(&repo_href(slug, name))
            );
            ui::UiError::from(page(s, StatusCode::FORBIDDEN, "Not permitted", &body))
        }
        StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
            let title = "Cannot apply that change";
            let body = format!(
                "<h1>{title}</h1><p>{}</p><p><a href=\"{}\">Back to branch protection</a></p>",
                ui::esc(error.message()),
                ui::esc(&format!("{}/settings/branches", repo_href(slug, name)))
            );
            ui::UiError::from(page(s, error.status_code(), title, &body))
        }
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

struct RuleDraft<'a> {
    pattern: &'a str,
    allow_force_push: bool,
    allow_deletion: bool,
    require_pull_request: bool,
    required_approvals: &'a str,
    required_status_checks: &'a str,
    dismiss_stale_approvals: bool,
    require_signed_commits: bool,
    require_merge_queue: bool,
    require_code_owner_review: bool,
}

impl Default for RuleDraft<'_> {
    fn default() -> Self {
        Self {
            pattern: "",
            allow_force_push: false,
            allow_deletion: false,
            require_pull_request: false,
            required_approvals: "0",
            required_status_checks: "",
            dismiss_stale_approvals: true,
            require_signed_commits: false,
            require_merge_queue: false,
            require_code_owner_review: false,
        }
    }
}

fn checked(value: bool) -> &'static str {
    if value { " checked" } else { "" }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// Status check contexts are entered one per line or comma-separated. The
/// shared API still validates duplicates, reserved contexts and whitespace.
fn parse_status_checks(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|check| !check.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn settings_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    render_settings(
        &state,
        &s,
        &slug,
        &name,
        StatusCode::OK,
        None,
        &RuleDraft::default(),
    )
    .await
}

/// Renders the settings page, optionally with a rejected submission shown and
/// its values preserved, so a validation error never discards the operator's
/// input or turns into a generic 503.
async fn render_settings(
    state: &AppState,
    s: &Session,
    slug: &str,
    name: &str,
    status: StatusCode,
    error: Option<&str>,
    draft: &RuleDraft<'_>,
) -> Result<Response, ui::UiError> {
    let (rules, _, can_manage) =
        match protection::list_rules(state, s.account.id, s.auth_age, slug, name).await {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.status_code(),
                    StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
                ) =>
            {
                return Err(ui::not_found_page(Some(&s.account.username)));
            }
            Err(_) => return Err(ui::unavailable_page(Some(&s.account.username))),
        };

    let mut rows = String::new();
    for rule in &rules {
        let delete = if can_manage {
            format!(
                "<td><form method=\"post\" action=\"{}/settings/branches/{}/delete\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\" aria-label=\"Delete rule {}\">Delete</button></form></td>",
                ui::esc(&repo_href(slug, name)),
                rule.id,
                ui::esc(&s.csrf),
                ui::esc(&rule.pattern),
            )
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>{delete}</tr>",
            ui::esc(&rule.pattern),
            yes_no(rule.allow_force_push),
            yes_no(rule.allow_deletion),
            yes_no(rule.require_pull_request),
            rule.required_approvals,
            ui::esc(&rule.required_status_checks.join(", ")),
            yes_no(rule.dismiss_stale_approvals),
            yes_no(rule.require_signed_commits),
            yes_no(rule.require_merge_queue),
            yes_no(rule.require_code_owner_review),
        ));
    }
    let action_header = if can_manage {
        "<th scope=\"col\"></th>"
    } else {
        ""
    };
    let table = if rules.is_empty() {
        "<p class=\"muted\">No branch protection rules yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Branch protection rules</caption><thead><tr>\
<th scope=\"col\">Pattern</th><th scope=\"col\">Force push</th><th scope=\"col\">Deletion</th>\
<th scope=\"col\">Require PR</th><th scope=\"col\">Approvals</th>\
<th scope=\"col\">Status checks</th><th scope=\"col\">Dismiss stale</th>\
<th scope=\"col\">Signed</th>\
<th scope=\"col\">Merge queue</th><th scope=\"col\">Code owners</th>\
{action_header}</tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let add_form = if can_manage {
        format!(
            "<div class=\"card\"><h2>Add a rule</h2>{error}\
<form method=\"post\" action=\"{}/settings/branches\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<label for=\"pattern\">Pattern</label>\
<input id=\"pattern\" name=\"pattern\" required value=\"{}\">\
<label for=\"required_approvals\">Required approvals (0-10)</label>\
<input id=\"required_approvals\" name=\"required_approvals\" type=\"number\" min=\"0\" max=\"10\" value=\"{}\">\
<label for=\"required_status_checks\">Required status checks (comma or line separated)</label>\
<textarea id=\"required_status_checks\" name=\"required_status_checks\" rows=\"2\" style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<label><input type=\"checkbox\" name=\"allow_force_push\" value=\"1\"{}> Allow force push</label>\
<label><input type=\"checkbox\" name=\"allow_deletion\" value=\"1\"{}> Allow deletion</label>\
<label><input type=\"checkbox\" name=\"require_pull_request\" value=\"1\"{}> Require a pull request</label>\
<label><input type=\"checkbox\" name=\"dismiss_stale_approvals\" value=\"1\"{}> Dismiss stale approvals</label>\
<label><input type=\"checkbox\" name=\"require_signed_commits\" value=\"1\"{}> Require signed commits</label>\
<label><input type=\"checkbox\" name=\"require_merge_queue\" value=\"1\"{}> Require a merge queue</label>\
<label><input type=\"checkbox\" name=\"require_code_owner_review\" value=\"1\"{}> Require code owner review</label>\
<p><button type=\"submit\">Add rule</button></p></form></div>",
            ui::esc(&repo_href(slug, name)),
            ui::esc(&s.csrf),
            ui::esc(&new_key()),
            ui::esc(draft.pattern),
            ui::esc(draft.required_approvals),
            ui::esc(draft.required_status_checks),
            checked(draft.allow_force_push),
            checked(draft.allow_deletion),
            checked(draft.require_pull_request),
            checked(draft.dismiss_stale_approvals),
            checked(draft.require_signed_commits),
            checked(draft.require_merge_queue),
            checked(draft.require_code_owner_review),
        )
    } else {
        String::new()
    };

    let body = format!(
        "<h1>Branch protection</h1><p><a href=\"{}\">Back to the repository</a></p>\
<div class=\"card\">{table}</div>{add_form}",
        ui::esc(&repo_href(slug, name)),
    );
    Ok(page(s, status, "Branch protection", &body))
}

#[derive(Deserialize)]
struct RuleForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    required_approvals: String,
    #[serde(default)]
    required_status_checks: String,
    #[serde(default)]
    allow_force_push: Option<String>,
    #[serde(default)]
    allow_deletion: Option<String>,
    #[serde(default)]
    require_pull_request: Option<String>,
    #[serde(default)]
    dismiss_stale_approvals: Option<String>,
    #[serde(default)]
    require_signed_commits: Option<String>,
    #[serde(default)]
    require_merge_queue: Option<String>,
    #[serde(default)]
    require_code_owner_review: Option<String>,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<RuleForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let key = if idempotency::valid_key(&form.idempotency_key) {
        form.idempotency_key.as_str()
    } else {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Branch protection",
            "<h1>Branch protection</h1><p>Reload the page and submit the form again.</p>",
        )));
    };
    let draft = RuleDraft {
        pattern: &form.pattern,
        allow_force_push: form.allow_force_push.is_some(),
        allow_deletion: form.allow_deletion.is_some(),
        require_pull_request: form.require_pull_request.is_some(),
        required_approvals: &form.required_approvals,
        required_status_checks: &form.required_status_checks,
        dismiss_stale_approvals: form.dismiss_stale_approvals.is_some(),
        require_signed_commits: form.require_signed_commits.is_some(),
        require_merge_queue: form.require_merge_queue.is_some(),
        require_code_owner_review: form.require_code_owner_review.is_some(),
    };
    let approvals = form.required_approvals.trim().parse::<i32>().unwrap_or(-1);
    let body = serde_json::json!({
        "pattern": &form.pattern,
        "allow_force_push": draft.allow_force_push,
        "allow_deletion": draft.allow_deletion,
        "require_pull_request": draft.require_pull_request,
        "required_approvals": approvals,
        "required_status_checks": parse_status_checks(&form.required_status_checks),
        "dismiss_stale_approvals": draft.dismiss_stale_approvals,
        "require_signed_commits": draft.require_signed_commits,
        "require_merge_queue": draft.require_merge_queue,
        "require_code_owner_review": draft.require_code_owner_review,
    })
    .to_string();
    match protection::create_rule(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        body.as_bytes(),
        Some(key),
    )
    .await
    {
        Ok(_) => Ok(
            Redirect::to(&format!("{}/settings/branches", repo_href(&slug, &name))).into_response(),
        ),
        Err(e) => {
            let status = match e.status_code() {
                StatusCode::BAD_REQUEST => StatusCode::BAD_REQUEST,
                StatusCode::UNPROCESSABLE_ENTITY => StatusCode::UNPROCESSABLE_ENTITY,
                StatusCode::CONFLICT => StatusCode::CONFLICT,
                _ => return Err(api_error(&s, &slug, &name, e)),
            };
            let message = match status {
                StatusCode::CONFLICT => "A rule with this pattern already exists.".to_owned(),
                _ => e.message().to_owned(),
            };
            render_settings(&state, &s, &slug, &name, status, Some(&message), &draft).await
        }
    }
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

async fn delete_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name, id)): Path<(String, String, Uuid)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    match protection::delete_rule(&state, s.account.id, s.auth_age, &slug, &name, id, None).await {
        Ok(_) => Ok(
            Redirect::to(&format!("{}/settings/branches", repo_href(&slug, &name))).into_response(),
        ),
        Err(e) => Err(api_error(&s, &slug, &name, e)),
    }
}
