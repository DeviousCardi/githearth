//! The repository transfer page: choose a target organization and the fork
//! pull-request policy, then submit the same operation as the JSON API.
//!
//! The page carries the session's CSRF token and a per-form idempotency key,
//! escapes every value it renders, and asks the engine which targets the caller
//! may create in rather than reading roles. A refused move keeps the operator's
//! choice on the page with the reason, so changing the policy is one click.
//! A successful move redirects to the repository at the target; the source page
//! is gone, so this redirect is how the operator is led to the new location.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    Account, AppState, idempotency, repository_api,
    transfer::{self, ForkPolicy},
    ui,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new().route(
        "/organizations/{slug}/repositories/{name}/transfer",
        get(transfer_page).post(transfer_submit),
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

fn transfer_href(slug: &str, name: &str) -> String {
    format!(
        "/organizations/{}/repositories/{}/transfer",
        ui::url_encode(slug),
        ui::url_encode(name)
    )
}

fn repo_href(slug: &str, name: &str) -> String {
    format!(
        "/organizations/{}/repositories/{}",
        ui::url_encode(slug),
        ui::url_encode(name)
    )
}

/// What a submission tried, so a refusal re-renders it instead of a blank form.
#[derive(Default, Clone)]
struct Draft {
    organization: Option<String>,
    policy: Option<String>,
    key: Option<String>,
    notice: Option<String>,
}

async fn render(
    state: &AppState,
    s: &Session,
    slug: &str,
    name: &str,
    status: StatusCode,
    draft: &Draft,
) -> Result<Response, ui::UiError> {
    let loaded = repository_api::load_current(state, s.account.id, s.auth_age, slug, name)
        .await
        .map_err(|e| map_error(s, e))?;
    // Only a caller who may transfer sees the form at all.
    repository_api::require(state, &loaded, githearth_authz::Action::RepositoryTransfer)
        .map_err(|e| map_error(s, e))?;
    let model = transfer::page_model(state, &loaded)
        .await
        .map_err(|e| map_error(s, e))?;

    let mut options = String::new();
    for (target_slug, target_name) in &model.targets {
        let selected = draft.organization.as_deref() == Some(target_slug.as_str());
        options.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            ui::esc(target_slug),
            if selected { " selected" } else { "" },
            ui::esc(&format!("{target_slug} ({target_name})"))
        ));
    }
    let targets = if options.is_empty() {
        "<p>You cannot create repositories in any other organization, so this repository cannot be transferred.</p>"
            .to_owned()
    } else {
        format!(
            "<label for=\"transfer_org\">Target organization</label><select id=\"transfer_org\" name=\"organization\">{options}</select>"
        )
    };
    let selected_policy = draft.policy.as_deref().unwrap_or("refuse");
    let policy = format!(
        "<fieldset><legend>Open fork pull requests ({})</legend>\
<label><input type=\"radio\" name=\"fork_pull_requests\" value=\"refuse\"{}> Refuse the transfer</label>\
<label><input type=\"radio\" name=\"fork_pull_requests\" value=\"close\"{}> Close them (audited)</label></fieldset>",
        model.open_fork_pulls,
        if selected_policy == "refuse" {
            " checked"
        } else {
            ""
        },
        if selected_policy == "close" {
            " checked"
        } else {
            ""
        },
    );
    let notice = draft
        .notice
        .as_deref()
        .map(|n| format!("<p role=\"alert\">{}</p>", ui::esc(n)))
        .unwrap_or_default();
    let key = draft
        .key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let action = transfer_href(slug, name);
    let body = format!(
        "<h1>Transfer {}</h1>{notice}<div class=\"card\">\
<p>The repository keeps its ID, history and every open pull request that does not come from a fork. \
Its storage path does not change. Every grant that gives access to it is revoked, because access \
grants belong to the organization that owns the repository. The old address stops working. \
The repository keeps its name; if the target organization already has a repository with that name \
the transfer is refused, and you rename first and transfer afterwards.</p>\
<form method=\"post\" action=\"{action}\">\
<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
<input type=\"hidden\" name=\"idempotency_key\" value=\"{key}\">\
{targets}{policy}\
<p><button type=\"submit\">Transfer repository</button></p></form></div>\
<p><a href=\"{back}\">Back to the repository</a></p>",
        ui::esc(&model.repo.name),
        csrf = ui::esc(&s.csrf),
        key = ui::esc(&key),
        back = repo_href(slug, &model.repo.name),
    );
    Ok(page(s, status, "Transfer repository", &body))
}

/// Map an operation's refusal to the page a browser should see: a masked 404
/// for what does not exist or is not readable, the panel with the reason for
/// what the operator can change, and the 403 page for a real denial.
fn map_error(s: &Session, error: crate::ApiError) -> ui::UiError {
    match error.status_code() {
        StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST => {
            ui::not_found_page(Some(&s.account.username))
        }
        StatusCode::GONE => ui::gone_page(Some(&s.account.username)),
        StatusCode::FORBIDDEN => ui::forbidden_page(),
        _ => ui::unavailable_page(Some(&s.account.username)),
    }
}

async fn transfer_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, ui::UiError> {
    if crate::ui::path_moved_away(&state, &slug, &name).await {
        let username = ui::browser_auth(&state, &headers)
            .await
            .ok()
            .map(|(account, _)| account.username);
        return Err(ui::moved_away_page(username.as_deref()));
    }
    let s = session(&state, &headers).await?;
    render(&state, &s, &slug, &name, StatusCode::OK, &Draft::default()).await
}

#[derive(Deserialize)]
struct TransferForm {
    #[serde(default)]
    csrf_token: String,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    organization: String,
    #[serde(default)]
    fork_pull_requests: String,
}

async fn transfer_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((slug, name)): Path<(String, String)>,
    Form(form): Form<TransferForm>,
) -> Result<Response, ui::UiError> {
    let token_hash = ui::browser_token_hash(&headers).ok_or_else(ui::auth_required)?;
    if !ui::csrf_matches(&form.csrf_token, &ui::csrf_expected(&token_hash)) {
        return Err(ui::forbidden_page());
    }
    let s = session(&state, &headers).await?;
    let policy = match form.fork_pull_requests.as_str() {
        "close" => ForkPolicy::Close,
        "refuse" => ForkPolicy::Refuse,
        _ => {
            return Err(ui::UiError::from(page(
                &s,
                StatusCode::BAD_REQUEST,
                "Transfer repository",
                "<h1>Transfer repository</h1><p>Reload the page and submit the form again.</p>",
            )));
        }
    };
    if !idempotency::valid_key(&form.idempotency_key) {
        return Err(ui::UiError::from(page(
            &s,
            StatusCode::BAD_REQUEST,
            "Transfer repository",
            "<h1>Transfer repository</h1><p>Reload the page and submit the form again.</p>",
        )));
    }
    let body = serde_json::json!({
        "organization": form.organization,
        "fork_pull_requests": policy.as_str(),
    })
    .to_string();
    match transfer::run(
        &state,
        s.account.id,
        s.auth_age,
        &slug,
        &name,
        &form.organization,
        policy,
        body.as_bytes(),
        Some(&form.idempotency_key),
    )
    .await
    {
        Ok(result) => {
            // A transfer keeps the name, so the target's page is the same name
            // under the target's slug.
            Ok(Redirect::to(&repo_href(&result.organization, &result.name)).into_response())
        }
        Err(e) => {
            let status = match e.status_code() {
                StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => e.status_code(),
                _ => return Err(map_error(&s, e)),
            };
            let notice = ui::idempotency_notice(&e).unwrap_or(e.message());
            let draft = Draft {
                organization: Some(form.organization),
                policy: Some(policy.as_str().to_owned()),
                key: Some(form.idempotency_key),
                notice: Some(notice.to_owned()),
            };
            // The repository is still at the source when the move was refused.
            render(&state, &s, &slug, &name, status, &draft).await
        }
    }
}
