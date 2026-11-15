use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, SET_COOKIE, X_FRAME_OPTIONS},
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use githearth_authz::{Action, Resource};
use serde::Deserialize;
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AppState, authenticate_token_hash, facts, forks, idempotency, repositories, repository_api,
};

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    csrf_token: String,
}

#[derive(Deserialize)]
struct ResetForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    new_password: String,
}

#[derive(Deserialize)]
struct LogoutForm {
    #[serde(default)]
    csrf_token: String,
}

#[derive(Deserialize, Default)]
struct LoginQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    reset: Option<String>,
}

pub(crate) struct UiError(Box<Response>);
impl From<Response> for UiError {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}
impl axum::response::IntoResponse for UiError {
    fn into_response(self) -> Response {
        *self.0
    }
}

pub(crate) fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The fresh-auth window rounded to the minute for prompt text, always at
/// least one so an unusually short configured window still reads sensibly.
pub(crate) fn fresh_auth_minutes(window: Duration) -> u64 {
    (window.as_secs() / 60).max(1)
}

pub(crate) fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn csrf_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("githearth_csrf=") {
            return Some(rest.to_owned());
        }
    }
    None
}

/// A well-formed CSRF token from the cookie, or a fresh one for a public page
/// such as the sign-in and reset forms.
fn page_csrf_token(headers: &HeaderMap) -> String {
    csrf_cookie(headers)
        .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            let mut bytes = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
            hex::encode(bytes)
        })
}

fn csrf_cookie_header(state: &AppState, token: &str) -> String {
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    format!(
        "githearth_csrf={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{secure}",
        ttl = state.config.token_ttl_seconds
    )
}

pub(crate) fn browser_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    let mut found = None;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("githearth_session=") {
            found = Some(rest.to_owned());
            break;
        }
    }
    let token = found?;
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(token)
}

pub(crate) fn browser_token_hash(headers: &HeaderMap) -> Option<Vec<u8>> {
    use sha2::{Digest, Sha256};
    Some(Sha256::digest(browser_token(headers)?.as_bytes()).to_vec())
}

pub(crate) async fn browser_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(crate::Account, u64), crate::ApiError> {
    let hash = browser_token_hash(headers).ok_or_else(crate::unauthorized)?;
    authenticate_token_hash(&state.pool, hash).await
}

/// Compares digests so the time taken does not reveal how much of the secret matched.
pub(crate) fn csrf_matches(supplied: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    !expected.is_empty() && Sha256::digest(supplied) == Sha256::digest(expected)
}

pub(crate) fn csrf_expected(token_hash: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token_hash);
    hex::encode(&digest[..16])
}

fn set_session_cookie(state: &AppState, token: &str) -> String {
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    format!(
        "githearth_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{secure}",
        ttl = state.config.token_ttl_seconds
    )
}

fn clear_session_cookie(state: &AppState) -> String {
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    format!("githearth_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
}

pub(crate) fn render_page(
    title: &str,
    username: Option<&str>,
    csrf: Option<&str>,
    is_admin: bool,
    body: &str,
) -> String {
    let nav = if let Some(name) = username {
        if let Some(token) = csrf {
            // Only a site administrator is offered the accounts surface; the
            // pages themselves still refuse anyone else.
            let admin_link = if is_admin {
                crate::account_admin_ui::nav_link()
            } else {
                ""
            };
            format!(
                "<nav aria-label=\"Primary\"><a href=\"/\">GitHearth</a><span class=\"muted\" style=\"color:#ccc\">{}</span>\
<a href=\"/notifications\" style=\"margin-left:auto\">Notifications</a>{}\
<a href=\"/settings\">Settings</a>\
<form method=\"post\" action=\"/logout\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Sign out</button></form></nav>",
                esc(name),
                admin_link,
                esc(token)
            )
        } else {
            format!(
                "<nav aria-label=\"Primary\"><a href=\"/\">GitHearth</a><span class=\"muted\" style=\"color:#ccc\">{}</span></nav>",
                esc(name)
            )
        }
    } else {
        "<nav aria-label=\"Primary\"><a href=\"/\">GitHearth</a><a href=\"/login\" style=\"margin-left:auto\">Sign in</a></nav>"
            .to_string()
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>\
body{{font-family:system-ui,sans-serif;margin:0;background:#f7f7f8;color:#111;overflow-wrap:anywhere}}\
main{{max-width:900px;margin:2rem auto;padding:0 1rem}}\
nav{{background:#111;color:#fff;padding:.75rem 1rem;display:flex;gap:1rem;align-items:center}}\
nav a{{color:#fff;text-decoration:none}}\
.card{{background:#fff;border:1px solid #ddd;border-radius:8px;padding:1rem;margin:1rem 0;overflow-x:auto}}\
label{{display:block;margin:.5rem 0 .25rem}}\
input:not([type=checkbox]):not([type=radio]){{padding:.5rem;width:100%;box-sizing:border-box}}\
input[type=checkbox],input[type=radio]{{width:auto;margin-right:.35rem}}\
table{{border-collapse:collapse}}\
button{{padding:.5rem 1rem;cursor:pointer}}\
.error{{color:#b00020;margin:.5rem 0}}\
.muted{{color:#666;font-size:.9rem}}\
.badge{{display:inline-block;background:#111;color:#fff;border-radius:999px;padding:.1rem .5rem;font-size:.85rem}}\
ul{{list-style:none;padding:0}}\
li{{padding:.4rem 0;border-bottom:1px solid #eee}}\
a:focus-visible,button:focus-visible,input:focus-visible,textarea:focus-visible,select:focus-visible{{outline:3px solid #005fcc;outline-offset:2px}}\
.skip-link{{position:absolute;top:-3rem;left:0;background:#111;color:#fff;padding:.5rem 1rem;z-index:1000}}\
.skip-link:focus-visible{{top:0}}\
</style></head><body><a class=\"skip-link\" href=\"#main\">Skip to content</a>{nav}<main id=\"main\" tabindex=\"-1\">{body}</main></body></html>",
        title = esc(title),
        nav = nav,
        body = body,
    )
}

fn clear_csrf_cookie(state: &AppState) -> String {
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    format!("githearth_csrf=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
}

/// Redirect to sign-in and drop the browser's session cookie, which the server no longer honours.
fn signed_out(state: &AppState) -> UiError {
    let mut response = Redirect::to("/login").into_response();
    if let Ok(value) = clear_session_cookie(state).parse() {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    UiError(Box::new(response))
}

pub(crate) fn auth_required() -> UiError {
    UiError(Box::new(Redirect::to("/login").into_response()))
}

/// `410 Gone` for a deleted or restoring repository path. A viewer who may
/// restore sees a CSRF-protected form; everyone else sees only the gone page.
/// This is the web half of the lifecycle rule: the old path never redirects,
/// and after a successful restore it resolves again because the row is active.
pub(crate) async fn repository_gone_page(
    state: &AppState,
    headers: &HeaderMap,
    account: Option<&crate::Account>,
    auth_age: Option<u64>,
    org_slug: &str,
    name: &str,
) -> UiError {
    let username = account.map(|account| account.username.as_str());
    let is_admin = account.is_some_and(|account| account.is_admin);
    let csrf = account.and_then(|_| browser_token_hash(headers).map(|hash| csrf_expected(&hash)));
    let mut restoring = false;
    let mut can_restore = false;
    if let (Some(account), Some(auth_age), Some(_)) = (account, auth_age, &csrf)
        && let Some((org_id, row)) =
            crate::repository_restore::deleted_row(state, org_slug, name).await
    {
        restoring = row.status == githearth_core::RepositoryStatus::Restoring;
        if let Ok(mut conn) = state.pool.acquire().await
            && let Ok(Some(facts)) = facts::load(&mut conn, account.id, org_id).await
        {
            can_restore = crate::repository_restore::may_restore(state, &facts, &row, auth_age);
        }
    }
    let mut action = String::new();
    if can_restore {
        if restoring {
            action.push_str(
                "<p class=\"muted\">A restore is already in progress. Reload in a moment.</p>",
            );
        } else if let Some(csrf) = &csrf {
            action.push_str(&format!(
                "<form class=\"card\" method=\"post\" action=\"/organizations/{}/repositories/{}/restore\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<p><button type=\"submit\">Restore repository</button></p></form>",
                url_encode(org_slug),
                url_encode(name),
                esc(csrf),
                esc(&Uuid::new_v4().simple().to_string()),
            ));
        }
    }
    let body = format!(
        "<h1>Repository gone</h1><p>This repository was deleted. Its path returns 410 until a restore completes.</p>{action}"
    );
    UiError(Box::new(
        (
            StatusCode::GONE,
            Html(render_page(
                "Repository gone",
                username,
                csrf.as_deref(),
                is_admin,
                &body,
            )),
        )
            .into_response(),
    ))
}

/// A generic `410 Gone` page for sub-pages of a deleted repository (issues,
/// pull requests, settings) that do not carry enough route context to render
/// the restore form. The repository landing page and browse pages use
/// [`repository_gone_page`], which can offer the form.
pub(crate) fn gone_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::GONE,
            Html(render_page(
                "Repository gone",
                username,
                None,
                false,
                "<h1>Repository gone</h1><p>This repository was deleted. The path returns 410 until a restore completes.</p>",
            )),
        )
            .into_response(),
    ))
}

pub(crate) fn not_found_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::NOT_FOUND,
            Html(render_page(
                "Not found",
                username,
                None,
                false,
                "<h1>Not found</h1>",
            )),
        )
            .into_response(),
    ))
}

/// `410 Gone` for an address a repository has moved away from (ADR 0003). The
/// body says only that the address is gone — never where the repository went,
/// and the same for a public and a private repository — so following an old
/// link reveals nothing about the new home.
pub(crate) fn moved_away_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::GONE,
            Html(render_page(
                "Repository moved",
                username,
                None,
                false,
                "<h1>This repository has moved</h1><p>This address no longer points at a repository. Look up the repository by its organization.</p>",
            )),
        )
            .into_response(),
    ))
}

/// Whether `owner/name` used to be a repository that has since moved to another
/// organization. The repository page and its sub-pages answer `410` instead of
/// resolving the name, so a redirected or reused name never reaches them.
pub(crate) async fn path_moved_away(state: &AppState, slug: &str, name: &str) -> bool {
    if !crate::valid_path_param(slug) || !crate::valid_path_param(name) {
        return false;
    }
    let Ok(mut conn) = state.pool.acquire().await else {
        return false;
    };
    let org_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await
        .ok()
        .flatten();
    let Some(org_id) = org_id else {
        return false;
    };
    repositories::path_moved_away(&mut conn, org_id, name)
        .await
        .unwrap_or(false)
}

/// Styled 404 for paths no router matched. Without this fallback axum answers
/// with a bare, header-less 404 for unknown browse URLs.
async fn fallback_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    // Unknown API paths keep the JSON envelope; only browser paths get a page.
    if uri.path() == "/api" || uri.path().starts_with("/api/") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "not found"})),
        )
            .into_response();
    }
    let username = browser_auth(&state, &headers)
        .await
        .ok()
        .map(|(account, _)| account.username);
    (
        StatusCode::NOT_FOUND,
        Html(render_page(
            "Not found",
            username.as_deref(),
            None,
            false,
            "<h1>Not found</h1>",
        )),
    )
        .into_response()
}

pub(crate) fn unavailable_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render_page(
                "Unavailable",
                username,
                None,
                false,
                "<h1>Unavailable</h1>",
            )),
        )
            .into_response(),
    ))
}

fn busy_page() -> UiError {
    UiError(Box::new(
        (
            StatusCode::TOO_MANY_REQUESTS,
            Html(render_page(
                "Busy",
                None,
                None,
                false,
                "<h1>Busy</h1><p>Too many sign-ins in progress. Try again in a moment.</p>",
            )),
        )
            .into_response(),
    ))
}

/// A failed sign-in throttle (M4.1 U03). The wording is the same for a real and
/// an unknown username, so it never reveals which accounts exist.
fn too_many_attempts_page() -> UiError {
    UiError(Box::new(
        (
            StatusCode::TOO_MANY_REQUESTS,
            Html(render_page(
                "Too many attempts",
                None,
                None,
                false,
                "<h1>Too many sign-in attempts</h1><p>Wait a few minutes and try again.</p>",
            )),
        )
            .into_response(),
    ))
}

pub(crate) fn forbidden_page() -> UiError {
    UiError(Box::new(
        (
            StatusCode::FORBIDDEN,
            Html(render_page(
                "Forbidden",
                None,
                None,
                false,
                "<h1>Forbidden</h1><p>Invalid CSRF token.</p>",
            )),
        )
            .into_response(),
    ))
}

/// The prompt a browser shows when the engine refused a sensitive action
/// because the session is older than the fresh-authentication window. The
/// submitted password refreshes `sessions.authenticated_at` and the settings
/// handler returns to `return_to`, so the operator can repeat the action.
pub(crate) fn reauthentication_page(
    username: Option<&str>,
    is_admin: bool,
    csrf: &str,
    fresh_auth: Duration,
    return_to: &str,
) -> UiError {
    let minutes = fresh_auth_minutes(fresh_auth);
    let body = format!(
        "<h1>Confirm your password</h1>\
<p role=\"alert\">Reauthentication required. This action needs a password entry in the last {minutes} minutes.</p>\
<form class=\"card\" method=\"post\" action=\"/settings/reauthenticate\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<input type=\"hidden\" name=\"return_to\" value=\"{back}\">\
<label for=\"reauth_password\">Password</label>\
<input id=\"reauth_password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required>\
<p><button type=\"submit\">Confirm password</button></p></form>",
        csrf = esc(csrf),
        back = esc(return_to),
    );
    UiError(Box::new(
        (
            StatusCode::FORBIDDEN,
            Html(render_page(
                "Confirm your password",
                username,
                Some(csrf),
                is_admin,
                &body,
            )),
        )
            .into_response(),
    ))
}

/// The friendly message a browser form should show when its submit ran into
/// the idempotency layer: 409 (the same key is still running) or 422 (the same
/// key was already used with a different body). Any other error is `None`, and
/// callers fall back to their usual error page.
pub(crate) fn idempotency_notice(error: &crate::ApiError) -> Option<&'static str> {
    match (error.status_code(), error.message()) {
        (StatusCode::CONFLICT, crate::idempotency::KEY_CONFLICT) => {
            Some("This form is already being submitted. Reload the page to see the result.")
        }
        (StatusCode::UNPROCESSABLE_ENTITY, crate::idempotency::KEY_REUSED) => {
            Some("This form was already submitted. Reload the page to start a new one.")
        }
        _ => None,
    }
}

async fn login_page(
    Query(q): Query<LoginQuery>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    match browser_auth(&state, &headers).await {
        Ok(_) => return Redirect::to("/").into_response(),
        Err(e) if e.status_code() != StatusCode::UNAUTHORIZED => {
            return unavailable_page(None).into_response();
        }
        Err(_) => {}
    }
    let (error, credential_invalid, form_describedby) = if q.error.as_deref() == Some("1") {
        (
            "<p class=\"error\" role=\"alert\" id=\"login-error\">Invalid username or password.</p>",
            " aria-invalid=\"true\" aria-describedby=\"login-error\"",
            "",
        )
    } else if q.error.as_deref() == Some("csrf") {
        (
            "<p class=\"error\" role=\"alert\" id=\"csrf-error\">Invalid CSRF token.</p>",
            "",
            " aria-describedby=\"csrf-error\"",
        )
    } else {
        ("", "", "")
    };
    // Reuse a well-formed token so a second sign-in tab does not invalidate the first.
    let token = page_csrf_token(&headers);
    let notice = if q.reset.as_deref() == Some("1") {
        "<p role=\"status\">Password changed. Sign in with your new password.</p>"
    } else {
        ""
    };
    let body = format!(
        "<h1>Sign in</h1><div class=\"card\">{notice}{error}<form method=\"post\" action=\"/login\"{form_describedby}>\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"username\">Username</label><input id=\"username\" name=\"username\" autocomplete=\"username\" required{credential_invalid}>\
<label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required{credential_invalid}>\
<button type=\"submit\">Sign in</button></form></div>\
<p><a href=\"/reset\">Have a reset code?</a></p>",
        notice = notice,
        error = error,
        csrf = esc(&token),
        form_describedby = form_describedby
    );
    let cookie = csrf_cookie_header(&state, &token);
    let mut response = Html(render_page("Sign in", None, None, false, &body)).into_response();
    if let Ok(value) = cookie.parse() {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    response
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(input): Form<LoginForm>,
) -> Result<Response, UiError> {
    let cookie_token = csrf_cookie(&headers).unwrap_or_default();
    if !csrf_matches(&input.csrf_token, &cookie_token) {
        return Err(forbidden_page());
    }
    // Only a definite mismatch is reported as bad credentials; an outage must not look like one.
    let id = match crate::sign_in(
        &state,
        &input.username,
        &input.password,
        crate::login_throttle::Request::from_current(),
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(Redirect::to("/login?error=1").into_response()),
        Err(e) if e.status_code() == StatusCode::TOO_MANY_REQUESTS => {
            // A failed-sign-in throttle and an overloaded hasher are different
            // situations and get different pages.
            return Err(if e.message() == crate::too_many_attempts().message() {
                too_many_attempts_page()
            } else {
                busy_page()
            });
        }
        Err(_) => return Err(unavailable_page(None)),
    };
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let token = hex::encode(bytes);
    use sha2::{Digest, Sha256};
    let token_hash = Sha256::digest(token.as_bytes()).to_vec();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|_| unavailable_page(None))?;
    if sqlx::query(
        "INSERT INTO sessions (token_hash, account_id, expires_at) VALUES ($1,$2,clock_timestamp() + $3 * interval '1 second')",
    )
    .bind(&token_hash)
    .bind(id)
    .bind(state.config.token_ttl_seconds as f64)
    .execute(&mut *tx)
    .await
    .is_err()
    {
        return Err(unavailable_page(None));
    }
    if crate::event(&mut tx, None, id, "session.created")
        .await
        .is_err()
    {
        return Err(unavailable_page(None));
    }
    if tx.commit().await.is_err() {
        return Err(unavailable_page(None));
    }
    let mut response = Redirect::to("/").into_response();
    for cookie in [
        set_session_cookie(&state, &token),
        clear_csrf_cookie(&state),
    ] {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    Ok(response)
}

/// The fixed refusal text the public reset page shows, regardless of whether
/// the code was wrong, used or expired, so the page never becomes an oracle.
fn reset_notice(error: &crate::ApiError) -> &'static str {
    match error.status_code() {
        StatusCode::UNAUTHORIZED => "Invalid or expired reset code.",
        StatusCode::BAD_REQUEST => "Use a password of 12 to 1024 characters.",
        StatusCode::TOO_MANY_REQUESTS => {
            "Too many password changes are in progress. Try again in a moment."
        }
        _ => "The password could not be changed. Try again.",
    }
}

fn reset_page_html(csrf: &str, username: &str, error: &str) -> String {
    format!(
        "<h1>Reset your password</h1>\
<p>Enter the one-time code an administrator gave you and choose a new password.</p>{error}\
<div class=\"card\"><form method=\"post\" action=\"/reset\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"reset_username\">Username</label>\
<input id=\"reset_username\" name=\"username\" autocomplete=\"username\" required value=\"{username}\">\
<label for=\"reset_code\">Reset code</label>\
<input id=\"reset_code\" name=\"code\" autocomplete=\"one-time-code\" required minlength=\"64\" maxlength=\"64\">\
<label for=\"reset_password\">New password</label>\
<input id=\"reset_password\" name=\"new_password\" type=\"password\" autocomplete=\"new-password\" required minlength=\"12\" maxlength=\"1024\">\
<p><button type=\"submit\">Set new password</button></p></form></div>\
<p><a href=\"/login\">Back to sign in</a></p>",
        csrf = esc(csrf),
        username = esc(username),
        error = error,
    )
}

/// A public page for the browser-only account owner: a flagged account cannot
/// sign in, so it must be able to redeem its administrator-issued code without
/// an API client.
async fn reset_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = page_csrf_token(&headers);
    let body = reset_page_html(&token, "", "");
    let cookie = csrf_cookie_header(&state, &token);
    let mut response =
        Html(render_page("Reset password", None, None, false, &body)).into_response();
    if let Ok(value) = cookie.parse() {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    response
}

async fn reset_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> Response {
    let cookie_token = csrf_cookie(&headers).unwrap_or_default();
    if !csrf_matches(&form.csrf_token, &cookie_token) {
        return forbidden_page().into_response();
    }
    let token = page_csrf_token(&headers);
    let password = crate::account_admin::Secret::new(form.new_password);
    match crate::account_admin::redeem_password_reset_inner(
        &state,
        &form.username,
        &form.code,
        password,
    )
    .await
    {
        Ok(()) => {
            let mut response = Redirect::to("/login?reset=1").into_response();
            if let Ok(value) = clear_csrf_cookie(&state).parse() {
                response.headers_mut().append(SET_COOKIE, value);
            }
            response
        }
        Err(error) => {
            let status = error.status_code();
            let error_html = format!(
                "<p class=\"error\" role=\"alert\">{}</p>",
                esc(reset_notice(&error))
            );
            let body = reset_page_html(&token, &form.username, &error_html);
            (
                status,
                Html(render_page("Reset password", None, None, false, &body)),
            )
                .into_response()
        }
    }
}

async fn logout_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LogoutForm>,
) -> Result<Response, UiError> {
    let token_hash = browser_token_hash(&headers).ok_or_else(auth_required)?;
    let supplied = if form.csrf_token.is_empty() {
        headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    } else {
        form.csrf_token.clone()
    };
    if !csrf_matches(&supplied, &csrf_expected(&token_hash)) {
        return Err(forbidden_page());
    }
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(signed_out(&state)),
        Err(_) => return Err(unavailable_page(None)),
    };
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    if sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
        .bind(&token_hash)
        .execute(&mut *tx)
        .await
        .is_err()
    {
        return Err(unavailable_page(Some(&account.username)));
    }
    if crate::event(&mut tx, None, account.id, "session.revoked")
        .await
        .is_err()
    {
        return Err(unavailable_page(Some(&account.username)));
    }
    if tx.commit().await.is_err() {
        return Err(unavailable_page(Some(&account.username)));
    }
    Ok(signed_out(&state).into_response())
}

async fn home(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, UiError> {
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    let rows = sqlx::query(
        "SELECT slug, name FROM organizations o JOIN memberships m ON m.organization_id=o.id JOIN accounts a ON a.id=m.account_id WHERE m.account_id=$1 AND a.suspended=false AND o.suspended=false ORDER BY o.slug",
    )
    .bind(account.id)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| unavailable_page(Some(&account.username)))?;
    let mut items = String::new();
    if rows.is_empty() {
        items.push_str("<li>No organizations yet.</li>");
    } else {
        for row in rows {
            let slug: String = row.get("slug");
            let name: String = row.get("name");
            items.push_str(&format!(
                "<li><a href=\"/organizations/{}\">{}</a> <span class=\"muted\">({})</span></li>",
                url_encode(&slug),
                esc(&name),
                esc(&slug)
            ));
        }
    }
    let body = format!(
        "<h1>Your organizations</h1><div class=\"card\"><ul>{}</ul></div><p><a href=\"/organizations/new\">New organization</a></p>",
        items
    );
    let hash = browser_token_hash(&headers).unwrap_or_default();
    let csrf = Some(csrf_expected(&hash));
    Ok(Html(render_page(
        "Home",
        Some(&account.username),
        csrf.as_deref(),
        account.is_admin,
        &body,
    ))
    .into_response())
}

async fn organization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, UiError> {
    let (account, auth_age) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
            return Err(auth_required());
        }
        Err(_) => return Err(unavailable_page(None)),
    };
    if !crate::valid_path_param(&slug) {
        return Err(not_found_page(Some(&account.username)));
    }
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    let org_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&slug)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    let org_id = org_id.ok_or_else(|| not_found_page(Some(&account.username)))?;
    let facts = facts::load(&mut conn, account.id, org_id)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?
        .ok_or_else(|| not_found_page(Some(&account.username)))?;
    let org_read = state
        .authz
        .authorize(&facts.request(
            Action::OrganizationRead,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    if !org_read {
        return Err(not_found_page(Some(&account.username)));
    }
    let org_name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id=$1")
        .bind(org_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    // shortcut: reads the first 100 repositories *before* authorization filtering, so a member
    // with access only to later ones sees an empty list; add cursor pagination before large orgs.
    let rows = repositories::list(&mut conn, org_id, None, 100)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    let mut items = String::new();
    let mut any = false;
    for row in rows {
        let authz_repo = repositories::authz_repository(&row);
        let allowed = state
            .authz
            .authorize(&facts.request(
                Action::RepositoryRead,
                Resource::Repository(&authz_repo),
                Some(Duration::from_secs(auth_age)),
            ))
            .is_allowed();
        if !allowed {
            continue;
        }
        any = true;
        items.push_str(&format!(
            "<li><a href=\"/organizations/{}/repositories/{}\">{}</a> <span class=\"muted\">{}</span></li>",
            url_encode(&slug),
            url_encode(&row.name),
            esc(&row.name),
            esc(&row.visibility)
        ));
    }
    if !any {
        items.push_str("<li>No repositories you can read.</li>");
    }
    // A restore is offered only to a viewer the engine permits to restore the
    // specific row. The deleted list itself is never shown to anyone else.
    let deleted = repositories::list_deleted(&mut conn, org_id, 100)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    let csrf_value = csrf_expected(&browser_token_hash(&headers).unwrap_or_default());
    let mut restorable = String::new();
    for row in deleted {
        if !crate::repository_restore::may_restore(&state, &facts, &row, auth_age) {
            continue;
        }
        restorable.push_str(&format!(
            "<li><form method=\"post\" action=\"/organizations/{}/repositories/{}/restore\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<span>{}</span> <button type=\"submit\">Restore</button></form></li>",
            url_encode(&slug),
            url_encode(&row.name),
            esc(&csrf_value),
            esc(&Uuid::new_v4().simple().to_string()),
            esc(&row.name),
        ));
    }
    let deleted_section = if restorable.is_empty() {
        String::new()
    } else {
        format!("<div class=\"card\"><h2>Deleted repositories</h2><ul>{restorable}</ul></div>")
    };
    let can_create = state
        .authz
        .authorize(&facts.request(
            Action::RepositoryCreate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    let new_repo_link = if can_create {
        format!(
            "<p><a href=\"/organizations/{}/repositories/new\">New repository</a></p>",
            url_encode(&slug)
        )
    } else {
        String::new()
    };
    // Owners (and only them) can administer membership, so only they get the
    // link; the page checks the same action again before rendering.
    let can_manage_members = state
        .authz
        .authorize(&facts.request(
            Action::OrganizationManageMembers,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    let members_link = if can_manage_members {
        format!(
            "<p><a href=\"/organizations/{}/members\">Members</a></p>",
            url_encode(&slug)
        )
    } else {
        String::new()
    };
    // Teams and invitations are administered by the same owner-only actions
    // their pages check again before rendering anything.
    let allowed = |action| {
        state
            .authz
            .authorize(&facts.request(
                action,
                Resource::Organization,
                Some(Duration::from_secs(auth_age)),
            ))
            .is_allowed()
    };
    let admin_links = [
        (Action::TeamCreate, "Teams", "teams"),
        (Action::InvitationRead, "Invitations", "invitations"),
    ]
    .into_iter()
    .filter(|(action, _, _)| allowed(*action))
    .map(|(_, label, path)| {
        format!(
            "<p><a href=\"/organizations/{}/{path}\">{label}</a></p>",
            url_encode(&slug)
        )
    })
    .collect::<String>();
    let body = format!(
        "<h1>{}</h1><p class=\"muted\">{}</p>{}<div class=\"card\"><ul>{}</ul></div>{}{}{}",
        esc(&org_name),
        esc(&slug),
        new_repo_link,
        items,
        deleted_section,
        members_link,
        admin_links
    );
    let hash = browser_token_hash(&headers).unwrap_or_default();
    let csrf = Some(csrf_expected(&hash));
    Ok(Html(render_page(
        "Organization",
        Some(&account.username),
        csrf.as_deref(),
        account.is_admin,
        &body,
    ))
    .into_response())
}

/// The SSH clone URL shown next to the HTTP one when SSH is enabled.
fn ssh_clone_url(state: &AppState, headers: &HeaderMap, slug: &str, name: &str) -> Option<String> {
    if !state.config.ssh_enabled {
        return None;
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h.rsplit_once(':').map_or(h, |(h, _)| h))
        .unwrap_or("127.0.0.1");
    Some(format!(
        "ssh://git@{host}:{}/{}/{}.git",
        state.config.ssh_bind.port(),
        url_encode(slug),
        url_encode(name)
    ))
}

/// The absolute HTTP clone URL for a repository. The scheme follows a
/// validated `X-Forwarded-Proto` when present, otherwise the secure-cookie
/// setting; the host comes from the request so a browser sees a copyable URL.
fn absolute_clone_url(state: &AppState, headers: &HeaderMap, slug: &str, name: &str) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase)
        .filter(|p| p == "http" || p == "https")
        .unwrap_or_else(|| {
            if state.config.secure_cookies {
                "https".to_owned()
            } else {
                "http".to_owned()
            }
        });
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| state.config.bind.to_string());
    format!(
        "{scheme}://{host}/{}/{}.git",
        url_encode(slug),
        url_encode(name)
    )
}

#[derive(Deserialize)]
struct NewOrgForm {
    #[serde(default)]
    csrf_token: String,
    slug: String,
    name: String,
}

#[derive(Deserialize)]
struct RestoreForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

#[derive(Deserialize)]
struct NewRepoForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    visibility: String,
}

fn form_key() -> String {
    Uuid::new_v4().simple().to_string()
}

fn error_form_page(username: &str, status: StatusCode, message: &str, body: &str) -> Response {
    let body = format!(
        "<h1>{}</h1><p class=\"error\" role=\"alert\">{}</p>{}",
        esc(message),
        esc(message),
        body
    );
    (
        status,
        Html(render_page(
            "Invalid request",
            Some(username),
            None,
            false,
            &body,
        )),
    )
        .into_response()
}

fn organization_form(csrf: &str, slug: &str, name: &str) -> String {
    format!(
        "<form class=\"card\" method=\"post\" action=\"/organizations/new\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"slug\">Slug</label><input id=\"slug\" name=\"slug\" required maxlength=\"64\" value=\"{}\" aria-describedby=\"slug-help\">\
<p class=\"muted\" id=\"slug-help\">Lowercase letters, digits and single hyphens.</p>\
<label for=\"name\">Name</label><input id=\"name\" name=\"name\" required maxlength=\"200\" value=\"{}\" placeholder=\"Acme Corporation\">\
<p><button type=\"submit\">Create organization</button></p></form>",
        esc(slug),
        esc(name)
    )
}

async fn new_organization_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, UiError> {
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    let csrf = csrf_expected(&browser_token_hash(&headers).unwrap_or_default());
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / New organization</nav>\
<h1>New organization</h1>{}",
        organization_form(&csrf, "", "")
    );
    Ok(Html(render_page(
        "New organization",
        Some(&account.username),
        Some(&csrf),
        account.is_admin,
        &body,
    ))
    .into_response())
}

async fn new_organization_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewOrgForm>,
) -> Result<Response, UiError> {
    let token_hash = browser_token_hash(&headers).ok_or_else(auth_required)?;
    if !csrf_matches(&form.csrf_token, &csrf_expected(&token_hash)) {
        return Err(forbidden_page());
    }
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    let token = browser_token(&headers).ok_or_else(auth_required)?;
    let mut api_headers = HeaderMap::new();
    api_headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| unavailable_page(Some(&account.username)))?,
    );
    match crate::create_organization(
        State(state.clone()),
        api_headers,
        axum::Json(crate::NewOrganization {
            slug: form.slug.clone(),
            name: form.name.clone(),
        }),
    )
    .await
    {
        Ok((_, axum::Json(org))) => {
            Ok(Redirect::to(&format!("/organizations/{}", url_encode(&org.slug))).into_response())
        }
        Err(e) => Ok(error_form_page(
            &account.username,
            e.status_code(),
            e.message(),
            &organization_form(&csrf_expected(&token_hash), &form.slug, &form.name),
        )),
    }
}

fn repository_form(
    csrf: &str,
    key: &str,
    name: &str,
    description: &str,
    visibility: &str,
) -> String {
    let selected = |value: &str| {
        if visibility == value { " selected" } else { "" }
    };
    format!(
        "<form class=\"card\" method=\"post\" action=\"\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{key}\">\
<label for=\"name\">Name</label><input id=\"name\" name=\"name\" required maxlength=\"100\" value=\"{}\" aria-describedby=\"name-help\">\
<p class=\"muted\" id=\"name-help\">Letters, digits, dots, underscores and hyphens.</p>\
<label for=\"description\">Description</label><textarea id=\"description\" name=\"description\" rows=\"3\" maxlength=\"1000\">{}</textarea>\
<label for=\"visibility\">Visibility</label><select id=\"visibility\" name=\"visibility\">\
<option value=\"private\"{}>Private</option><option value=\"internal\"{}>Internal</option><option value=\"public\"{}>Public</option></select>\
<p><button type=\"submit\">Create repository</button></p></form>",
        esc(name),
        esc(description),
        selected("private"),
        selected("internal"),
        selected("public")
    )
}

async fn new_repository_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, UiError> {
    let (account, auth_age) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    if !crate::valid_path_param(&slug) {
        return Err(not_found_page(Some(&account.username)));
    }
    // The form is only for an organization the viewer can read and create in.
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?;
    let org_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1 AND NOT suspended")
            .bind(&slug)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|_| unavailable_page(Some(&account.username)))?;
    let Some(org_id) = org_id else {
        return Err(not_found_page(Some(&account.username)));
    };
    let Some(org_facts) = facts::load(&mut conn, account.id, org_id)
        .await
        .map_err(|_| unavailable_page(Some(&account.username)))?
    else {
        return Err(not_found_page(Some(&account.username)));
    };
    drop(conn);
    let can_create = state
        .authz
        .authorize(&org_facts.request(
            Action::RepositoryCreate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed();
    if !can_create {
        return Err(forbidden_page());
    }
    let csrf = csrf_expected(&browser_token_hash(&headers).unwrap_or_default());
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / <a href=\"/organizations/{}\">{}</a> / New repository</nav>\
<h1>New repository</h1>{}",
        url_encode(&slug),
        esc(&slug),
        repository_form(&csrf, &form_key(), "", "", "private")
    );
    Ok(Html(render_page(
        "New repository",
        Some(&account.username),
        Some(&csrf),
        account.is_admin,
        &body,
    ))
    .into_response())
}

async fn new_repository_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Form(form): Form<NewRepoForm>,
) -> Result<Response, UiError> {
    let token_hash = browser_token_hash(&headers).ok_or_else(auth_required)?;
    if !csrf_matches(&form.csrf_token, &csrf_expected(&token_hash)) {
        return Err(forbidden_page());
    }
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    if !idempotency::valid_key(&form.idempotency_key) {
        return Ok(error_form_page(
            &account.username,
            StatusCode::BAD_REQUEST,
            "Reload the page and submit the form again.",
            &repository_form(
                &csrf_expected(&token_hash),
                &form_key(),
                &form.name,
                &form.description,
                &form.visibility,
            ),
        ));
    }
    let token = browser_token(&headers).ok_or_else(auth_required)?;
    let mut api_headers = HeaderMap::new();
    api_headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| unavailable_page(Some(&account.username)))?,
    );
    api_headers.insert(
        "idempotency-key",
        HeaderValue::from_str(&form.idempotency_key)
            .map_err(|_| unavailable_page(Some(&account.username)))?,
    );
    api_headers.insert("content-type", HeaderValue::from_static("application/json"));
    let payload = serde_json::json!({
        "name": form.name,
        "description": form.description,
        "visibility": form.visibility,
    })
    .to_string();
    match crate::repository_api::create(
        State(state.clone()),
        api_headers,
        Path(slug.clone()),
        axum::body::Bytes::from(payload),
    )
    .await
    {
        Ok(_) => Ok(Redirect::to(&format!(
            "/organizations/{}/repositories/{}",
            url_encode(&slug),
            url_encode(&form.name)
        ))
        .into_response()),
        Err(e) => Ok(error_form_page(
            &account.username,
            e.status_code(),
            e.message(),
            &repository_form(
                &csrf_expected(&token_hash),
                &form_key(),
                &form.name,
                &form.description,
                &form.visibility,
            ),
        )),
    }
}

/// Browser submit for restore. It checks the session's CSRF token and then
/// calls the same JSON handler the API uses, so the operation, authorization,
/// idempotency key and audit event are identical on both surfaces.
async fn restore_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<RestoreForm>,
) -> Result<Response, UiError> {
    let token_hash = browser_token_hash(&headers).ok_or_else(auth_required)?;
    if !csrf_matches(&form.csrf_token, &csrf_expected(&token_hash)) {
        return Err(forbidden_page());
    }
    let (account, auth_age) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    if !idempotency::valid_key(&form.idempotency_key) {
        return Ok(error_form_page(
            &account.username,
            StatusCode::BAD_REQUEST,
            "Reload the page and submit the form again.",
            "",
        ));
    }
    let token = browser_token(&headers).ok_or_else(auth_required)?;
    let mut api_headers = HeaderMap::new();
    api_headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| unavailable_page(Some(&account.username)))?,
    );
    api_headers.insert(
        "idempotency-key",
        HeaderValue::from_str(&form.idempotency_key)
            .map_err(|_| unavailable_page(Some(&account.username)))?,
    );
    match repository_api::restore(
        State(state.clone()),
        api_headers,
        Path((slug.clone(), name.clone())),
    )
    .await
    {
        Ok(_) => Ok(Redirect::to(&format!(
            "/organizations/{}/repositories/{}",
            url_encode(&slug),
            url_encode(&name)
        ))
        .into_response()),
        Err(e) if e.status_code() == StatusCode::GONE => Err(repository_gone_page(
            &state,
            &headers,
            Some(&account),
            Some(auth_age),
            &slug,
            &name,
        )
        .await),
        Err(e) if e.message() == "reauthentication required" => Err(reauthentication_page(
            Some(&account.username),
            account.is_admin,
            &csrf_expected(&token_hash),
            state.fresh_auth(),
            &format!(
                "/organizations/{}/repositories/{}",
                url_encode(&slug),
                url_encode(&name)
            ),
        )),
        Err(e) => {
            let back = format!(
                "<p><a href=\"/organizations/{}/repositories/{}\">Back to repository</a></p>",
                url_encode(&slug),
                url_encode(&name)
            );
            Ok(error_form_page(
                &account.username,
                e.status_code(),
                e.message(),
                &back,
            ))
        }
    }
}

async fn repository(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, UiError> {
    if path_moved_away(&state, &slug, &name).await {
        let username = browser_auth(&state, &headers)
            .await
            .ok()
            .map(|(account, _)| account.username);
        return Err(moved_away_page(username.as_deref()));
    }
    let (repo, username, csrf, account_id, auth_age, is_admin) =
        match browser_auth(&state, &headers).await {
            Ok((account, auth_age)) => {
                let loaded =
                    match repository_api::load_repo(&state, account.id, auth_age, &slug, &name)
                        .await
                    {
                        Ok(loaded) => loaded,
                        Err(e) if e.status_code() == StatusCode::GONE => {
                            return Err(repository_gone_page(
                                &state,
                                &headers,
                                Some(&account),
                                Some(auth_age),
                                &slug,
                                &name,
                            )
                            .await);
                        }
                        Err(e) => {
                            return Err(
                                if matches!(
                                    e.status_code(),
                                    StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
                                ) {
                                    not_found_page(Some(&account.username))
                                } else {
                                    unavailable_page(Some(&account.username))
                                },
                            );
                        }
                    };
                if repository_api::require(&state, &loaded, githearth_authz::Action::RepositoryRead)
                    .is_err()
                {
                    return Err(not_found_page(Some(&account.username)));
                }
                let hash = browser_token_hash(&headers).unwrap_or_default();
                (
                    loaded.repo,
                    Some(account.username),
                    Some(csrf_expected(&hash)),
                    Some(account.id),
                    Some(auth_age),
                    account.is_admin,
                )
            }
            Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
                // An anonymous visitor may read public repositories only. The
                // live lookup comes first: a new public repository that reused
                // a deleted name must still resolve, not inherit the old row's
                // `410`. A private deleted row, or a private replacement, is
                // the ordinary sign-in challenge — never a `410` that confirms
                // the name existed.
                match repository_api::load_public_anonymous(&state, &slug, &name).await {
                    Some(repo) => (repo, None, None, None, None, false),
                    None => {
                        if crate::repository_restore::anonymous_gone(&state, &slug, &name).await {
                            return Err(repository_gone_page(
                                &state, &headers, None, None, &slug, &name,
                            )
                            .await);
                        }
                        return Err(auth_required());
                    }
                }
            }
            Err(_) => return Err(unavailable_page(None)),
        };
    let repo = &repo;
    let mut browse_section = String::new();
    if repo.status == githearth_core::RepositoryStatus::Active {
        let storage = state.storage.clone();
        let repo_id = repo.id;
        let timeout = Duration::from_secs(state.config.git_timeout_seconds);
        let refs = match state.git_slots.clone().try_acquire_owned() {
            Ok(permit) => tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let path = storage.read_path(repo_id)?;
                githearth_git::browse::refs(&path, timeout)
            })
            .await
            .ok()
            .and_then(Result::ok),
            Err(_) => None,
        };
        match refs {
            Some(refs) if !refs.branches.is_empty() => {
                let branch = refs
                    .default_branch
                    .clone()
                    .or_else(|| refs.branches.first().map(|b| b.name.clone()))
                    .unwrap_or_else(|| "main".to_owned());
                browse_section = format!(
                    "<p><a href=\"/organizations/{}/repositories/{}/tree/{}\">Browse files</a> · \
<a href=\"/organizations/{}/repositories/{}/commits/{}\">Commit history</a> · \
<a href=\"/organizations/{}/repositories/{}/compare/{}...{}\">Compare</a></p>",
                    url_encode(&slug),
                    url_encode(&repo.name),
                    url_encode(&branch),
                    url_encode(&slug),
                    url_encode(&repo.name),
                    url_encode(&branch),
                    url_encode(&slug),
                    url_encode(&repo.name),
                    url_encode(&branch),
                    url_encode(&branch)
                );
            }
            Some(_) => {
                browse_section = format!(
                    "<p>This repository is empty. Clone it with:</p><pre>git clone {}</pre>",
                    esc(&absolute_clone_url(&state, &headers, &slug, &repo.name))
                );
            }
            None => {}
        }
    }
    // Only viewers who may manage tag protection get the settings link; a
    // read-only visitor would only reach a page whose forms return 403.
    let can_manage_protection = match (account_id, auth_age) {
        (Some(account_id), Some(auth_age)) => match state.pool.acquire().await {
            Ok(mut conn) => match facts::load(&mut conn, account_id, repo.organization_id).await {
                Ok(Some(facts)) => {
                    let authz_repo = repositories::authz_repository(repo);
                    state
                        .authz
                        .authorize(&facts.request(
                            Action::RepositoryUpdate,
                            Resource::Repository(&authz_repo),
                            Some(Duration::from_secs(auth_age)),
                        ))
                        .is_allowed()
                }
                _ => false,
            },
            Err(_) => false,
        },
        _ => false,
    };
    let protection_links = if can_manage_protection {
        format!(
            " · <a href=\"/organizations/{}/repositories/{}/settings/branches\">Branch protection</a> · \
<a href=\"/organizations/{}/repositories/{}/settings/tags\">Tag protection</a>",
            url_encode(&slug),
            url_encode(&repo.name),
            url_encode(&slug),
            url_encode(&repo.name)
        )
    } else {
        String::new()
    };
    browse_section.push_str(&format!(
        "<p><a href=\"/organizations/{}/repositories/{}/issues\">Issues</a> · <a href=\"/organizations/{}/repositories/{}/pulls\">Pull requests</a>{protection_links}</p>",
        url_encode(&slug),
        url_encode(&repo.name),
        url_encode(&slug),
        url_encode(&repo.name),
    ));
    // A fork is offered to anyone who can read the repository and can create
    // repositories in at least one (non-suspended) organization they belong to.
    let fork_section = match (account_id, auth_age) {
        (Some(account_id), Some(auth_age))
            if repo.status == githearth_core::RepositoryStatus::Active =>
        {
            let org_rows = sqlx::query(
                "SELECT o.slug, o.id FROM organizations o JOIN memberships m ON m.organization_id = o.id \
                 WHERE m.account_id = $1 AND NOT o.suspended ORDER BY o.slug",
            )
            .bind(account_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|_| unavailable_page(username.as_deref()))?;
            let mut options = String::new();
            for row in &org_rows {
                let org_slug: String = row.get("slug");
                let org_id: Uuid = row.get("id");
                // Forking needs `repository.create` on the target namespace, so a
                // read-only member is never offered a namespace they cannot write to.
                let mut conn = state
                    .pool
                    .acquire()
                    .await
                    .map_err(|_| unavailable_page(username.as_deref()))?;
                let Some(facts) = facts::load(&mut conn, account_id, org_id)
                    .await
                    .map_err(|_| unavailable_page(username.as_deref()))?
                else {
                    continue;
                };
                drop(conn);
                let create = state.authz.authorize(&facts.request(
                    Action::RepositoryCreate,
                    Resource::Organization,
                    Some(Duration::from_secs(auth_age)),
                ));
                if !create.is_allowed() {
                    continue;
                }
                options.push_str(&format!(
                    "<option value=\"{}\">{}</option>",
                    esc(&org_slug),
                    esc(&org_slug)
                ));
            }
            if options.is_empty() {
                String::new()
            } else {
                format!(
                    "<div class=\"card\"><h2>Fork</h2>\
<form method=\"post\" action=\"/organizations/{}/repositories/{}/fork\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<label for=\"fork_org\">Organization</label><select id=\"fork_org\" name=\"organization\">{}</select>\
<label for=\"fork_name\">Name</label><input id=\"fork_name\" name=\"name\" value=\"{}\">\
<p><button type=\"submit\">Fork repository</button></p></form></div>",
                    url_encode(&slug),
                    url_encode(&repo.name),
                    esc(csrf.as_deref().unwrap_or_default()),
                    esc(&Uuid::new_v4().simple().to_string()),
                    options,
                    esc(&repo.name),
                )
            }
        }
        _ => String::new(),
    };
    // Only name the parent when the viewer can read it (the API applies the
    // same rule in forks.rs); an unreadable parent is omitted entirely.
    let fork_note = match (repo.parent_repository_id, account_id, auth_age) {
        (Some(parent_id), Some(actor_id), Some(age)) => {
            let mut conn = match state.pool.acquire().await {
                Ok(conn) => conn,
                Err(_) => return Err(unavailable_page(username.as_deref())),
            };
            let parent = repositories::get(&mut conn, parent_id)
                .await
                .ok()
                .flatten()
                .filter(|p| p.status != githearth_core::RepositoryStatus::Deleted);
            let parent_slug: Option<String> = match &parent {
                Some(parent) => sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
                    .bind(parent.organization_id)
                    .fetch_optional(&mut *conn)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let readable = match (&parent, &parent_slug) {
                (Some(parent), Some(_)) => facts::load(&mut conn, actor_id, parent.organization_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|f| {
                        state
                            .authz
                            .authorize(&f.request(
                                Action::RepositoryRead,
                                Resource::Repository(&repositories::authz_repository(parent)),
                                Some(Duration::from_secs(age)),
                            ))
                            .is_allowed()
                    }),
                _ => false,
            };
            drop(conn);
            match (parent, parent_slug, readable) {
                (Some(parent), Some(parent_slug), true) => format!(
                    "<p class=\"muted\">Forked from <a href=\"/organizations/{}/repositories/{}\">{}/{}</a></p>",
                    url_encode(&parent_slug),
                    url_encode(&parent.name),
                    esc(&parent_slug),
                    esc(&parent.name)
                ),
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    let http_clone = absolute_clone_url(&state, &headers, &slug, &repo.name);
    let ssh_clone = ssh_clone_url(&state, &headers, &slug, &repo.name);
    let clone_line = match ssh_clone {
        Some(ssh) => format!(
            "<p class=\"muted\">Clone: <code>git clone {}</code> or <code>git clone {}</code></p>",
            esc(&http_clone),
            esc(&ssh)
        ),
        None => format!(
            "<p class=\"muted\">Clone: <code>git clone {}</code></p>",
            esc(&http_clone)
        ),
    };
    let body = format!(
        "<h1>{}</h1>{}{}<div class=\"card\"><p><strong>Organization:</strong> {}</p><p><strong>Visibility:</strong> {}</p><p><strong>Status:</strong> {}</p><p>{}</p>{}</div>{}",
        esc(&repo.name),
        fork_note,
        clone_line,
        esc(&slug),
        esc(&repo.visibility),
        esc(repo.status.as_str()),
        esc(&repo.description),
        browse_section,
        fork_section
    );
    Ok(Html(render_page(
        "Repository",
        username.as_deref(),
        csrf.as_deref(),
        is_admin,
        &body,
    ))
    .into_response())
}

/// Pages carry account data and a sign-out form: keep them out of shared caches and frames.
pub(crate) const PAGE_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'; \
     form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

#[derive(Deserialize)]
struct ForkForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    organization: String,
    #[serde(default)]
    name: String,
}

/// Submits a fork from the repository page. Same authorization and validation
/// as the JSON API, driven by the browser session and CSRF token.
async fn fork_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<ForkForm>,
) -> Result<Response, UiError> {
    let token_hash = browser_token_hash(&headers).ok_or_else(auth_required)?;
    if !csrf_matches(&form.csrf_token, &csrf_expected(&token_hash)) {
        return Err(forbidden_page());
    }
    let (account, auth_age) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
        Err(_) => return Err(unavailable_page(None)),
    };
    let key = if idempotency::valid_key(&form.idempotency_key) {
        &form.idempotency_key
    } else {
        return Err(UiError(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Html(render_page(
                    "Invalid request",
                    Some(&account.username),
                    None,
                    account.is_admin,
                    "<h1>Invalid request</h1><p>Reload the page and submit the form again.</p>",
                )),
            )
                .into_response(),
        )));
    };
    let payload = serde_json::json!({
        "organization": form.organization,
        "name": if form.name.trim().is_empty() { serde_json::Value::Null } else { serde_json::Value::String(form.name.trim().to_owned()) },
    })
    .to_string();
    let result = forks::create_fork(
        &state,
        account.id,
        auth_age,
        &slug,
        &name,
        payload.as_bytes(),
        Some(key),
    )
    .await;
    match result {
        Ok((_, value)) => {
            let fork_name = value["name"].as_str().unwrap_or(&name);
            Ok(Redirect::to(&format!(
                "/organizations/{}/repositories/{}",
                url_encode(&form.organization),
                url_encode(fork_name)
            ))
            .into_response())
        }
        Err(e) => Err(fork_error(&account.username, e)),
    }
}

/// Maps a fork-submit failure to the page a browser should see.
fn fork_error(username: &str, error: crate::ApiError) -> UiError {
    let (status, message) = match error.status_code() {
        StatusCode::NOT_FOUND => (StatusCode::NOT_FOUND, "The repository no longer exists."),
        StatusCode::FORBIDDEN => (
            StatusCode::FORBIDDEN,
            "You do not have permission to fork this repository.",
        ),
        StatusCode::CONFLICT => (StatusCode::CONFLICT, error.message()),
        StatusCode::BAD_REQUEST => (StatusCode::BAD_REQUEST, "Check the form and try again."),
        _ => return unavailable_page(Some(username)),
    };
    UiError(Box::new(
        (
            status,
            Html(render_page(
                "Fork",
                Some(username),
                None,
                false,
                &format!("<h1>Fork failed</h1><p>{}</p>", esc(message)),
            )),
        )
            .into_response(),
    ))
}

async fn page_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Pages use no JavaScript and only inline style attributes (#87).
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(PAGE_CSP));
    response
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/reset", get(reset_page).post(reset_submit))
        .route("/logout", axum::routing::post(logout_submit))
        .route("/", get(home))
        .route(
            "/organizations/new",
            get(new_organization_page).post(new_organization_submit),
        )
        .route("/organizations/{slug}", get(organization))
        .route(
            "/organizations/{slug}/repositories/new",
            get(new_repository_page).post(new_repository_submit),
        )
        .route("/organizations/{slug}/repositories", get(organization))
        .route("/organizations/{slug}/repositories/{name}", get(repository))
        .route(
            "/organizations/{slug}/repositories/{name}/fork",
            axum::routing::post(fork_submit),
        )
        .route(
            "/organizations/{slug}/repositories/{name}/restore",
            axum::routing::post(restore_submit),
        )
        .merge(crate::issues_ui::router())
        .merge(crate::pulls_ui::router())
        .merge(crate::browse_ui::router())
        .merge(crate::settings_ui::router())
        .merge(crate::tag_protection_ui::router())
        .merge(crate::branch_protection_ui::router())
        .merge(crate::notifications_ui::router())
        .merge(crate::account_admin_ui::router())
        .merge(crate::membership_ui::router())
        .fallback(fallback_page)
        .layer(axum::middleware::map_response(page_headers))
}

#[cfg(test)]
mod tests {
    use super::render_page;

    #[test]
    fn render_page_declares_lang_skip_link_and_landmarks() {
        let html = render_page("Sign in", None, None, false, "<h1>Sign in</h1>");
        assert!(
            html.contains("<html lang=\"en\">"),
            "missing lang attribute"
        );
        assert!(
            html.contains("<a class=\"skip-link\" href=\"#main\">Skip to content</a>"),
            "missing skip link"
        );
        assert!(
            html.contains("<main id=\"main\" tabindex=\"-1\">"),
            "missing labelled main landmark"
        );
        assert!(
            html.contains("<nav aria-label=\"Primary\">"),
            "missing labelled primary navigation"
        );
    }

    #[test]
    fn render_page_ships_focus_visible_styles() {
        let html = render_page("Sign in", None, None, false, "");
        assert!(html.contains("a:focus-visible,button:focus-visible,input:focus-visible,textarea:focus-visible,select:focus-visible"));
        assert!(html.contains("outline:3px solid #005fcc"));
        assert!(html.contains(".skip-link:focus-visible"));
    }

    #[test]
    fn render_page_wraps_long_text_and_scrolls_wide_tables() {
        let html = render_page("Sign in", None, None, false, "");
        assert!(html.contains("overflow-wrap:anywhere"), "{html}");
        assert!(
            html.contains(".card{") && html.contains("overflow-x:auto"),
            "{html}"
        );
    }

    #[test]
    fn render_page_does_not_stretch_checkboxes() {
        let html = render_page("Sign in", None, None, false, "");
        assert!(
            html.contains("input:not([type=checkbox]):not([type=radio])"),
            "{html}"
        );
        assert!(
            html.contains("input[type=checkbox],input[type=radio]{width:auto"),
            "{html}"
        );
    }
}
