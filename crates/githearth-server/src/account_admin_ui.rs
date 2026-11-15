//! Server-rendered administration pages for accounts (M4.1, U01–U03).
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and every value is escaped. Reads and writes go through the same
//! `account_admin` functions as the JSON API, so authorization, validation,
//! password hashing and audit events are identical. Only an enabled
//! administrator reaches the pages; anyone else gets the usual `403 not
//! permitted` page, and a signed-out visitor is redirected to sign-in.

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AppState,
    account_admin::{self, Secret},
    ui,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/accounts", get(accounts_page))
        .route("/admin/accounts", axum::routing::post(create_submit))
        .route(
            "/admin/accounts/{id}/disable",
            axum::routing::post(disable_submit),
        )
        .route(
            "/admin/accounts/{id}/enable",
            axum::routing::post(enable_submit),
        )
        .route(
            "/admin/accounts/{id}/promote",
            axum::routing::post(promote_submit),
        )
        .route(
            "/admin/accounts/{id}/demote",
            axum::routing::post(demote_submit),
        )
        .route(
            "/admin/accounts/{id}/password-reset",
            axum::routing::post(reset_submit),
        )
}

/// The navigation link to the administration pages. Nothing is rendered for
/// the page template's anonymous navigation, so it never reveals the pages to
/// a visitor who is not signed in.
pub(crate) fn nav_link() -> &'static str {
    "<a href=\"/admin/accounts\">Accounts</a>"
}

struct Session {
    account: crate::Account,
    csrf: String,
    auth_age: u64,
    fresh_auth: Duration,
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
        csrf: ui::csrf_expected(&hash),
        auth_age,
        fresh_auth: state.fresh_auth(),
    })
}

async fn admin_session(state: &AppState, headers: &HeaderMap) -> Result<Session, ui::UiError> {
    let s = session(state, headers).await?;
    if !s.account.is_admin {
        return Err(not_permitted(&s));
    }
    // Account administration is a sensitive action: the page applies the same
    // fresh-authentication window as the JSON API and offers the prompt that
    // repeats the action once the password has been confirmed.
    if s.auth_age > s.fresh_auth.as_secs() {
        return Err(reauthentication_page(&s));
    }
    Ok(s)
}

/// Checks the form's CSRF token before anything else touches the session, then
/// requires an administrator.
async fn admin_form_session(
    state: &AppState,
    headers: &HeaderMap,
    supplied: &str,
) -> Result<Session, ui::UiError> {
    let hash = ui::browser_token_hash(headers).ok_or_else(ui::auth_required)?;
    if !ui::csrf_matches(supplied, &ui::csrf_expected(&hash)) {
        return Err(ui::forbidden_page());
    }
    admin_session(state, headers).await
}

fn page(s: &Session, status: StatusCode, title: &str, body: &str) -> Response {
    (
        status,
        Html(ui::render_page(
            title,
            Some(&s.account.username),
            Some(&s.csrf),
            s.account.is_admin,
            body,
        )),
    )
        .into_response()
}

/// The refusal a non-administrator sees, matching the JSON API's `403 not
/// permitted`.
fn not_permitted(s: &Session) -> ui::UiError {
    let body = "<h1>Not permitted</h1><p>You do not have permission to administer accounts.</p>";
    ui::UiError::from(page(s, StatusCode::FORBIDDEN, "Not permitted", body))
}

/// The same page for a failed action, with the API's own message.
fn message_page(s: &Session, status: StatusCode, title: &str, message: &str) -> ui::UiError {
    let body = format!(
        "<h1>{}</h1><p role=\"alert\">{}</p><p><a href=\"/admin/accounts\">Back to accounts</a></p>",
        ui::esc(title),
        ui::esc(message)
    );
    ui::UiError::from(page(s, status, title, &body))
}

/// Maps an API failure to the page a browser should see.
fn api_error(s: &Session, error: &crate::ApiError) -> ui::UiError {
    if error.message() == "reauthentication required" {
        return reauthentication_page(s);
    }
    if error.status_code() == StatusCode::FORBIDDEN {
        return not_permitted(s);
    }
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::INTERNAL_SERVER_ERROR => {
            ui::unavailable_page(Some(&s.account.username))
        }
        status => message_page(s, status, "Account not changed", account_notice(error)),
    }
}

/// The settings-page prompt for a sensitive action whose session is no longer
/// fresh. Confirming the password there refreshes `authenticated_at` and
/// returns the administrator to the accounts page to repeat the action.
fn reauthentication_page(s: &Session) -> ui::UiError {
    let minutes = ui::fresh_auth_minutes(s.fresh_auth);
    let body = format!(
        "<h1>Confirm your password</h1>\
<p role=\"alert\">Reauthentication required. Account administration needs a password entry in the last {minutes} minutes.</p>\
<form class=\"card\" method=\"post\" action=\"/settings/reauthenticate\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"return_to\" value=\"/admin/accounts\">\
<label for=\"reauth_password\">Password</label>\
<input id=\"reauth_password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required>\
<p><button type=\"submit\">Confirm password</button></p></form>\
<p><a href=\"/admin/accounts\">Back to accounts</a></p>",
        ui::esc(&s.csrf),
        minutes = minutes
    );
    ui::UiError::from(page(
        s,
        StatusCode::FORBIDDEN,
        "Confirm your password",
        &body,
    ))
}

/// A short message for a refused account action, never echoing a password.
fn account_notice(error: &crate::ApiError) -> &'static str {
    match (error.status_code(), error.message()) {
        (StatusCode::CONFLICT, message) if message.contains("own account") => {
            "You cannot disable your own account."
        }
        (StatusCode::CONFLICT, message) if message.contains("cannot be demoted") => {
            "The last enabled administrator cannot be removed."
        }
        (StatusCode::CONFLICT, message) if message.contains("last enabled administrator") => {
            "The last enabled administrator cannot be disabled."
        }
        (StatusCode::CONFLICT, message) if message.contains("disabled account") => {
            "A disabled account cannot be promoted."
        }
        (StatusCode::CONFLICT, _) => "An account with that username already exists.",
        (StatusCode::BAD_REQUEST, message) if message.starts_with("username") => {
            "Use 3 to 64 lowercase letters, digits, underscores or hyphens for the username."
        }
        (StatusCode::BAD_REQUEST, _) => "Use a password of 12 to 1024 characters.",
        _ => "The account could not be changed. Try again.",
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    after: Option<Uuid>,
    #[serde(default)]
    notice: Option<String>,
}

const PAGE_SIZE: i64 = 50;

fn next_href(accounts: &[account_admin::AccountView]) -> Option<String> {
    if accounts.len() as i64 != PAGE_SIZE {
        return None;
    }
    let last = accounts.last()?;
    Some(format!("/admin/accounts?after={}", last.id))
}

async fn accounts_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response, ui::UiError> {
    let s = admin_session(&state, &headers).await?;
    let accounts = account_admin::list(&state.pool, query.after, PAGE_SIZE)
        .await
        .map_err(|e| api_error(&s, &e))?;
    let notice = query.notice.as_deref().map(|notice| match notice {
        "created" => "Account created.",
        "disabled" => "Account disabled.",
        "enabled" => "Account enabled.",
        "promoted" => "Account promoted to site administrator.",
        "demoted" => "Account demoted from site administrator.",
        "reset" => "Password reset. That account's sessions were signed out.",
        _ => "",
    });
    Ok(render(
        &s,
        &accounts,
        StatusCode::OK,
        notice.filter(|n| !n.is_empty()),
        None,
    ))
}

fn render(
    s: &Session,
    accounts: &[account_admin::AccountView],
    status: StatusCode,
    notice: Option<&str>,
    error: Option<&str>,
) -> Response {
    let notice = notice
        .map(|n| format!("<p role=\"status\">{}</p>", ui::esc(n)))
        .unwrap_or_default();
    let error = error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();

    let mut rows = String::new();
    for account in accounts {
        let state_label = if account.suspended {
            if account.must_change_password {
                "<span class=\"badge\">disabled</span> · reset pending".to_owned()
            } else {
                "<span class=\"badge\">disabled</span>".to_owned()
            }
        } else if account.is_admin && account.must_change_password {
            "enabled · administrator · reset pending".to_owned()
        } else if account.is_admin {
            "enabled · administrator".to_owned()
        } else if account.must_change_password {
            "enabled · reset pending".to_owned()
        } else {
            "enabled".to_owned()
        };
        let own = account.id == s.account.id;
        // A form per action: the administration page never relies on
        // JavaScript, and each button names the account it acts on.
        let toggle = if account.suspended {
            action_form(
                s,
                &format!("/admin/accounts/{}/enable", account.id),
                "Enable",
                &format!("Enable {}", account.username),
                None,
            )
        } else if own {
            "<span class=\"muted\">you</span>".to_owned()
        } else {
            action_form(
                s,
                &format!("/admin/accounts/{}/disable", account.id),
                "Disable",
                &format!("Disable {}", account.username),
                None,
            )
        };
        let reset = action_form(
            s,
            &format!("/admin/accounts/{}/password-reset", account.id),
            "Reset password",
            &format!("Issue a one-time reset code for {}", account.username),
            None,
        );
        let role = if own || account.suspended {
            String::new()
        } else if account.is_admin {
            action_form(
                s,
                &format!("/admin/accounts/{}/demote", account.id),
                "Demote",
                &format!("Remove site administration from {}", account.username),
                None,
            )
        } else {
            action_form(
                s,
                &format!("/admin/accounts/{}/promote", account.id),
                "Promote",
                &format!("Make {} a site administrator", account.username),
                None,
            )
        };
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{state_label}</td><td>{}</td><td>{toggle}</td><td>{role}</td><td>{reset}</td></tr>",
            ui::esc(&account.username),
            ui::esc(&account.created_at),
        ));
    }
    let table = if accounts.is_empty() {
        "<p class=\"muted\">No accounts yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Accounts</caption><thead><tr>\
<th scope=\"col\">Username</th><th scope=\"col\">State</th><th scope=\"col\">Created</th>\
<th scope=\"col\"></th><th scope=\"col\"></th><th scope=\"col\"></th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    let more = next_href(accounts)
        .map(|href| format!("<p><a href=\"{}\">Next page</a></p>", ui::esc(&href)))
        .unwrap_or_default();

    let create = format!(
        "<form class=\"card\" method=\"post\" action=\"/admin/accounts\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"account_username\">Username</label>\
<input id=\"account_username\" name=\"username\" required pattern=\"[a-z0-9_-]{{3,64}}\" aria-describedby=\"account-username-help\">\
<p class=\"muted\" id=\"account-username-help\">3 to 64 lowercase letters, digits, underscores or hyphens.</p>\
<label for=\"account_password\">Initial password</label>\
<input id=\"account_password\" name=\"password\" type=\"password\" autocomplete=\"new-password\" required minlength=\"12\" maxlength=\"1024\" aria-describedby=\"account-password-help\">\
<p class=\"muted\" id=\"account-password-help\">At least 12 characters. Give it to the account owner out of band; it is never shown again.</p>\
<p><button type=\"submit\">Create account</button></p></form>",
        csrf = ui::esc(&s.csrf)
    );

    let body = format!(
        "<h1>Accounts</h1>{notice}{error}<div class=\"card\">{table}</div>{more}<h2>Create account</h2>{create}"
    );
    page(s, status, "Accounts", &body)
}

/// A single-button form carrying the CSRF token and, for a password action,
/// the new password field.
fn action_form(
    s: &Session,
    action: &str,
    label: &str,
    title: &str,
    password: Option<(&str, &str)>,
) -> String {
    let field = password
        .map(|(name, placeholder)| {
            format!(
                "<label for=\"new_password_{id}\" class=\"muted\">New password</label>\
<input id=\"new_password_{id}\" name=\"{name}\" type=\"password\" autocomplete=\"new-password\" \
required minlength=\"12\" maxlength=\"1024\" placeholder=\"{placeholder}\">",
                id = action.rsplit('/').nth(1).unwrap_or("x").replace('-', ""),
                placeholder = ui::esc(placeholder)
            )
        })
        .unwrap_or_default();
    format!(
        "<form method=\"post\" action=\"{action}\">{field}\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<button type=\"submit\" title=\"{title}\">{label}</button></form>",
        action = ui::esc(action),
        csrf = ui::esc(&s.csrf),
        title = ui::esc(title),
        label = ui::esc(label),
    )
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::create_account_inner(
        &state,
        s.account.id,
        &form.username,
        Secret::new(form.password),
    )
    .await
    {
        Ok(_) => Ok(see_other("/admin/accounts?notice=created")),
        Err(e) if e.status_code() == StatusCode::BAD_REQUEST => {
            let accounts = account_admin::list(&state.pool, None, PAGE_SIZE)
                .await
                .map_err(|e| api_error(&s, &e))?;
            Ok(render(
                &s,
                &accounts,
                StatusCode::BAD_REQUEST,
                None,
                Some(account_notice(&e)),
            ))
        }
        Err(e) => Err(api_error(&s, &e)),
    }
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

async fn disable_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::disable_inner(&state, s.account.id, id).await {
        Ok(_) => Ok(see_other("/admin/accounts?notice=disabled")),
        Err(e) => Err(api_error(&s, &e)),
    }
}

async fn enable_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::enable_inner(&state, s.account.id, id).await {
        Ok(_) => Ok(see_other("/admin/accounts?notice=enabled")),
        Err(e) => Err(api_error(&s, &e)),
    }
}

async fn promote_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::promote_inner(&state, s.account.id, id).await {
        Ok(_) => Ok(see_other("/admin/accounts?notice=promoted")),
        Err(e) => Err(api_error(&s, &e)),
    }
}

async fn demote_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::demote_inner(&state, s.account.id, id).await {
        Ok(_) => Ok(see_other("/admin/accounts?notice=demoted")),
        Err(e) => Err(api_error(&s, &e)),
    }
}

#[derive(Deserialize)]
struct ResetForm {
    #[serde(default)]
    csrf_token: String,
}

async fn reset_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<ResetForm>,
) -> Result<Response, ui::UiError> {
    let s = admin_form_session(&state, &headers, &form.csrf_token).await?;
    match account_admin::reset_password_inner(&state, s.account.id, id).await {
        Ok(reset) => {
            let body = format!(
                "<h1>Password reset</h1>\
<p role=\"status\">Give this one-time code to the account owner out of band. \
It expires at {} and can be used once.</p>\
<pre style=\"white-space:pre-wrap;word-break:break-all\" role=\"alert\">{}</pre>\
<p class=\"muted\">The account's sessions, access tokens and SSH keys were revoked. \
It cannot use the API, Git or the browser until it redeems the code at <a href=\"/reset\">/reset</a> and sets a new password.</p>\
<p><a href=\"/admin/accounts\">Back to accounts</a></p>",
                ui::esc(&reset.expires_at),
                ui::esc(&reset.code)
            );
            Ok((
                StatusCode::OK,
                Html(ui::render_page(
                    "Password reset",
                    Some(&s.account.username),
                    Some(&s.csrf),
                    s.account.is_admin,
                    &body,
                )),
            )
                .into_response())
        }
        Err(e) => Err(api_error(&s, &e)),
    }
}

fn see_other(path: &str) -> Response {
    Redirect::to(path).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str, suspended: bool) -> account_admin::AccountView {
        account_admin::AccountView {
            id: Uuid::new_v4(),
            username: username.to_owned(),
            is_admin: false,
            suspended,
            must_change_password: false,
            email: None,
            email_verified_at: None,
            created_at: "2026-11-15T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn only_a_full_page_offers_a_next_link() {
        let full: Vec<_> = (0..PAGE_SIZE)
            .map(|i| account(&format!("u{i}"), false))
            .collect();
        let href = next_href(&full).unwrap();
        assert!(href.starts_with("/admin/accounts?after="));
        assert!(href.ends_with(&full.last().unwrap().id.to_string()));
        assert!(next_href(&full[..1]).is_none());
        assert!(next_href(&[]).is_none());
    }

    #[test]
    fn refusal_notices_are_fixed_text() {
        let own = crate::ApiError(
            StatusCode::CONFLICT,
            "an administrator cannot disable their own account",
        );
        assert_eq!(account_notice(&own), "You cannot disable your own account.");
        let last = crate::ApiError(
            StatusCode::CONFLICT,
            "the last enabled administrator cannot be disabled",
        );
        assert_eq!(
            account_notice(&last),
            "The last enabled administrator cannot be disabled."
        );
        let last_demoted = crate::ApiError(
            StatusCode::CONFLICT,
            "the last enabled administrator cannot be demoted",
        );
        assert_eq!(
            account_notice(&last_demoted),
            "The last enabled administrator cannot be removed."
        );
        let username = crate::ApiError(
            StatusCode::BAD_REQUEST,
            "username must contain 3..64 lowercase ASCII letters, digits, underscores or hyphens",
        );
        assert!(account_notice(&username).starts_with("Use 3 to 64"));
        let password = crate::ApiError(
            StatusCode::BAD_REQUEST,
            "password must contain 12..1024 bytes without NUL",
        );
        assert_eq!(
            account_notice(&password),
            "Use a password of 12 to 1024 characters."
        );
        let other = crate::ApiError(StatusCode::CONFLICT, "already exists");
        assert_eq!(
            account_notice(&other),
            "An account with that username already exists."
        );
    }

    #[test]
    fn navigation_link_never_renders_for_a_visitor() {
        let page = ui::render_page("Home", None, None, false, "<h1>Home</h1>");
        assert!(!page.contains("/admin/accounts"));
        let page = ui::render_page("Home", Some("root"), Some("token"), false, "<h1>Home</h1>");
        assert!(!page.contains(nav_link()));
        let page = ui::render_page("Home", Some("root"), Some("token"), true, "<h1>Home</h1>");
        assert!(page.contains(nav_link()));
    }
}
