//! Browser page for changing your own password (M4.1 U02). Uses the browser
//! session and CSRF token, and the same `passwords` functions as the JSON API.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use githearth_authz::Action;
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, accounts, passwords, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/password", get(page).post(submit))
        .route("/admin/accounts/{id}/password", post(reset_submit))
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

fn page_response(s: &Session, status: StatusCode, error: Option<&str>) -> Response {
    let error = error
        .map(|message| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(message)))
        .unwrap_or_default();
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/settings\">Settings</a> / Password</nav>\
<h1>Change password</h1>\
<form class=\"card\" method=\"post\" action=\"/settings/password\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">{error}\
<label for=\"current_password\">Current password</label>\
<input id=\"current_password\" name=\"current_password\" type=\"password\" required \
autocomplete=\"current-password\">\
<label for=\"new_password\">New password</label>\
<input id=\"new_password\" name=\"new_password\" type=\"password\" required minlength=\"12\" \
autocomplete=\"new-password\" aria-describedby=\"password-help\">\
<p class=\"muted\" id=\"password-help\">At least 12 characters. Changing it signs out every session.</p>\
<label for=\"confirm_password\">Confirm new password</label>\
<input id=\"confirm_password\" name=\"confirm_password\" type=\"password\" required \
autocomplete=\"new-password\">\
<p><button type=\"submit\">Change password</button></p></form>",
        csrf = ui::esc(&s.csrf),
        error = error
    );
    (
        status,
        Html(ui::render_page(
            "Change password",
            Some(&s.account.username),
            Some(&s.csrf),
            &body,
        )),
    )
        .into_response()
}

async fn page(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    Ok(page_response(&s, StatusCode::OK, None))
}

#[derive(Deserialize)]
struct PasswordForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    if form.new_password != form.confirm_password {
        return Ok(page_response(
            &s,
            StatusCode::BAD_REQUEST,
            Some("The new passwords do not match."),
        ));
    }
    if !accounts::password_ok(&form.new_password) {
        return Ok(page_response(
            &s,
            StatusCode::BAD_REQUEST,
            Some("A new password of at least 12 characters is required."),
        ));
    }
    match passwords::change_password(
        &state,
        s.account.clone(),
        &form.current_password,
        form.new_password,
    )
    .await
    {
        Ok(()) => Ok(ui::signed_out(&state).into_response()),
        Err(e) if e.status_code() == StatusCode::FORBIDDEN => Ok(page_response(
            &s,
            StatusCode::FORBIDDEN,
            Some("The current password is incorrect."),
        )),
        Err(e) if e.status_code() == StatusCode::TOO_MANY_REQUESTS => Ok(page_response(
            &s,
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many failed attempts. Try again in a few minutes."),
        )),
        Err(_) => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}

#[derive(Deserialize)]
struct ResetForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    new_password: String,
}

/// The administrator reset form on the accounts page. Shares authorization and
/// hashing with the JSON API; a valid reset redirects back to the list.
async fn reset_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<ResetForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    if accounts::authorize_instance(
        &state,
        s.account.id,
        s.auth_age,
        Action::AccountResetPassword,
    )
    .await
    .is_err()
    {
        return Err(ui::forbidden_page());
    }
    if !accounts::password_ok(&form.new_password) {
        return Err(ui::UiError::from(
            (
                StatusCode::BAD_REQUEST,
                Html(ui::render_page(
                    "Accounts",
                    Some(&s.account.username),
                    Some(&s.csrf),
                    "<h1>Accounts</h1><p class=\"error\" role=\"alert\">A new password of at \
                 least 12 characters is required.</p>\
                 <p><a href=\"/admin/accounts\">Back to accounts</a></p>",
                )),
            )
                .into_response(),
        ));
    }
    match passwords::set_password(
        &state,
        s.account.id,
        id,
        form.new_password,
        "account.password_reset",
    )
    .await
    {
        Ok(()) => Ok(Redirect::to("/admin/accounts").into_response()),
        Err(e) if e.status_code() == StatusCode::NOT_FOUND => {
            Err(ui::not_found_page(Some(&s.account.username)))
        }
        Err(e) if e.status_code() == StatusCode::FORBIDDEN => Err(ui::forbidden_page()),
        Err(_) => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}
