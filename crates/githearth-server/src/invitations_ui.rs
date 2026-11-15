//! Server-rendered invitations page: list and create the organization's
//! invitations.
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and a per-form idempotency key (validated, though creation is not
//! replayed), and the writes go through the same `pub(crate)` functions as the
//! JSON API, so the `invitation.*` decisions, validation and audit events are
//! identical. Only a caller the engine permits `invitation.read` sees the page; a
//! reader of the organization gets "not permitted", anyone else the masked 404.
//! An invitation's token is shown once, on the response that created it: the
//! page never queries it back, because no row stores the plaintext. Creation is
//! deliberately not replayed, so a repeated form submit creates a second
//! invitation rather than re-showing the first token.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use githearth_authz::Action;
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use crate::{Account, AppState, idempotency, invitations, membership, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/invitations",
            get(invitations_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/invitations/{id}/revoke",
            post(revoke_submit),
        )
}

struct Session {
    account: Account,
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

fn page(s: &Session, status: StatusCode, body: &str) -> Response {
    (
        status,
        Html(ui::render_page(
            "Invitations",
            Some(&s.account.username),
            Some(&s.csrf),
            s.account.is_admin,
            body,
        )),
    )
        .into_response()
}

fn new_key() -> String {
    Uuid::new_v4().simple().to_string()
}

fn form_fields(s: &Session, key: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">",
        ui::esc(&s.csrf),
        ui::esc(key)
    )
}

fn may(state: &AppState, s: &Session, loaded: &membership::Loaded, action: Action) -> bool {
    state
        .authz
        .authorize(
            &loaded
                .facts
                .on_organization(action, Some(Duration::from_secs(s.auth_age))),
        )
        .is_allowed()
}

fn refuse(
    state: &AppState,
    s: &Session,
    loaded: &membership::Loaded,
    action: Action,
) -> ui::UiError {
    match loaded.require(state, action) {
        Err(error) if error.message() == "reauthentication required" => ui::reauthentication_page(
            Some(&s.account.username),
            s.account.is_admin,
            &s.csrf,
            state.fresh_auth(),
            &format!(
                "/organizations/{}/invitations",
                ui::url_encode(&loaded.slug)
            ),
        ),
        Err(error) if error.status_code() == StatusCode::FORBIDDEN => not_permitted_page(s),
        Err(_) => ui::not_found_page(Some(&s.account.username)),
        Ok(()) => not_permitted_page(s),
    }
}

fn not_permitted_page(s: &Session) -> ui::UiError {
    let body =
        "<h1>Not permitted</h1><p>Only an organization owner can administer invitations.</p>";
    ui::UiError::from(page(s, StatusCode::FORBIDDEN, body))
}

fn key_or_page(s: &Session, supplied: &str) -> Result<String, ui::UiError> {
    if idempotency::valid_key(supplied) {
        Ok(supplied.to_owned())
    } else {
        Err(ui::UiError::from(page(
            s,
            StatusCode::BAD_REQUEST,
            "<h1>Invitations</h1><p>Reload the page and submit the form again.</p>",
        )))
    }
}

fn head(slug: &str) -> String {
    format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / \
<a href=\"/organizations/{slug}\">{slug}</a> / Invitations</nav><h1>Invitations</h1>",
        slug = ui::esc(slug)
    )
}

fn role_options(selected: &str) -> String {
    membership::ROLES
        .iter()
        .map(|role| {
            let chosen = if *role == selected { " selected" } else { "" };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                ui::esc(role),
                chosen,
                ui::esc(role)
            )
        })
        .collect()
}

async fn render(
    state: &AppState,
    s: &Session,
    loaded: &membership::Loaded,
    notice: Option<&str>,
    token: Option<(&str, &str)>,
) -> Result<Response, ui::UiError> {
    if !may(state, s, loaded, Action::InvitationRead) {
        return Err(refuse(state, s, loaded, Action::InvitationRead));
    }
    let list = invitations::list_in(state, loaded.org_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let teams = crate::teams::all_teams(state, loaded.org_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = format!(
        "/organizations/{}/invitations",
        ui::url_encode(&loaded.slug)
    );

    let mut rows = String::new();
    for invitation in &list {
        let revoke = if invitation.status == "pending" {
            format!(
                "<form method=\"post\" action=\"{base}/{}/revoke\" style=\"display:inline\">{fields}\
<button type=\"submit\">Revoke</button></form>",
                ui::esc(&invitation.id.to_string()),
                fields = form_fields(s, &new_key()),
            )
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><time datetime=\"{}\">{}</time></td><td>{}</td><td>{revoke}</td></tr>",
            ui::esc(&invitation.email),
            ui::esc(&invitation.role),
            ui::esc(invitation.team.as_deref().unwrap_or("—")),
            ui::esc(&invitation.status),
            ui::esc(&invitation.expires_at),
            ui::esc(&invitation.expires_at),
            if invitation.status == "pending" { "Valid" } else { "—" },
        ));
    }
    let table = if list.is_empty() {
        "<p class=\"muted\">No invitations yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Invitations</caption><thead><tr>\
<th scope=\"col\">Email</th><th scope=\"col\">Role</th><th scope=\"col\">Team</th>\
<th scope=\"col\">Status</th><th scope=\"col\">Expires</th><th scope=\"col\">Actions</th>\
</tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    let team_options: String = teams
        .iter()
        .map(|team| {
            format!(
                "<option value=\"{}\">{}</option>",
                ui::esc(&team.slug),
                ui::esc(&team.name)
            )
        })
        .collect();
    let team_select = if team_options.is_empty() {
        String::new()
    } else {
        format!(
            "<label for=\"invite-team\">Team (optional)</label>\
<select id=\"invite-team\" name=\"team\"><option value=\"\">No team</option>{team_options}</select>"
        )
    };
    let notice = notice
        .map(|n| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(n)))
        .unwrap_or_default();
    let token_box = token
        .map(|(token, href)| {
            format!(
                "<div class=\"card\"><h2>Invitation created</h2>\
<p>The invitation token is shown once. Send this link to the invited address; it will not be shown again.</p>\
<p><code>{token}</code></p><p><a href=\"{href}\">Accept this invitation</a></p></div>",
                token = ui::esc(token),
                href = ui::esc(href),
            )
        })
        .unwrap_or_default();
    let body = format!(
        "{breadcrumb}{notice}{token_box}\
<div class=\"card\">{table}</div>\
<div class=\"card\"><h2>Invite someone</h2>\
<form method=\"post\" action=\"{base}\">{fields}\
<label for=\"invite-email\">Email address</label>\
<input id=\"invite-email\" name=\"email\" type=\"email\" required maxlength=\"320\">\
<label for=\"invite-role\">Organization role</label>\
<select id=\"invite-role\" name=\"role\">{roles}</select>\
{team_select}\
<label for=\"invite-days\">Expires after (days)</label>\
<input id=\"invite-days\" name=\"days\" type=\"number\" min=\"1\" max=\"30\" value=\"{days}\">\
<p><button type=\"submit\">Create invitation</button></p></form></div>",
        breadcrumb = head(&loaded.slug),
        fields = form_fields(s, &new_key()),
        roles = role_options("member"),
        days = invitations::DEFAULT_EXPIRY_DAYS,
    );
    Ok(page(s, StatusCode::OK, &body))
}

async fn invitations_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_page_error(&s, &e))?;
    render(&state, &s, &loaded, None, None).await
}

fn api_page_error(s: &Session, error: &crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => not_permitted_page(s),
        StatusCode::BAD_REQUEST => {
            ui::UiError::from(page(s, StatusCode::BAD_REQUEST, "<h1>Invalid request</h1>"))
        }
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    team: String,
    #[serde(default)]
    days: String,
}

async fn create_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Form(form): Form<CreateForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_page_error(&s, &e))?;
    if !may(&state, &s, &loaded, Action::InvitationCreate) {
        return Err(refuse(&state, &s, &loaded, Action::InvitationCreate));
    }
    // The form still carries a per-form key and it is validated, but creation
    // is not replayed: a second submit creates a second invitation.
    key_or_page(&s, &form.idempotency_key)?;
    let body = serde_json::json!({
        "email": form.email,
        "role": form.role,
        "team": if form.team.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(form.team) },
        "expires_in_days": form.days.parse::<i64>().ok(),
    })
    .to_string();
    match invitations::create(&state, &loaded, body.as_bytes()).await {
        Ok(created) => {
            // The token is shown once, right here; the page that lists
            // invitations never carries it again.
            let href = format!("/invitations/{}", ui::url_encode(&created.token));
            render(&state, &s, &loaded, None, Some((&created.token, &href))).await
        }
        Err(e) if e.message() == "reauthentication required" => {
            Err(refuse(&state, &s, &loaded, Action::InvitationCreate))
        }
        Err(e) => match e.status_code() {
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT => {
                render(&state, &s, &loaded, Some(e.message()), None).await
            }
            StatusCode::NOT_FOUND => Err(ui::not_found_page(Some(&s.account.username))),
            StatusCode::FORBIDDEN => Err(not_permitted_page(&s)),
            _ => Err(ui::unavailable_page(Some(&s.account.username))),
        },
    }
}

#[derive(Deserialize)]
struct RevokeForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn revoke_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, id)): Path<(String, String)>,
    Form(form): Form<RevokeForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_page_error(&s, &e))?;
    if !may(&state, &s, &loaded, Action::InvitationRevoke) {
        return Err(refuse(&state, &s, &loaded, Action::InvitationRevoke));
    }
    let key = key_or_page(&s, &form.idempotency_key)?;
    let id = Uuid::parse_str(&id).map_err(|_| {
        ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "<h1>Invalid request</h1>",
        ))
    })?;
    match invitations::revoke(&state, &loaded, id, Some(&key)).await {
        Ok(()) => Ok(Redirect::to(&format!(
            "/organizations/{}/invitations",
            ui::url_encode(&loaded.slug)
        ))
        .into_response()),
        Err(e) if e.message() == "reauthentication required" => {
            Err(refuse(&state, &s, &loaded, Action::InvitationRevoke))
        }
        Err(e) => match e.status_code() {
            StatusCode::NOT_FOUND => Err(ui::not_found_page(Some(&s.account.username))),
            StatusCode::FORBIDDEN => Err(not_permitted_page(&s)),
            StatusCode::BAD_REQUEST | StatusCode::CONFLICT => {
                render(&state, &s, &loaded, Some(e.message()), None).await
            }
            _ => Err(ui::unavailable_page(Some(&s.account.username))),
        },
    }
}

/// The acceptance page: the building block for case U21's invite → join flow
/// (two accounts, then clone, branch, push, open a pull request, review,
/// merge). It shows the invitations a live token names for the signed-in
/// account's verified address and accepts one.
pub(crate) fn accept_router() -> Router<AppState> {
    Router::new().route("/invitations/{token}", get(accept_page).post(accept_submit))
}

async fn accept_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let offer = invitations::offer(&state, s.account.id, &token).await;
    let Ok(offer) = offer else {
        return Err(declined(&s));
    };
    let mut items = String::new();
    for invitation in &offer.invitations {
        items.push_str(&format!(
            "<li>{} ({}) <span class=\"muted\">expires {}</span></li>",
            ui::esc(&invitation.organization),
            ui::esc(&invitation.role),
            ui::esc(&invitation.expires_at),
        ));
    }
    let body = format!(
        "<h1>Accept an invitation</h1><ul>{items}</ul>\
<form method=\"post\" action=\"/invitations/{token}\">{fields}\
<p><button type=\"submit\">Accept</button></p></form>",
        token = ui::url_encode(&token),
        fields = form_fields(&s, &new_key()),
    );
    Ok(page(&s, StatusCode::OK, &body))
}

/// The one refusal page: no organization name, no slug, nothing an inviter
/// chose. It is the same page for an unknown, foreign, expired, revoked or
/// consumed token.
fn declined(s: &Session) -> ui::UiError {
    ui::UiError::from(page(
        s,
        StatusCode::NOT_FOUND,
        "<h1>Invitation not available</h1><p>This invitation is not available to this account.</p>",
    ))
}

async fn accept_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Form(form): Form<RevokeForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    match invitations::accept_invitation(&state, s.account.id, &token).await {
        Ok(_) => Ok(Redirect::to("/").into_response()),
        Err(_) => Err(declined(&s)),
    }
}
