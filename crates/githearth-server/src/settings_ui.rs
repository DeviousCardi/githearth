//! Server-rendered account settings page: personal access tokens and SSH keys.
//!
//! Pages use the browser session cookie; every form carries the session's
//! CSRF token. All reads and writes go through the same `pub(crate)` functions
//! as the JSON API, so authorization, validation and audit events are identical.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{AppState, idempotency, ssh_keys, tokens, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_page))
        .route("/settings/tokens", axum::routing::post(create_token_submit))
        .route(
            "/settings/tokens/{id}/revoke",
            axum::routing::post(revoke_token_submit),
        )
        .route(
            "/settings/ssh-keys",
            axum::routing::post(create_ssh_key_submit),
        )
        .route(
            "/settings/ssh-keys/{id}/revoke",
            axum::routing::post(revoke_ssh_key_submit),
        )
}

struct Session {
    account: crate::Account,
    csrf: String,
}

async fn session(state: &AppState, headers: &HeaderMap) -> Result<Session, ui::UiError> {
    let (account, _) = match ui::browser_auth(state, headers).await {
        Ok(v) => v,
        Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => return Err(ui::auth_required()),
        Err(_) => return Err(ui::unavailable_page(None)),
    };
    let hash = ui::browser_token_hash(headers).unwrap_or_default();
    Ok(Session {
        account,
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

/// The CSRF token plus a per-form idempotency key.
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
        Err(ui::UiError::from(page(
            s,
            StatusCode::BAD_REQUEST,
            "Settings",
            "<h1>Settings</h1><p>Reload the page and submit the form again.</p>",
        )))
    }
}

/// Parses the optional expiry field: empty means no expiry, otherwise the value
/// must be an integer within the accepted range.
fn parse_days(value: &str) -> Result<Option<i64>, ()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let days: i64 = value.parse().map_err(|_| ())?;
    if !(1..=3650).contains(&days) {
        return Err(());
    }
    Ok(Some(days))
}

fn opt(column: &Option<String>) -> String {
    column
        .as_deref()
        .map(ui::esc)
        .unwrap_or_else(|| "—".to_owned())
}

/// Renders the full settings page, optionally showing a notice and a plaintext
/// token that must be copied now.
#[allow(clippy::too_many_arguments)]
fn render(
    s: &Session,
    tokens: &[tokens::TokenMeta],
    keys: &[ssh_keys::SshKeyMeta],
    status: StatusCode,
    notice: Option<&str>,
    plaintext: Option<&str>,
    token_form: TokenDraft<'_>,
    key_form: KeyDraft<'_>,
) -> Response {
    let notice = notice
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();

    let plaintext_box = plaintext
        .map(|t| {
            format!(
                "<div class=\"card\" role=\"alert\"><p><strong>Your new token</strong></p>\
<pre style=\"white-space:pre-wrap;word-break:break-all\">{}</pre>\
<p class=\"muted\">Copy it now; it won't be shown again.</p></div>",
                ui::esc(t)
            )
        })
        .unwrap_or_default();

    let mut token_rows = String::new();
    for t in tokens {
        let revoked = t.revoked_at.is_some();
        let revoke = if revoked {
            "<span class=\"muted\">Revoked</span>".to_owned()
        } else {
            format!(
                "<form method=\"post\" action=\"/settings/tokens/{}/revoke\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Revoke</button></form>",
                t.id,
                ui::esc(&s.csrf)
            )
        };
        token_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(&t.name),
            ui::esc(&t.scopes.join(", ")),
            ui::esc(&t.created_at),
            opt(&t.expires_at),
            opt(&t.last_used_at),
            opt(&t.revoked_at),
            revoke,
        ));
    }
    let token_table = if tokens.is_empty() {
        "<p class=\"muted\">No access tokens yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Access tokens</caption><thead><tr>\
<th scope=\"col\">Name</th><th scope=\"col\">Scopes</th><th scope=\"col\">Created</th>\
<th scope=\"col\">Expires</th><th scope=\"col\">Last used</th><th scope=\"col\">Revoked</th>\
<th scope=\"col\"></th></tr></thead><tbody>{token_rows}</tbody></table>"
        )
    };

    let mut key_rows = String::new();
    for k in keys {
        let revoked = k.revoked_at.is_some();
        let revoke = if revoked {
            "<span class=\"muted\">Revoked</span>".to_owned()
        } else {
            format!(
                "<form method=\"post\" action=\"/settings/ssh-keys/{}/revoke\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Revoke</button></form>",
                k.id,
                ui::esc(&s.csrf)
            )
        };
        key_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            ui::esc(&k.name),
            ui::esc(&k.algorithm),
            ui::esc(&k.fingerprint),
            ui::esc(&k.created_at),
            opt(&k.expires_at),
            opt(&k.last_used_at),
            revoke,
        ));
    }
    let key_table = if keys.is_empty() {
        "<p class=\"muted\">No SSH keys yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">SSH keys</caption><thead><tr>\
<th scope=\"col\">Name</th><th scope=\"col\">Algorithm</th><th scope=\"col\">Fingerprint</th>\
<th scope=\"col\">Created</th><th scope=\"col\">Expires</th><th scope=\"col\">Last used</th>\
<th scope=\"col\"></th></tr></thead><tbody>{key_rows}</tbody></table>"
        )
    };

    // Token form.
    let token_error = token_form
        .error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let repo_read_checked = if token_form.repo_read { " checked" } else { "" };
    let repo_write_checked = if token_form.repo_write {
        " checked"
    } else {
        ""
    };
    let token_body = format!(
        "<form class=\"card\" method=\"post\" action=\"/settings/tokens\">{}\
<label for=\"token_name\">Name</label>\
<input id=\"token_name\" name=\"name\" required maxlength=\"100\" value=\"{}\">\
<fieldset><legend>Scopes</legend>\
<label><input type=\"checkbox\" name=\"scope_read\" value=\"repo:read\"{}> repo:read</label>\
<label><input type=\"checkbox\" name=\"scope_write\" value=\"repo:write\"{}> repo:write</label>\
</fieldset>\
<label for=\"token_expiry\">Expiry (days, optional)</label>\
<input id=\"token_expiry\" name=\"expires_in_days\" type=\"text\" value=\"{}\">\
{token_error}<p><button type=\"submit\">Create token</button></p></form>",
        form_fields(s, token_form.key),
        ui::esc(token_form.name),
        repo_read_checked,
        repo_write_checked,
        ui::esc(token_form.expiry),
    );

    // SSH key form.
    let key_error = key_form
        .error
        .map(|e| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(e)))
        .unwrap_or_default();
    let key_body = format!(
        "<form class=\"card\" method=\"post\" action=\"/settings/ssh-keys\">{}\
<label for=\"key_name\">Name</label>\
<input id=\"key_name\" name=\"name\" required maxlength=\"100\" value=\"{}\">\
<label for=\"public_key\">Public key</label>\
<textarea id=\"public_key\" name=\"public_key\" rows=\"4\" required \
style=\"width:100%;box-sizing:border-box\">{}</textarea>\
<p class=\"muted\">Only Ed25519 and ECDSA keys are accepted.</p>\
<label for=\"key_expiry\">Expiry (days, optional)</label>\
<input id=\"key_expiry\" name=\"expires_in_days\" type=\"text\" value=\"{}\">\
{key_error}<p><button type=\"submit\">Add SSH key</button></p></form>",
        form_fields(s, key_form.key),
        ui::esc(key_form.name),
        ui::esc(key_form.public_key),
        ui::esc(key_form.expiry),
    );

    let body = format!(
        "<h1>Settings</h1><p><a href=\"/settings/password\">Change password</a></p>\
{plaintext_box}{notice}\
<h2>Access tokens</h2><div class=\"card\">{token_table}</div>{token_body}\
<h2>SSH keys</h2><div class=\"card\">{key_table}</div>{key_body}"
    );
    page(s, status, "Settings", &body)
}

/// Contents of the "Create token" form, kept across a rejected submit.
struct TokenDraft<'a> {
    name: &'a str,
    repo_read: bool,
    repo_write: bool,
    expiry: &'a str,
    key: &'a str,
    error: Option<&'a str>,
}

/// Contents of the "Add SSH key" form, kept across a rejected submit.
struct KeyDraft<'a> {
    name: &'a str,
    public_key: &'a str,
    expiry: &'a str,
    key: &'a str,
    error: Option<&'a str>,
}

async fn render_page(
    state: &AppState,
    s: &Session,
    status: StatusCode,
    notice: Option<&str>,
    plaintext: Option<&str>,
    token_form: TokenDraft<'_>,
    key_form: KeyDraft<'_>,
) -> Result<Response, ui::UiError> {
    let tokens = tokens::list_tokens(state, s.account.id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let keys = ssh_keys::list_ssh_keys(state, s.account.id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    Ok(render(
        s, &tokens, &keys, status, notice, plaintext, token_form, key_form,
    ))
}

async fn settings_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    render_page(
        &state,
        &s,
        StatusCode::OK,
        None,
        None,
        TokenDraft {
            name: "",
            repo_read: true,
            repo_write: false,
            expiry: "",
            key: &new_key(),
            error: None,
        },
        KeyDraft {
            name: "",
            public_key: "",
            expiry: "",
            key: &new_key(),
            error: None,
        },
    )
    .await
}

#[derive(Deserialize)]
struct TokenForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    scope_read: Option<String>,
    #[serde(default)]
    scope_write: Option<String>,
    #[serde(default)]
    expires_in_days: String,
}

impl TokenForm {
    fn scopes(&self) -> Vec<String> {
        let mut scopes = Vec::new();
        if self.scope_read.as_deref() == Some("repo:read") {
            scopes.push("repo:read".to_owned());
        }
        if self.scope_write.as_deref() == Some("repo:write") {
            scopes.push("repo:write".to_owned());
        }
        scopes
    }
}

async fn create_token_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let key = checked_key(&s, &form.idempotency_key)?;
    let expiry = match parse_days(&form.expires_in_days) {
        Ok(days) => days,
        Err(()) => {
            return render_page(
                &state,
                &s,
                StatusCode::BAD_REQUEST,
                Some("Expiry must be a whole number of days between 1 and 3650, or left blank."),
                None,
                form_draft(&form, key),
                empty_key_draft(),
            )
            .await;
        }
    };
    match tokens::create_token(
        &state,
        s.account.id,
        &form.name,
        &form.scopes(),
        expiry,
        Some(key),
    )
    .await
    {
        Ok(Some(created)) => {
            let notice = "Token created.";
            return render_page(
                &state,
                &s,
                StatusCode::OK,
                Some(notice),
                Some(&created.token),
                TokenDraft {
                    name: "",
                    repo_read: true,
                    repo_write: false,
                    expiry: "",
                    key: &new_key(),
                    error: None,
                },
                empty_key_draft(),
            )
            .await;
        }
        Ok(None) => {
            let notice = "This token was already created. Its value was shown once and cannot be shown again.";
            return render_page(
                &state,
                &s,
                StatusCode::OK,
                Some(notice),
                None,
                TokenDraft {
                    name: "",
                    repo_read: true,
                    repo_write: false,
                    expiry: "",
                    key: &new_key(),
                    error: None,
                },
                empty_key_draft(),
            )
            .await;
        }
        Err(e) => {
            let notice = token_notice(&e);
            let scopes = form.scopes();
            return render_page(
                &state,
                &s,
                e.status_code(),
                Some(notice),
                None,
                TokenDraft {
                    name: &form.name,
                    repo_read: scopes.iter().any(|s| s == "repo:read"),
                    repo_write: scopes.iter().any(|s| s == "repo:write"),
                    expiry: &form.expires_in_days,
                    key,
                    error: Some(notice),
                },
                empty_key_draft(),
            )
            .await;
        }
    }
}

async fn revoke_token_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    match tokens::revoke_token(&state, s.account.id, id).await {
        Ok(()) => Ok(Redirect::to("/settings").into_response()),
        Err(e) if e.status_code() == StatusCode::NOT_FOUND => {
            Err(ui::not_found_page(Some(&s.account.username)))
        }
        Err(_) => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}

#[derive(Deserialize)]
struct SshKeyForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    public_key: String,
    #[serde(default)]
    expires_in_days: String,
}

async fn create_ssh_key_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SshKeyForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let key = checked_key(&s, &form.idempotency_key)?;
    let expiry = match parse_days(&form.expires_in_days) {
        Ok(days) => days,
        Err(()) => {
            return render_page(
                &state,
                &s,
                StatusCode::BAD_REQUEST,
                Some("Expiry must be a whole number of days between 1 and 3650, or left blank."),
                None,
                empty_token_draft(),
                KeyDraft {
                    name: &form.name,
                    public_key: &form.public_key,
                    expiry: &form.expires_in_days,
                    key,
                    error: Some(
                        "Expiry must be a whole number of days between 1 and 3650, or left blank.",
                    ),
                },
            )
            .await;
        }
    };
    match ssh_keys::create_ssh_key(&state, s.account.id, &form.name, &form.public_key, expiry).await
    {
        Ok(_) => Ok(Redirect::to("/settings").into_response()),
        Err(e)
            if e.status_code() == StatusCode::BAD_REQUEST
                || e.status_code() == StatusCode::CONFLICT =>
        {
            let notice = if e.status_code() == StatusCode::CONFLICT {
                "This SSH key is already registered to an account."
            } else {
                "Only Ed25519 and ECDSA public keys are accepted."
            };
            render_page(
                &state,
                &s,
                e.status_code(),
                Some(notice),
                None,
                empty_token_draft(),
                KeyDraft {
                    name: &form.name,
                    public_key: &form.public_key,
                    expiry: &form.expires_in_days,
                    key,
                    error: Some(notice),
                },
            )
            .await
        }
        Err(_) => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}

async fn revoke_ssh_key_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    match ssh_keys::revoke_ssh_key(&state, s.account.id, id).await {
        Ok(()) => Ok(Redirect::to("/settings").into_response()),
        Err(e) if e.status_code() == StatusCode::NOT_FOUND => {
            Err(ui::not_found_page(Some(&s.account.username)))
        }
        Err(_) => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

fn form_draft<'a>(form: &'a TokenForm, key: &'a str) -> TokenDraft<'a> {
    let scopes = form.scopes();
    TokenDraft {
        name: &form.name,
        repo_read: scopes.iter().any(|s| s == "repo:read"),
        repo_write: scopes.iter().any(|s| s == "repo:write"),
        expiry: &form.expires_in_days,
        key,
        error: None,
    }
}

fn empty_token_draft() -> TokenDraft<'static> {
    TokenDraft {
        name: "",
        repo_read: true,
        repo_write: false,
        expiry: "",
        key: "",
        error: None,
    }
}

fn empty_key_draft() -> KeyDraft<'static> {
    KeyDraft {
        name: "",
        public_key: "",
        expiry: "",
        key: "",
        error: None,
    }
}

/// The notice a rejected token form shows, keeping the entered name.
fn token_notice(error: &crate::ApiError) -> &'static str {
    match error.status_code() {
        StatusCode::BAD_REQUEST => {
            "A name of 1 to 100 characters and at least one scope are required."
        }
        _ => "The token could not be created. Try again.",
    }
}
