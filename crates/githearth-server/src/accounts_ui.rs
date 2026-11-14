//! Browser page for account administration (M4.1 U01). Every form carries
//! the session's CSRF token; every action runs the same authorization and
//! shared functions as the JSON API.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use githearth_authz::Action;
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, accounts, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/accounts", get(admin_page).post(create_submit))
        .route(
            "/admin/accounts/{id}/disable",
            axum::routing::post(disable_submit),
        )
        .route(
            "/admin/accounts/{id}/enable",
            axum::routing::post(enable_submit),
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

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

/// Renders the admin page from a fresh account list, optionally showing an
/// error above the forms.
async fn render_admin(
    state: &AppState,
    s: &Session,
    status: StatusCode,
    error: Option<&str>,
) -> Result<Response, ui::UiError> {
    let users = accounts::list_accounts(state)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let error = error
        .map(|message| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(message)))
        .unwrap_or_default();

    let mut rows = String::new();
    for user in &users {
        let status_badge = if user.disabled {
            "<span class=\"badge\" style=\"background:#b00020\">Disabled</span>"
        } else {
            "<span class=\"badge\" style=\"background:#1a7f37\">Active</span>"
        };
        let admin = if user.is_admin {
            " (administrator)"
        } else {
            ""
        };
        let toggle = if user.disabled {
            format!(
                "<form method=\"post\" action=\"/admin/accounts/{}/enable\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Enable</button></form>",
                user.id,
                ui::esc(&s.csrf)
            )
        } else {
            format!(
                "<form method=\"post\" action=\"/admin/accounts/{}/disable\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Disable</button></form>",
                user.id,
                ui::esc(&s.csrf)
            )
        };
        let reset = format!(
            "<form method=\"post\" action=\"/admin/accounts/{}/password\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<label for=\"pw-{}\" class=\"muted\">New password</label>\
<input id=\"pw-{}\" name=\"new_password\" type=\"password\" required \
autocomplete=\"new-password\" style=\"width:auto\">\
<button type=\"submit\">Reset password</button></form>",
            user.id,
            ui::esc(&s.csrf),
            user.id,
            user.id
        );
        rows.push_str(&format!(
            "<tr><td>{}{}</td><td>{}</td><td>{}</td><td>{}</td><td>{toggle} {reset}</td></tr>",
            ui::esc(&user.username),
            admin,
            status_badge,
            ui::esc(&user.created_at),
            ui::esc(user.password_changed_at.as_deref().unwrap_or("—")),
        ));
    }
    let table = if users.is_empty() {
        "<p class=\"muted\">No accounts.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Accounts</caption><thead><tr>\
<th scope=\"col\">Username</th><th scope=\"col\">Status</th><th scope=\"col\">Created</th>\
<th scope=\"col\">Password changed</th><th scope=\"col\">Actions</th></tr></thead>\
<tbody>{rows}</tbody></table>"
        )
    };
    let create_form = format!(
        "<form class=\"card\" method=\"post\" action=\"/admin/accounts\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"username\">Username</label>\
<input id=\"username\" name=\"username\" required maxlength=\"64\" aria-describedby=\"username-help\">\
<p class=\"muted\" id=\"username-help\">3 to 64 lowercase letters, digits, underscores or hyphens.</p>\
<label for=\"password\">Password</label>\
<input id=\"password\" name=\"password\" type=\"password\" required minlength=\"12\" \
autocomplete=\"new-password\" aria-describedby=\"password-help\">\
<p class=\"muted\" id=\"password-help\">At least 12 characters. Never generated or emailed.</p>\
<label><input type=\"checkbox\" name=\"is_admin\" value=\"1\"> Site administrator</label>\
<p><button type=\"submit\">Create account</button></p></form>",
        csrf = ui::esc(&s.csrf)
    );
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / Accounts</nav>\
<h1>Accounts</h1>{error}<h2>Create account</h2>{create_form}\
<h2>Existing accounts</h2><div class=\"card\">{table}</div>"
    );
    Ok(page(s, status, "Accounts", &body))
}

async fn admin_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    if accounts::authorize_instance(&state, s.account.id, s.auth_age, Action::AccountRead)
        .await
        .is_err()
    {
        return Err(ui::forbidden_page());
    }
    render_admin(&state, &s, StatusCode::OK, None).await
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    is_admin: Option<String>,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    if accounts::authorize_instance(&state, s.account.id, s.auth_age, Action::AccountCreate)
        .await
        .is_err()
    {
        return Err(ui::forbidden_page());
    }
    let is_admin = form.is_admin.as_deref() == Some("1");
    match accounts::create_account(
        &state,
        s.account.id,
        &form.username,
        &form.password,
        is_admin,
    )
    .await
    {
        Ok(_) => Ok(Redirect::to("/admin/accounts").into_response()),
        Err(e) => {
            let message = if e.status_code() == StatusCode::CONFLICT {
                "That username is already taken."
            } else {
                "Enter a valid username and a password of at least 12 characters."
            };
            render_admin(&state, &s, e.status_code(), Some(message)).await
        }
    }
}

fn action_error(s: &Session, error: crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => ui::forbidden_page(),
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

async fn disable_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    if accounts::authorize_instance(&state, s.account.id, s.auth_age, Action::AccountDisable)
        .await
        .is_err()
    {
        return Err(ui::forbidden_page());
    }
    match accounts::set_disabled(&state, s.account.id, id, true).await {
        Ok(()) => Ok(Redirect::to("/admin/accounts").into_response()),
        Err(e) if e.status_code() == StatusCode::CONFLICT => {
            render_admin(&state, &s, StatusCode::CONFLICT, Some(e.message())).await
        }
        Err(e) => Err(action_error(&s, e)),
    }
}

async fn enable_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    if accounts::authorize_instance(&state, s.account.id, s.auth_age, Action::AccountEnable)
        .await
        .is_err()
    {
        return Err(ui::forbidden_page());
    }
    match accounts::set_disabled(&state, s.account.id, id, false).await {
        Ok(()) => Ok(Redirect::to("/admin/accounts").into_response()),
        Err(e) => Err(action_error(&s, e)),
    }
}
