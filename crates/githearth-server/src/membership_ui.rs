//! Server-rendered organization members page: list members, add a member by
//! username, change a role and remove a member.
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and a per-form idempotency key. Reads and writes go through the same
//! `pub(crate)` functions as the JSON API, so authorization, validation and
//! audit events are identical. The page is only rendered for a caller allowed
//! to administer membership; a member who may read the organization gets a
//! "not permitted" page, anyone else the same 404 a missing organization gives.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Account, ApiError, AppState, idempotency, membership, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/members",
            get(members_page).post(add_submit),
        )
        .route(
            "/organizations/{slug}/members/{username}/role",
            axum::routing::post(role_submit),
        )
        .route(
            "/organizations/{slug}/members/{username}/remove",
            axum::routing::post(remove_submit),
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
            s.account.is_admin,
            body,
        )),
    )
        .into_response()
}

fn members_href(slug: &str) -> String {
    format!("/organizations/{}/members", ui::url_encode(slug))
}

fn new_key() -> String {
    Uuid::new_v4().simple().to_string()
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

fn role_options(selected: &str) -> String {
    membership::ROLES
        .iter()
        .map(|role| {
            let checked = if *role == selected { " selected" } else { "" };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                ui::esc(role),
                checked,
                ui::esc(role)
            )
        })
        .collect()
}

/// Maps an API failure to the page a browser should see. A freshness denial
/// keeps the prompt the ADR names; every other 403 keeps the not-permitted
/// page.
fn api_error(state: &AppState, s: &Session, slug: &str, error: &ApiError) -> ui::UiError {
    if error.message() == "reauthentication required" {
        return ui::reauthentication_page(
            Some(&s.account.username),
            s.account.is_admin,
            &s.csrf,
            state.fresh_auth(),
            &members_href(slug),
        );
    }
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => not_permitted_page(s),
        // A malformed slug or another rejected request is the client's, not a
        // service failure: it must not read as 503. Everything else (database
        // errors and the like) keeps the 503 page.
        StatusCode::BAD_REQUEST => invalid_page(s),
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

/// The 400 page for a request the server refuses to interpret.
fn invalid_page(s: &Session) -> ui::UiError {
    let body = "<h1>Invalid request</h1><p>The organization name is not valid.</p>";
    ui::UiError::from(page(s, StatusCode::BAD_REQUEST, "Invalid request", body))
}

fn not_permitted_page(s: &Session) -> ui::UiError {
    let body = "<h1>Not permitted</h1><p>Only an organization owner can administer members.</p>";
    ui::UiError::from(page(s, StatusCode::FORBIDDEN, "Not permitted", body))
}

/// Require `organization.manage_members` before any member data is loaded or
/// rendered. Every page path uses this, including error re-renders, so a
/// validation failure can never show the member list to a caller who may not
/// administer it.
fn require_manage(
    state: &AppState,
    s: &Session,
    loaded: &membership::Loaded,
    slug: &str,
) -> Result<(), ui::UiError> {
    loaded
        .require(state, githearth_authz::Action::OrganizationManageMembers)
        .map_err(|e| api_error(state, s, slug, &e))
}

/// The page equivalent of `require_target`: an administrator, or the caller
/// themself where leaving is allowed. This runs before the per-form
/// idempotency key is inspected, so a bad or reused key cannot distinguish a
/// private organization from a missing one.
fn require_target_page(
    state: &AppState,
    s: &Session,
    loaded: &membership::Loaded,
    slug: &str,
    username: &str,
    allow_self: bool,
) -> Result<(), ui::UiError> {
    // Leaving is not the sensitive `manage_members` action; every other change
    // asks the engine, keeping a freshness denial as the password prompt.
    if allow_self && username == loaded.account.username {
        return Ok(());
    }
    loaded
        .require(state, githearth_authz::Action::OrganizationManageMembers)
        .map_err(|e| api_error(state, s, slug, &e))
}

struct Draft<'a> {
    username: &'a str,
    role: &'a str,
    key: &'a str,
}

impl Default for Draft<'_> {
    fn default() -> Self {
        Self {
            username: "",
            role: "member",
            key: "",
        }
    }
}

async fn members_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    require_manage(&state, &s, &loaded, &slug)?;
    render(&state, &s, &slug, StatusCode::OK, None, &Draft::default()).await
}

/// Renders the members page, optionally with a rejected submission and its
/// values preserved, so a validation error never discards the operator's input
/// or turns into a generic 503.
async fn render(
    state: &AppState,
    s: &Session,
    slug: &str,
    status: StatusCode,
    notice: Option<&str>,
    draft: &Draft<'_>,
) -> Result<Response, ui::UiError> {
    let loaded = membership::load(state, s.account.clone(), s.auth_age, slug)
        .await
        .map_err(|e| api_error(state, s, slug, &e))?;
    require_manage(state, s, &loaded, slug)?;
    let members = membership::all_members(state, loaded.org_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let owners = members.iter().filter(|m| m.role == "owner").count();
    let href = members_href(slug);

    let mut rows = String::new();
    for member in &members {
        // The last owner keeps no role-change or remove control: the server
        // refuses either, and offering it would only produce an error page.
        let last_owner = member.role == "owner" && owners <= 1;
        let actions = if last_owner {
            "<span class=\"muted\">Last owner</span>".to_owned()
        } else {
            format!(
                "<form method=\"post\" action=\"{href}/{}/role\" style=\"display:inline\">\
{}\
<label class=\"muted\" for=\"role-{}\">Role</label>\
<select id=\"role-{}\" name=\"role\">{}</select>\
<button type=\"submit\" aria-label=\"Change role for {}\">Change role</button></form> \
<form method=\"post\" action=\"{href}/{}/remove\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{}\">\
<button type=\"submit\" aria-label=\"Remove {}\">Remove</button></form>",
                ui::url_encode(&member.username),
                form_fields(s, &new_key()),
                ui::esc(&member.username),
                ui::esc(&member.username),
                if member.role == "owner" {
                    role_options("owner")
                } else {
                    role_options("member")
                },
                ui::esc(&member.username),
                ui::url_encode(&member.username),
                ui::esc(&s.csrf),
                ui::esc(&new_key()),
                ui::esc(&member.username),
            )
        };
        rows.push_str(&format!(
            "<tr><td>{}{}</td><td>{}</td><td><time datetime=\"{}\">{}</time></td><td>{}</td></tr>",
            ui::esc(&member.username),
            if member.account_id == s.account.id {
                " <span class=\"muted\">(you)</span>"
            } else {
                ""
            },
            ui::esc(&member.role),
            ui::esc(&member.created_at),
            ui::esc(&member.created_at),
            actions,
        ));
    }
    let table = if members.is_empty() {
        "<p class=\"muted\">No members yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Organization members</caption><thead><tr>\
<th scope=\"col\">Member</th><th scope=\"col\">Role</th><th scope=\"col\">Since</th>\
<th scope=\"col\">Actions</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    let notice = notice
        .map(|n| format!("<p class=\"error\" role=\"alert\">{}</p>", ui::esc(n)))
        .unwrap_or_default();
    let add_key = if draft.key.is_empty() {
        new_key()
    } else {
        draft.key.to_owned()
    };
    let add_form = format!(
        "<div class=\"card\"><h2>Add a member</h2>{notice}\
<form method=\"post\" action=\"{href}\">{fields}\
<label for=\"member-username\">Username</label>\
<input id=\"member-username\" name=\"username\" required maxlength=\"64\" value=\"{username}\">\
<label for=\"member-role\">Role</label>\
<select id=\"member-role\" name=\"role\">{roles}</select>\
<p><button type=\"submit\">Add member</button></p></form></div>",
        fields = form_fields(s, &add_key),
        username = ui::esc(draft.username),
        roles = role_options(draft.role),
    );

    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / \
<a href=\"/organizations/{slug}\">{slug}</a> / Members</nav>\
<h1>Members</h1><p class=\"muted\">Owners can administer members. Every change is audited.</p>\
<div class=\"card\">{table}</div>{add_form}",
        slug = ui::esc(slug),
    );
    Ok(page(s, status, "Members", &body))
}

#[derive(Deserialize)]
struct AddForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    role: String,
}

async fn add_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Form(form): Form<AddForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    require_manage(&state, &s, &loaded, &slug)?;
    // Authorization is decided before the per-form key is validated.
    let key = if idempotency::valid_key(&form.idempotency_key) {
        form.idempotency_key.as_str()
    } else {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Members",
            "<h1>Members</h1><p>Reload the page and submit the form again.</p>",
        )));
    };
    let draft = Draft {
        username: &form.username,
        role: if membership::ROLES.contains(&form.role.as_str()) {
            &form.role
        } else {
            "member"
        },
        key,
    };
    match membership::add_from_page(&state, &loaded, &form.username, &form.role, Some(key)).await {
        Ok(_) => Ok(Redirect::to(&members_href(&slug)).into_response()),
        Err(e) => {
            let status = match e.status_code() {
                StatusCode::BAD_REQUEST
                | StatusCode::CONFLICT
                | StatusCode::UNPROCESSABLE_ENTITY => e.status_code(),
                _ => return Err(api_error(&state, &s, &slug, &e)),
            };
            let notice = ui::idempotency_notice(&e).unwrap_or(e.message());
            render(&state, &s, &slug, status, Some(notice), &draft).await
        }
    }
}

#[derive(Deserialize)]
struct RoleForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    role: String,
}

async fn role_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, username)): Path<(String, String)>,
    Form(form): Form<RoleForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    require_manage(&state, &s, &loaded, &slug)?;
    // Authorization is decided before the per-form key is validated.
    let key = if idempotency::valid_key(&form.idempotency_key) {
        form.idempotency_key.as_str()
    } else {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Members",
            "<h1>Members</h1><p>Reload the page and submit the form again.</p>",
        )));
    };
    match membership::set_role(&state, &loaded, &username, &form.role, Some(key)).await {
        Ok(_) => Ok(Redirect::to(&members_href(&slug)).into_response()),
        Err(e) => {
            let status = match e.status_code() {
                StatusCode::BAD_REQUEST
                | StatusCode::CONFLICT
                | StatusCode::UNPROCESSABLE_ENTITY => e.status_code(),
                _ => return Err(api_error(&state, &s, &slug, &e)),
            };
            let notice = ui::idempotency_notice(&e).unwrap_or(e.message());
            render(&state, &s, &slug, status, Some(notice), &Draft::default()).await
        }
    }
}

#[derive(Deserialize)]
struct RemoveForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn remove_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, username)): Path<(String, String)>,
    Form(form): Form<RemoveForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    require_target_page(&state, &s, &loaded, &slug, &username, true)?;
    // Authorization is decided before the per-form key is validated.
    let key = if idempotency::valid_key(&form.idempotency_key) {
        form.idempotency_key.as_str()
    } else {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Members",
            "<h1>Members</h1><p>Reload the page and submit the form again.</p>",
        )));
    };
    match membership::remove(&state, &loaded, &username, Some(key), true).await {
        Ok(()) if username == s.account.username => Ok(Redirect::to("/").into_response()),
        Ok(()) => Ok(Redirect::to(&members_href(&slug)).into_response()),
        Err(e) => {
            let status = match e.status_code() {
                StatusCode::BAD_REQUEST
                | StatusCode::CONFLICT
                | StatusCode::UNPROCESSABLE_ENTITY => e.status_code(),
                _ => return Err(api_error(&state, &s, &slug, &e)),
            };
            let notice = ui::idempotency_notice(&e).unwrap_or(e.message());
            render(&state, &s, &slug, status, Some(notice), &Draft::default()).await
        }
    }
}
