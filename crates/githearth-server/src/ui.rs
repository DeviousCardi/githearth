use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::SET_COOKIE},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use githearth_authz::{Action, Resource};
use serde::Deserialize;
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AppState, authenticate_token_hash, facts, repositories, repository_api, verify_password,
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

struct UiError(Box<Response>);
impl axum::response::IntoResponse for UiError {
    fn into_response(self) -> Response {
        *self.0
    }
}

fn esc(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn url_encode(value: &str) -> String {
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

fn browser_token_hash(headers: &HeaderMap) -> Option<Vec<u8>> {
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

async fn browser_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(crate::Account, u64), crate::ApiError> {
    let hash = browser_token_hash(headers).ok_or_else(crate::unauthorized)?;
    authenticate_token_hash(&state.pool, hash).await
}

fn csrf_expected(token_hash: &[u8]) -> String {
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

fn render_page(title: &str, username: Option<&str>, csrf: Option<&str>, body: &str) -> String {
    let nav = if let Some(name) = username {
        if let Some(token) = csrf {
            format!(
                "<nav><a href=\"/\">GitHearth</a><span class=\"muted\" style=\"color:#ccc\">{}</span>\
<form method=\"post\" action=\"/logout\" style=\"margin-left:auto;display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Sign out</button></form></nav>",
                esc(name),
                esc(token)
            )
        } else {
            format!(
                "<nav><a href=\"/\">GitHearth</a><span class=\"muted\" style=\"color:#ccc\">{}</span></nav>",
                esc(name)
            )
        }
    } else {
        "<nav><a href=\"/\">GitHearth</a><a href=\"/login\" style=\"margin-left:auto\">Sign in</a></nav>"
            .to_string()
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{title}</title><style>\
body{{font-family:system-ui,sans-serif;margin:0;background:#f7f7f8;color:#111}}\
main{{max-width:900px;margin:2rem auto;padding:0 1rem}}\
nav{{background:#111;color:#fff;padding:.75rem 1rem;display:flex;gap:1rem;align-items:center}}\
nav a{{color:#fff;text-decoration:none}}\
.card{{background:#fff;border:1px solid #ddd;border-radius:8px;padding:1rem;margin:1rem 0}}\
label{{display:block;margin:.5rem 0 .25rem}}\
input{{padding:.5rem;width:100%;box-sizing:border-box}}\
button{{padding:.5rem 1rem;cursor:pointer}}\
.error{{color:#b00020;margin:.5rem 0}}\
.muted{{color:#666;font-size:.9rem}}\
ul{{list-style:none;padding:0}}\
li{{padding:.4rem 0;border-bottom:1px solid #eee}}\
</style></head><body>{nav}<main>{body}</main></body></html>",
        title = esc(title),
        nav = nav,
        body = body,
    )
}

fn auth_required() -> UiError {
    UiError(Box::new(Redirect::to("/login").into_response()))
}

fn not_found_page(username: Option<&str>) -> UiError {
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

fn unavailable_page(username: Option<&str>) -> UiError {
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

fn forbidden_page() -> UiError {
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
    let error = if q.error.as_deref() == Some("1") {
        "<p class=\"error\" role=\"alert\">Invalid username or password.</p>"
    } else if q.error.as_deref() == Some("csrf") {
        "<p class=\"error\" role=\"alert\">Invalid CSRF token.</p>"
    } else {
        ""
    };
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let token = hex::encode(bytes);
    let body = format!(
        "<h1>Sign in</h1><div class=\"card\">{error}<form method=\"post\" action=\"/login\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<label for=\"username\">Username</label><input id=\"username\" name=\"username\" autocomplete=\"username\" required>\
<label for=\"password\">Password</label><input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" required>\
<button type=\"submit\">Sign in</button></form></div>",
        error = error,
        csrf = esc(&token)
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
    let cookie_token = csrf_cookie(&headers);
    if cookie_token.is_none()
        || cookie_token.as_deref() != Some(input.csrf_token.as_str())
        || input.csrf_token.is_empty()
    {
        return Err(forbidden_page());
    }
    let id = match verify_password(&state, &input.username, &input.password).await {
        Ok(Some(id)) => id,
        _ => return Ok(Redirect::to("/login?error=1").into_response()),
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
        .map_err(|_| UiError(Box::new(Redirect::to("/login?error=1").into_response())))?;
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
        return Ok(Redirect::to("/login?error=1").into_response());
    }
    if crate::event(&mut tx, None, id, "session.created")
        .await
        .is_err()
    {
        return Ok(Redirect::to("/login?error=1").into_response());
    }
    if tx.commit().await.is_err() {
        return Ok(Redirect::to("/login?error=1").into_response());
    }
    let mut response = Redirect::to("/").into_response();
    let cookie = set_session_cookie(&state, &token);
    if let Ok(value) = cookie.parse() {
        response.headers_mut().insert(SET_COOKIE, value);
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
    let expected = csrf_expected(&token_hash);
    if supplied != expected {
        return Err(forbidden_page());
    }
    let (account, _) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(auth_required()),
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
    let mut response = Redirect::to("/login").into_response();
    let cookie = clear_session_cookie(&state);
    if let Ok(value) = cookie.parse() {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    Ok(response)
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
    // shortcut: navigation loads only the first 100 repositories; cursor pagination is deferred to a later increment.
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
    let (account, auth_age) = match browser_auth(&state, &headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
            return Err(auth_required());
        }
        Err(_) => return Err(unavailable_page(None)),
    };
    let loaded = repository_api::load_repo(&state, account.id, auth_age, &slug, &name)
        .await
        .map_err(|e| {
            if e.status_code() == StatusCode::NOT_FOUND {
                not_found_page(Some(&account.username))
            } else {
                unavailable_page(Some(&account.username))
            }
        })?;
    if repository_api::require(&state, &loaded, githearth_authz::Action::RepositoryRead).is_err() {
        return Err(not_found_page(Some(&account.username)));
    }
    let repo = &loaded.repo;
    let body = format!(
        "<h1>{}</h1><div class=\"card\"><p><strong>Organization:</strong> {}</p><p><strong>Visibility:</strong> {}</p><p><strong>Status:</strong> {}</p><p>{}</p></div>",
        esc(&repo.name),
        esc(&slug),
        esc(&repo.visibility),
        esc(repo.status.as_str()),
        esc(&repo.description)
    );
    let hash = browser_token_hash(&headers).unwrap_or_default();
    let csrf = Some(csrf_expected(&hash));
    Ok(Html(render_page(
        "Repository",
        Some(&account.username),
        csrf.as_deref(),
        &body,
    ))
    .into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", axum::routing::post(logout_submit))
        .route("/", get(home))
        .route("/organizations/{slug}", get(organization))
        .route("/organizations/{slug}/repositories", get(organization))
        .route("/organizations/{slug}/repositories/{name}", get(repository))
}
