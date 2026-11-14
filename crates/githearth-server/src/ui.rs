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
    verify_password,
};

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    csrf_token: String,
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

pub(crate) fn browser_token_hash(headers: &HeaderMap) -> Option<Vec<u8>> {
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
    use sha2::{Digest, Sha256};
    Some(Sha256::digest(token.as_bytes()).to_vec())
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
    body: &str,
) -> String {
    let nav = if let Some(name) = username {
        if let Some(token) = csrf {
            format!(
                "<nav aria-label=\"Primary\"><a href=\"/\">GitHearth</a><span class=\"muted\" style=\"color:#ccc\">{}</span>\
<a href=\"/notifications\" style=\"margin-left:auto\">Notifications</a>\
<a href=\"/settings\">Settings</a>\
<form method=\"post\" action=\"/logout\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Sign out</button></form></nav>",
                esc(name),
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

pub(crate) fn not_found_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::NOT_FOUND,
            Html(render_page(
                "Not found",
                username,
                None,
                "<h1>Not found</h1>",
            )),
        )
            .into_response(),
    ))
}

pub(crate) fn unavailable_page(username: Option<&str>) -> UiError {
    UiError(Box::new(
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render_page(
                "Unavailable",
                username,
                None,
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
                "<h1>Busy</h1><p>Too many sign-ins in progress. Try again in a moment.</p>",
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
                "<h1>Forbidden</h1><p>Invalid CSRF token.</p>",
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
    let token = csrf_cookie(&headers)
        .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            let mut bytes = [0u8; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
            hex::encode(bytes)
        });
    let body = format!(
        "<h1>Sign in</h1><div class=\"card\">{error}<form method=\"post\" action=\"/login\"{form_describedby}>\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"username\">Username</label><input id=\"username\" name=\"username\" autocomplete=\"username\" required{credential_invalid}>\
<label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required{credential_invalid}>\
<button type=\"submit\">Sign in</button></form></div>",
        error = error,
        csrf = esc(&token),
        form_describedby = form_describedby
    );
    let secure = if state.config.secure_cookies {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "githearth_csrf={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{secure}",
        ttl = state.config.token_ttl_seconds
    );
    let mut response = Html(render_page("Sign in", None, None, &body)).into_response();
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
    let id = match verify_password(&state, &input.username, &input.password).await {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(Redirect::to("/login?error=1").into_response()),
        Err(e) if e.status_code() == StatusCode::TOO_MANY_REQUESTS => return Err(busy_page()),
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
        "<h1>Your organizations</h1><div class=\"card\"><ul>{}</ul></div>",
        items
    );
    let hash = browser_token_hash(&headers).unwrap_or_default();
    let csrf = Some(csrf_expected(&hash));
    Ok(Html(render_page(
        "Home",
        Some(&account.username),
        csrf.as_deref(),
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
    let body = format!(
        "<h1>{}</h1><p class=\"muted\">{}</p><div class=\"card\"><ul>{}</ul></div>",
        esc(&org_name),
        esc(&slug),
        items
    );
    let hash = browser_token_hash(&headers).unwrap_or_default();
    let csrf = Some(csrf_expected(&hash));
    Ok(Html(render_page(
        "Organization",
        Some(&account.username),
        csrf.as_deref(),
        &body,
    ))
    .into_response())
}

async fn repository(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, UiError> {
    let (repo, username, csrf, account_id, auth_age) = match browser_auth(&state, &headers).await {
        Ok((account, auth_age)) => {
            let loaded = repository_api::load_repo(&state, account.id, auth_age, &slug, &name)
                .await
                .map_err(|e| {
                    if e.status_code() == StatusCode::NOT_FOUND {
                        not_found_page(Some(&account.username))
                    } else {
                        unavailable_page(Some(&account.username))
                    }
                })?;
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
            )
        }
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
            // An anonymous visitor may read public repositories only.
            match repository_api::load_public_anonymous(&state, &slug, &name).await {
                Some(repo) => (repo, None, None, None, None),
                None => return Err(auth_required()),
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
<a href=\"/organizations/{}/repositories/{}/compare/{}...{}\">Compare</a></p>",
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
                    "<p>This repository is empty. Clone it with:</p><pre>git clone /{}/{}.git</pre>",
                    url_encode(&slug),
                    url_encode(&repo.name)
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
    let tag_link = if can_manage_protection {
        format!(
            " · <a href=\"/organizations/{}/repositories/{}/settings/tags\">Tag protection</a>",
            url_encode(&slug),
            url_encode(&repo.name)
        )
    } else {
        String::new()
    };
    browse_section.push_str(&format!(
        "<p><a href=\"/organizations/{}/repositories/{}/issues\">Issues</a> · <a href=\"/organizations/{}/repositories/{}/pulls\">Pull requests</a>{tag_link}</p>",
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
    let body = format!(
        "<h1>{}</h1><div class=\"card\"><p><strong>Organization:</strong> {}</p><p><strong>Visibility:</strong> {}</p><p><strong>Status:</strong> {}</p><p>{}</p>{}</div>{}",
        esc(&repo.name),
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
        .route("/logout", axum::routing::post(logout_submit))
        .route("/", get(home))
        .route("/organizations/{slug}", get(organization))
        .route("/organizations/{slug}/repositories", get(organization))
        .route("/organizations/{slug}/repositories/{name}", get(repository))
        .route(
            "/organizations/{slug}/repositories/{name}/fork",
            axum::routing::post(fork_submit),
        )
        .merge(crate::issues_ui::router())
        .merge(crate::pulls_ui::router())
        .merge(crate::browse_ui::router())
        .merge(crate::settings_ui::router())
        .merge(crate::tag_protection_ui::router())
        .merge(crate::notifications_ui::router())
        .layer(axum::middleware::map_response(page_headers))
}

#[cfg(test)]
mod tests {
    use super::render_page;

    #[test]
    fn render_page_declares_lang_skip_link_and_landmarks() {
        let html = render_page("Sign in", None, None, "<h1>Sign in</h1>");
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
        let html = render_page("Sign in", None, None, "");
        assert!(html.contains("a:focus-visible,button:focus-visible,input:focus-visible,textarea:focus-visible,select:focus-visible"));
        assert!(html.contains("outline:3px solid #005fcc"));
        assert!(html.contains(".skip-link:focus-visible"));
    }

    #[test]
    fn render_page_wraps_long_text_and_scrolls_wide_tables() {
        let html = render_page("Sign in", None, None, "");
        assert!(html.contains("overflow-wrap:anywhere"), "{html}");
        assert!(
            html.contains(".card{") && html.contains("overflow-x:auto"),
            "{html}"
        );
    }

    #[test]
    fn render_page_does_not_stretch_checkboxes() {
        let html = render_page("Sign in", None, None, "");
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
