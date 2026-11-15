//! Server-rendered teams page: list teams, create and delete a team, add and
//! remove members, and grant or revoke a team's repository access.
//!
//! Pages use the browser session cookie; every form carries the session's CSRF
//! token and a per-form idempotency key. Reads and writes go through the same
//! `pub(crate)` functions as the JSON API, so authorization, validation and
//! audit events are identical. The page asks the engine the same `team.*`
//! actions the API does; a caller allowed to read the organization gets "not
//! permitted", anyone else the 404 a missing organization gives. A team that
//! does not exist is the same 404.

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

use crate::{Account, ApiError, AppState, idempotency, membership, teams, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/organizations/{slug}/teams",
            get(teams_page).post(create_submit),
        )
        .route(
            "/organizations/{slug}/teams/{team}/delete",
            post(delete_submit),
        )
        .route(
            "/organizations/{slug}/teams/{team}",
            get(team_page).post(add_member_submit),
        )
        .route(
            "/organizations/{slug}/teams/{team}/members/{username}/remove",
            post(remove_member_submit),
        )
        .route(
            "/organizations/{slug}/teams/{team}/grants",
            post(grant_submit),
        )
        .route(
            "/organizations/{slug}/teams/{team}/grants/{grant}/revoke",
            post(revoke_grant_submit),
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
    let csrf_ok = ui::csrf_matches(supplied, &ui::csrf_expected(&hash));

    if !csrf_ok {
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

fn api_error(state: &AppState, s: &Session, slug: &str, error: &ApiError) -> ui::UiError {
    if error.message() == "reauthentication required" {
        return ui::reauthentication_page(
            Some(&s.account.username),
            s.account.is_admin,
            &s.csrf,
            state.fresh_auth(),
            &format!("/organizations/{}/teams", ui::url_encode(slug)),
        );
    }
    match error.status_code() {
        StatusCode::UNAUTHORIZED => ui::auth_required(),
        StatusCode::NOT_FOUND => ui::not_found_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => not_permitted_page(s),
        StatusCode::BAD_REQUEST => invalid_page(s),
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

fn invalid_page(s: &Session) -> ui::UiError {
    let body = "<h1>Invalid request</h1><p>The request is not valid. Reload and try again.</p>";
    ui::UiError::from(page(s, StatusCode::BAD_REQUEST, "Invalid request", body))
}

fn not_permitted_page(s: &Session) -> ui::UiError {
    let body = "<h1>Not permitted</h1><p>Only an organization owner can administer teams.</p>";
    ui::UiError::from(page(s, StatusCode::FORBIDDEN, "Not permitted", body))
}

/// The page refusal for an action the caller does not hold: a reader of the
/// organization gets "not permitted", anyone else the masked 404, and a stale
/// session whose grant is intact gets the password prompt.
fn refuse(state: &AppState, s: &Session, org: &membership::Loaded, action: Action) -> ui::UiError {
    match org.require(state, action) {
        Err(error) => api_error(state, s, &org.slug, &error),
        // `may` already answered no, so this arm is unreachable; keeping it
        // total avoids a second, different refusal.
        Ok(()) => not_permitted_page(s),
    }
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

/// A per-form key, or the "reload and submit again" page when it is missing or
/// malformed. Authorization is decided before this is reached, so a bad key
/// cannot distinguish a private organization from a missing one.
fn key_or_page(s: &Session, supplied: &str) -> Result<String, ui::UiError> {
    if idempotency::valid_key(supplied) {
        Ok(supplied.to_owned())
    } else {
        Err(ui::UiError::from(page(
            s,
            StatusCode::BAD_REQUEST,
            "Teams",
            "<h1>Teams</h1><p>Reload the page and submit the form again.</p>",
        )))
    }
}

fn render_error(s: &Session, slug: &str, notice: &str) -> Response {
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / \
<a href=\"/organizations/{slug}\">{slug}</a> / <a href=\"/organizations/{slug}/teams\">Teams</a></nav>\
<h1>Teams</h1><p class=\"error\" role=\"alert\">{}</p>\
<p><a href=\"/organizations/{slug}/teams\">Back to teams</a></p>",
        ui::esc(notice),
        slug = ui::esc(slug),
    );
    page(s, StatusCode::CONFLICT, "Teams", &body)
}

/// A rejected submission stays on the page with its reason; anything else is
/// the page its status deserves.
fn submission(
    state: &AppState,
    error: ApiError,
    s: &Session,
    slug: &str,
) -> Result<Response, ui::UiError> {
    if error.message() == "reauthentication required" {
        return Err(api_error(state, s, slug, &error));
    }
    match error.status_code() {
        StatusCode::BAD_REQUEST | StatusCode::CONFLICT => {
            Ok(render_error(s, slug, error.message()))
        }
        StatusCode::NOT_FOUND => Err(ui::not_found_page(Some(&s.account.username))),
        StatusCode::FORBIDDEN => Err(not_permitted_page(s)),
        _ => Err(ui::unavailable_page(Some(&s.account.username))),
    }
}

// ---------------------------------------------------------------------------
// The teams list
// ---------------------------------------------------------------------------

async fn teams_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let loaded = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    if !may(&state, &s, &loaded, Action::TeamCreate) {
        return Err(refuse(&state, &s, &loaded, Action::TeamCreate));
    }
    let list = teams::all_teams(&state, loaded.org_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = format!("/organizations/{}/teams", ui::url_encode(&loaded.slug));

    let mut rows = String::new();
    for team in &list {
        rows.push_str(&format!(
            "<li><a href=\"{base}/{slug}\">{name}</a> <span class=\"muted\">{slug}</span>\
<form method=\"post\" action=\"{base}/{slug}/delete\" style=\"display:inline\">{fields}\
<button type=\"submit\">Delete</button></form></li>",
            slug = ui::url_encode(&team.slug),
            name = ui::esc(&team.name),
            fields = form_fields(&s, &new_key()),
        ));
    }
    let list = if list.is_empty() {
        "<p class=\"muted\">No teams yet.</p>".to_owned()
    } else {
        format!("<div class=\"card\"><ul>{rows}</ul></div>")
    };

    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / \
<a href=\"/organizations/{slug}\">{slug}</a> / Teams</nav>\
<h1>Teams</h1><p class=\"muted\">A team grants its repository access to every member. \
Teams grant access exactly as a member's own grant does. Every change is audited.</p>\
{list}<div class=\"card\"><h2>Create a team</h2>\
<form method=\"post\" action=\"{base}\">{fields}\
<label for=\"team-name\">Name</label>\
<input id=\"team-name\" name=\"name\" required maxlength=\"200\">\
<label for=\"team-slug\">Slug</label>\
<input id=\"team-slug\" name=\"slug\" required maxlength=\"64\" pattern=\"[a-z0-9]+(-[a-z0-9]+)*\">\
<p><button type=\"submit\">Create team</button></p></form></div>",
        slug = ui::esc(&loaded.slug),
        fields = form_fields(&s, &new_key()),
    );
    Ok(page(&s, StatusCode::OK, "Teams", &body))
}

#[derive(Deserialize)]
struct CreateForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
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
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    if !may(&state, &s, &loaded, Action::TeamCreate) {
        return Err(refuse(&state, &s, &loaded, Action::TeamCreate));
    }
    let key = key_or_page(&s, &form.idempotency_key)?;
    let body = serde_json::json!({"slug": form.slug, "name": form.name}).to_string();
    match teams::create(&state, &loaded, body.as_bytes(), Some(&key)).await {
        Ok(_) => Ok(Redirect::to(&format!(
            "/organizations/{}/teams",
            ui::url_encode(&loaded.slug)
        ))
        .into_response()),
        Err(e) => submission(&state, e, &s, &loaded.slug),
    }
}

#[derive(Deserialize)]
struct TeamForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
}

async fn delete_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team)): Path<(String, String)>,
    Form(form): Form<TeamForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    if !may(&state, &s, &org, Action::TeamDelete) {
        return Err(refuse(&state, &s, &org, Action::TeamDelete));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    let key = key_or_page(&s, &form.idempotency_key)?;
    match teams::delete_team_inner(&state, &loaded, Some(&key)).await {
        Ok(()) => Ok(Redirect::to(&format!(
            "/organizations/{}/teams",
            ui::url_encode(&loaded.org.slug)
        ))
        .into_response()),
        Err(e) => submission(&state, e, &s, &slug),
    }
}

// ---------------------------------------------------------------------------
// One team
// ---------------------------------------------------------------------------

async fn team_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team)): Path<(String, String)>,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    // The page and every member and grant control on it are `team.update` or
    // `team.delete`; a caller with neither gets the refusal page, not the team.
    if !may(&state, &s, &org, Action::TeamUpdate) {
        return Err(refuse(&state, &s, &org, Action::TeamUpdate));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    render_team(&state, &s, &loaded).await
}

async fn render_team(
    state: &AppState,
    s: &Session,
    loaded: &teams::Loaded,
) -> Result<Response, ui::UiError> {
    let org = &loaded.org;
    let team = teams::get(state, org.org_id, &loaded.team_slug)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?
        .ok_or_else(|| ui::not_found_page(Some(&s.account.username)))?;
    let members = teams::members(state, loaded.team_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let grants = teams::grants(state, loaded.team_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let org_members = membership::all_members(state, org.org_id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    let base = format!(
        "/organizations/{}/teams/{}",
        ui::url_encode(&org.slug),
        ui::url_encode(&team.slug)
    );

    let can_update = may(state, s, org, Action::TeamUpdate);
    let mut rows = String::new();
    for member in &members {
        let remove = if may(state, s, org, Action::TeamUpdate) {
            format!(
                "<form method=\"post\" action=\"{base}/members/{path}/remove\" style=\"display:inline\">{fields}\
<button type=\"submit\">Remove</button></form>",
                path = ui::url_encode(&member.username),
                fields = form_fields(s, &new_key()),
            )
        } else {
            String::new()
        };
        rows.push_str(&format!(
            "<tr><td>{username}</td><td>{role}</td><td>{remove}</td></tr>",
            username = ui::esc(&member.username),
            role = ui::esc(&member.role),
        ));
    }
    let member_table = if members.is_empty() {
        "<p class=\"muted\">No members yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Team members</caption><thead><tr>\
<th scope=\"col\">Member</th><th scope=\"col\">Organization role</th><th scope=\"col\">Actions</th>\
</tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    // Each control appears only when the caller holds the action its handler
    // asks for: granting needs `team.update`, revoking a grant `team.delete`
    // and deleting the team `team.delete`. A caller who may update but not
    // delete gets a page with no form whose submit would be refused.
    let can_delete = may(state, s, org, Action::TeamDelete);
    let mut grant_rows = String::new();
    for grant in &grants {
        let target = grant.repository.clone().map_or_else(
            || "Whole organization".to_owned(),
            |name| format!("Repository {name}"),
        );
        let revoke = if can_delete {
            format!(
                "<form method=\"post\" action=\"{base}/grants/{}/revoke\" style=\"display:inline\">{fields}\
<button type=\"submit\">Revoke</button></form>",
                ui::esc(&grant.id.to_string()),
                fields = form_fields(s, &new_key()),
            )
        } else {
            String::new()
        };
        grant_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{revoke}</td></tr>",
            ui::esc(&target),
            ui::esc(&grant.role),
        ));
    }
    let grant_table = if grants.is_empty() {
        "<p class=\"muted\">This team grants no access yet.</p>".to_owned()
    } else {
        format!(
            "<table><caption class=\"muted\">Grants</caption><thead><tr>\
<th scope=\"col\">Scope</th><th scope=\"col\">Role</th><th scope=\"col\">Actions</th>\
</tr></thead><tbody>{grant_rows}</tbody></table>"
        )
    };

    let options: String = org_members
        .iter()
        .filter(|m| {
            !members
                .iter()
                .any(|existing| existing.account_id == m.account_id)
        })
        .filter(|m| can_update || m.account_id == s.account.id)
        .filter(|m| can_update || m.account_id == s.account.id)
        .map(|m| {
            format!(
                "<option value=\"{}\">{}</option>",
                ui::esc(&m.username),
                ui::esc(&m.username)
            )
        })
        .collect();
    let add_member = if options.is_empty() {
        "<p class=\"muted\">Every organization member is already on this team.</p>".to_owned()
    } else {
        // Adding any organization member to the team is `team.update`; the
        // list is kept only when the caller holds it.
        format!(
            "<form method=\"post\" action=\"{base}\">{fields}\
<label for=\"team-username\">Member</label>\
<select id=\"team-username\" name=\"username\">{options}</select>\
<p><button type=\"submit\">Add to team</button></p></form>",
            fields = form_fields(s, &new_key()),
        )
    };

    let roles: String = teams::REPOSITORY_ROLES
        .iter()
        .map(|role| {
            let selected = if *role == "write" { " selected" } else { "" };
            format!(
                "<option value=\"{}\"{}>{}</option>",
                ui::esc(role),
                selected,
                ui::esc(role)
            )
        })
        .collect();
    let grant_access = if can_update {
        format!(
            "<h2>Grant repository access</h2>\
<form method=\"post\" action=\"{base}/grants\">{fields}\
<label for=\"grant-repository\">Repository</label>\
<input id=\"grant-repository\" name=\"repository\" required maxlength=\"100\" pattern=\"[a-z0-9]+(-[a-z0-9]+)*\">\
<label for=\"grant-role\">Role</label>\
<select id=\"grant-role\" name=\"role\">{roles}</select>\
<p><button type=\"submit\">Grant</button></p></form>",
            fields = form_fields(s, &new_key()),
        )
    } else {
        String::new()
    };
    let body = format!(
        "<nav aria-label=\"Breadcrumb\"><a href=\"/\">Home</a> / \
<a href=\"/organizations/{org_slug}\">{org_slug}</a> / \
<a href=\"/organizations/{org_slug}/teams\">Teams</a> / {name}</nav>\
<h1>{name}</h1><p class=\"muted\">{slug}</p>\
<div class=\"card\">{member_table}<h2>Add a member</h2>{add_member}</div>\
<div class=\"card\">{grant_table}{grant_access}</div>",
        org_slug = ui::esc(&org.slug),
        name = ui::esc(&team.name),
        slug = ui::esc(&team.slug),
    );
    Ok(page(s, StatusCode::OK, &team.name, &body))
}

#[derive(Deserialize)]
struct AddMemberForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    username: String,
}

async fn add_member_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team)): Path<(String, String)>,
    Form(form): Form<AddMemberForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    // Adding a member is `team.update`, the same action `add_member` asks for,
    // whether the form names the caller themselves or someone else.
    if !may(&state, &s, &org, Action::TeamUpdate) {
        return Err(refuse(&state, &s, &org, Action::TeamUpdate));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    let key = key_or_page(&s, &form.idempotency_key)?;
    let body = serde_json::json!({"username": form.username}).to_string();
    match teams::add_member(&state, &loaded, &form.username, body.as_bytes(), Some(&key)).await {
        Ok(_) => Ok(redirect_to_team(&loaded)),
        Err(e) => submission(&state, e, &s, &loaded.org.slug),
    }
}

async fn remove_member_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team, username)): Path<(String, String, String)>,
    Form(form): Form<TeamForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    if !may(&state, &s, &org, Action::TeamUpdate) {
        return Err(refuse(&state, &s, &org, Action::TeamUpdate));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    let key = key_or_page(&s, &form.idempotency_key)?;
    match teams::remove_member(&state, &loaded, &username, Some(&key)).await {
        Ok(()) => Ok(redirect_to_team(&loaded)),
        Err(e) => submission(&state, e, &s, &loaded.org.slug),
    }
}

#[derive(Deserialize)]
struct GrantForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    repository: String,
    #[serde(default)]
    role: String,
}

async fn grant_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team)): Path<(String, String)>,
    Form(form): Form<GrantForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    // Adding a grant is `team.update`, the same action `add_grant` asks for.
    if !may(&state, &s, &org, Action::TeamUpdate) {
        return Err(refuse(&state, &s, &org, Action::TeamUpdate));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    let key = key_or_page(&s, &form.idempotency_key)?;
    let body = serde_json::json!({"repository": form.repository, "role": form.role}).to_string();
    match teams::add_grant(&state, &loaded, body.as_bytes(), Some(&key)).await {
        Ok(_) => Ok(redirect_to_team(&loaded)),
        Err(e) => submission(&state, e, &s, &loaded.org.slug),
    }
}

async fn revoke_grant_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, team, grant)): Path<(String, String, String)>,
    Form(form): Form<TeamForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let org = membership::load(&state, s.account.clone(), s.auth_age, &slug)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    // Revoking a grant is `team.delete`, not `team.update`.
    if !may(&state, &s, &org, Action::TeamDelete) {
        return Err(refuse(&state, &s, &org, Action::TeamDelete));
    }
    let loaded = teams::load_team(&state, org, &team)
        .await
        .map_err(|e| api_error(&state, &s, &slug, &e))?;
    let key = key_or_page(&s, &form.idempotency_key)?;
    let grant_id = Uuid::parse_str(&grant).map_err(|_| invalid_page(&s))?;
    match teams::revoke_grant(&state, &loaded, grant_id, Some(&key)).await {
        Ok(()) => Ok(redirect_to_team(&loaded)),
        Err(e) => submission(&state, e, &s, &loaded.org.slug),
    }
}

fn redirect_to_team(loaded: &teams::Loaded) -> Response {
    Redirect::to(&format!(
        "/organizations/{}/teams/{}",
        ui::url_encode(&loaded.org.slug),
        ui::url_encode(&loaded.team_slug)
    ))
    .into_response()
}
