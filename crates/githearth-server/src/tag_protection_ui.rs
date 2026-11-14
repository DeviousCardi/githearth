//! Server-rendered tag-protection settings page: list, create and delete rules.
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and a per-form idempotency key. Reads and writes go through the same
//! `pub(crate)` functions as the JSON API, so authorization, validation and
//! audit events are identical.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, idempotency, tag_protection, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/repositories/{name}/settings/tags",
            get(settings_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/settings/tags/{id}/delete",
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

/// Maps a tag-protection failure to the page a browser should see.
fn api_error(s: &Session, slug: &str, name: &str, error: crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => {
            let body = format!(
                "<h1>Not permitted</h1><p>You do not have permission to manage tag protection.</p><p><a href=\"{}\">Back to the repository</a></p>",
                ui::esc(&repo_href(slug, name))
            );
            ui::UiError::from(page(s, StatusCode::FORBIDDEN, "Not permitted", &body))
        }
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

#[derive(Default)]
struct RuleDraft<'a> {
    pattern: &'a str,
    allowed_to_create: &'a str,
    allow_update: bool,
    allow_delete: bool,
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
        match tag_protection::list_rules(state, s.account.id, s.auth_age, slug, name).await {
            Ok(v) => v,
            Err(e) if e.status_code() == StatusCode::NOT_FOUND => {
                return Err(ui::not_found_page(Some(&s.account.username)));
            }
            Err(_) => return Err(ui::unavailable_page(Some(&s.account.username))),
        };

    let mut rows = String::new();
    for rule in &rules {
        // A viewer who can read but not manage sees the rule but no control
        // that would only return 403.
        let delete = if can_manage {
            format!(
                "<td><form method=\"post\" action=\"{}/settings/tags/{}/delete\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Delete</button></form></td>",
                ui::esc(&repo_href(slug, name)),
                rule.id,
                ui::esc(&s.csrf),
            )
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>{delete}</tr>",
            ui::esc(&rule.pattern),
            ui::esc(&rule.allowed_to_create),
            if rule.allow_update { "yes" } else { "no" },
            if rule.allow_delete { "yes" } else { "no" },
        ));
    }
    let action_header = if can_manage {
        "<th scope=\"col\"></th>"
    } else {
        ""
    };
    let table = if rules.is_empty() {
        "<p class=\"muted\">No tag protection rules yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Tag protection rules</caption><thead><tr>\
<th scope=\"col\">Pattern</th><th scope=\"col\">Create role</th>\
<th scope=\"col\">Allow update</th><th scope=\"col\">Allow delete</th>\
{action_header}</tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let add_form = if can_manage {
        let selected_role = if draft.allowed_to_create.is_empty() {
            "admin"
        } else {
            draft.allowed_to_create
        };
        let roles = tag_protection::CREATE_ROLES
            .iter()
            .map(|role| {
                let selected = if *role == selected_role {
                    " selected"
                } else {
                    ""
                };
                format!("<option value=\"{role}\"{selected}>{role}</option>")
            })
            .collect::<String>();
        format!(
            "<div class=\"card\"><h2>Add a rule</h2>{error}\
<form method=\"post\" action=\"{}/settings/tags\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<label for=\"pattern\">Pattern</label>\
<input id=\"pattern\" name=\"pattern\" required value=\"{}\">\
<label for=\"allowed_to_create\">Create role</label>\
<select id=\"allowed_to_create\" name=\"allowed_to_create\">{roles}</select>\
<label><input type=\"checkbox\" name=\"allow_update\" value=\"1\"{}> Allow update (move)</label>\
<label><input type=\"checkbox\" name=\"allow_delete\" value=\"1\"{}> Allow delete</label>\
<p><button type=\"submit\">Add rule</button></p></form></div>",
            ui::esc(&repo_href(slug, name)),
            ui::esc(&s.csrf),
            ui::esc(&new_key()),
            ui::esc(draft.pattern),
            if draft.allow_update { " checked" } else { "" },
            if draft.allow_delete { " checked" } else { "" },
        )
    } else {
        String::new()
    };

    let body = format!(
        "<h1>Tag protection</h1><p><a href=\"{}\">Back to the repository</a></p>\
<div class=\"card\">{table}</div>{add_form}",
        ui::esc(&repo_href(slug, name)),
    );
    Ok(page(s, status, "Tag protection", &body))
}

#[derive(Deserialize)]
struct TagForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    pattern: String,
    #[serde(default)]
    allowed_to_create: String,
    #[serde(default)]
    allow_update: Option<String>,
    #[serde(default)]
    allow_delete: Option<String>,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<TagForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let key = if idempotency::valid_key(&form.idempotency_key) {
        form.idempotency_key.as_str()
    } else {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Tag protection",
            "<h1>Tag protection</h1><p>Reload the page and submit the form again.</p>",
        )));
    };
    let draft = RuleDraft {
        pattern: &form.pattern,
        allowed_to_create: &form.allowed_to_create,
        allow_update: form.allow_update.is_some(),
        allow_delete: form.allow_delete.is_some(),
    };
    let body = serde_json::json!({
        "pattern": &form.pattern,
        "allowed_to_create": &form.allowed_to_create,
        "allow_update": draft.allow_update,
        "allow_delete": draft.allow_delete,
    })
    .to_string();
    match tag_protection::create_rule(
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
        Ok(_) => {
            Ok(Redirect::to(&format!("{}/settings/tags", repo_href(&slug, &name))).into_response())
        }
        Err(e) => {
            let (status, message) = match e.status_code() {
                StatusCode::BAD_REQUEST => (
                    StatusCode::BAD_REQUEST,
                    "Choose a valid pattern and create role.",
                ),
                StatusCode::CONFLICT => (
                    StatusCode::CONFLICT,
                    "A rule with this pattern already exists.",
                ),
                _ => return Err(api_error(&s, &slug, &name, e)),
            };
            render_settings(&state, &s, &slug, &name, status, Some(message), &draft).await
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
    match tag_protection::delete_rule(&state, s.account.id, s.auth_age, &slug, &name, id, None)
        .await
    {
        Ok(_) => {
            Ok(Redirect::to(&format!("{}/settings/tags", repo_href(&slug, &name))).into_response())
        }
        Err(e) => Err(api_error(&s, &slug, &name, e)),
    }
}
