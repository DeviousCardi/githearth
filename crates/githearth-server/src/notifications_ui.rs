//! Server-rendered notifications page: list the signed-in account's in-app
//! notifications and mark them read.
//!
//! Pages use the browser session cookie; every form carries the session's
//! CSRF token. Reads and writes reuse the same authorization checks as the
//! JSON API (`repository_api::load_current` + `require_readable_repo`), so a
//! notification on a repository the viewer can no longer read is never shown.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, issues, repository_api, ui};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/notifications", get(notifications_page))
        .route(
            "/notifications/{id}/read",
            axum::routing::post(mark_read_submit),
        )
        .route(
            "/notifications/read-all",
            axum::routing::post(mark_all_read_submit),
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

/// One notification row, joined against its pull request, repository,
/// organization and (optionally) the acting account.
struct NotificationRow {
    id: Uuid,
    kind: String,
    number: i32,
    title: String,
    repo_name: String,
    org_slug: String,
    actor_username: Option<String>,
    created_at: String,
    read_at: Option<String>,
}

const NOTIFICATION_SQL: &str = "SELECT n.id, n.kind, p.number, p.title, r.name AS repo_name, \
     o.slug AS org_slug, a.username AS actor_username, \
     to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
     to_char(n.read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS read_at \
     FROM notifications n \
     JOIN pull_requests p ON p.id = n.pull_request_id \
     JOIN repositories r ON r.id = p.repository_id \
     JOIN organizations o ON o.id = r.organization_id \
     LEFT JOIN accounts a ON a.id = n.actor_id \
     WHERE n.account_id = $1 \
     ORDER BY n.created_at DESC, n.id DESC LIMIT 50";

async fn list_notifications(
    state: &AppState,
    account_id: Uuid,
) -> Result<Vec<NotificationRow>, sqlx::Error> {
    let rows = sqlx::query(NOTIFICATION_SQL)
        .bind(account_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| NotificationRow {
            id: row.get("id"),
            kind: row.get("kind"),
            number: row.get("number"),
            title: row.get("title"),
            repo_name: row.get("repo_name"),
            org_slug: row.get("org_slug"),
            actor_username: row.get("actor_username"),
            created_at: row.get("created_at"),
            read_at: row.get("read_at"),
        })
        .collect())
}

/// Whether the viewer can still read the repository a notification points at.
/// Any failure (renamed repo, deleted repo, revoked membership, non-active
/// status) means the row's details must not be shown.
async fn can_read(
    state: &AppState,
    account_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
) -> bool {
    let loaded = match repository_api::load_current(
        state, account_id, auth_age, org_slug, repo_name,
    )
    .await
    {
        Ok(v) => v,
        Err(_) => return false,
    };
    if issues::require_readable_repo(&loaded).is_err() {
        return false;
    }
    repository_api::require(state, &loaded, githearth_authz::Action::RepositoryRead).is_ok()
}

/// The sentence prefix for each notification kind.
fn kind_sentence(kind: &str, actor: &str) -> String {
    let actor = ui::esc(actor);
    match kind {
        "pull_request.merged" => format!("<strong>{actor}</strong> merged"),
        "pull_request.reviewed" => format!("<strong>{actor}</strong> reviewed"),
        "pull_request.review_requested" => {
            format!("<strong>{actor}</strong> requested your review on")
        }
        _ => format!("<strong>{actor}</strong>"),
    }
}

async fn notifications_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ui::UiError> {
    let s = session(&state, &headers).await?;
    let rows = list_notifications(&state, s.account.id)
        .await
        .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;

    let mut items = String::new();
    let mut any_unread = false;
    for row in rows {
        let actor = row.actor_username.as_deref().unwrap_or("someone");
        let read = row.read_at.is_some();
        if !read {
            any_unread = true;
        }

        if !can_read(
            &state,
            s.account.id,
            s.auth_age,
            &row.org_slug,
            &row.repo_name,
        )
        .await
        {
            let muted = if read { " class=\"muted\"" } else { "" };
            let mark_button = if read {
                String::new()
            } else {
                format!(
                    "<form method=\"post\" action=\"/notifications/{}/read\" style=\"display:inline\">\
<button type=\"submit\">Mark read</button></form>",
                    row.id
                )
            };
            items.push_str(&format!(
                "<li{}><p>A pull request you can no longer access <span class=\"muted\">{}</span></p>{}</li>",
                muted,
                ui::esc(&row.created_at),
                mark_button
            ));
            continue;
        }

        let href = format!(
            "/organizations/{}/repositories/{}/pulls/{}",
            ui::url_encode(&row.org_slug),
            ui::url_encode(&row.repo_name),
            row.number
        );
        let sentence = kind_sentence(&row.kind, actor);
        let target = format!(
            "<a href=\"{}\">{} #{} {}</a>",
            ui::esc(&href),
            ui::esc(&format!("{}/{}", row.org_slug, row.repo_name)),
            row.number,
            ui::esc(&row.title)
        );
        let muted = if read { " class=\"muted\"" } else { "" };
        let mark_button = if read {
            String::new()
        } else {
            format!(
                "<form method=\"post\" action=\"/notifications/{}/read\" style=\"display:inline\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Mark read</button></form>",
                row.id,
                ui::esc(&s.csrf)
            )
        };
        items.push_str(&format!(
            "<li{}><p>{} {}</p><p class=\"muted\">{}</p>{}</li>",
            muted,
            sentence,
            target,
            ui::esc(&row.created_at),
            mark_button
        ));
    }

    if items.is_empty() {
        items.push_str("<li>No notifications.</li>");
    }

    let mark_all = if any_unread {
        format!(
            "<form method=\"post\" action=\"/notifications/read-all\" class=\"card\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">\
<button type=\"submit\">Mark all read</button></form>",
            ui::esc(&s.csrf)
        )
    } else {
        String::new()
    };

    let body =
        format!("<h1>Notifications</h1>{mark_all}<div class=\"card\"><ul>{items}</ul></div>");
    Ok(page(&s, StatusCode::OK, "Notifications", &body))
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

async fn mark_read_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    let result = sqlx::query(
        "UPDATE notifications SET read_at = COALESCE(read_at, clock_timestamp()) \
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(s.account.id)
    .execute(&state.pool)
    .await
    .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    if result.rows_affected() == 0 {
        return Err(ui::not_found_page(Some(&s.account.username)));
    }
    Ok(Redirect::to("/notifications").into_response())
}

async fn mark_all_read_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Result<Response, ui::UiError> {
    let s = form_session(&state, &headers, &form.csrf_token).await?;
    sqlx::query(
        "UPDATE notifications SET read_at = COALESCE(read_at, clock_timestamp()) \
         WHERE account_id = $1 AND read_at IS NULL",
    )
    .bind(s.account.id)
    .execute(&state.pool)
    .await
    .map_err(|_| ui::unavailable_page(Some(&s.account.username)))?;
    Ok(Redirect::to("/notifications").into_response())
}
