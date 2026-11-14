//! Commit statuses/checks (M3): a CI system reports the state of a commit
//! under a named `context`, and branch protection can require specific
//! contexts to be `success` before a pull request may merge.
//!
//! Statuses are append-only: posting a new result for a context supersedes the
//! previous one for merge purposes, while the full history stays visible. A
//! status names the exact commit it was posted for, so a status for one commit
//! can never authorize the merge of a different commit.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::Action;
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, event_with, has_nul, idempotency, invalid, issues,
    repositories, repository_api,
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/organizations/{org}/repositories/{repo}/commits/{sha}/statuses",
        axum::routing::post(create).get(list),
    )
}

/// The status states a poster may report, mirroring GitHub's commit status.
fn valid_state(state: &str) -> bool {
    matches!(state, "pending" | "success" | "failure" | "error")
}

/// A status context is a short, non-empty name (e.g. "ci/build") with no
/// leading or trailing whitespace. Such whitespace would make a posted status
/// or a required check context impossible to match against a well-formed rule.
pub(crate) fn valid_context(context: &str) -> bool {
    (1..=255).contains(&context.chars().count()) && context == context.trim() && !has_nul(context)
}

/// Whether a context has leading or trailing whitespace around a non-empty
/// name (e.g. " ci "). Such a name can never match a rule, so callers reject it
/// with 422 rather than the generic 400 for an empty or oversized context.
pub(crate) fn has_surrounding_whitespace(context: &str) -> bool {
    !context.trim().is_empty() && context != context.trim()
}

/// The reserved context used to keep pre-M3 `require_checks = true` branch
/// protection rules gating merges after the migration to `required_status_checks`
/// (see migration 0023). It is intentionally un-satisfiable: the status API
/// refuses to post a status under this context, so a rule requiring it never
/// passes. Owners must replace it with a real context list to unblock merges.
pub(crate) const LEGACY_REQUIRE_CHECKS_CONTEXT: &str = "githearth/legacy-require-checks";

/// A `target_url` may only use an `http`/`https` scheme; anything else (such as
/// `javascript:`) must be refused on create and never rendered as a link href.
pub(crate) fn valid_target_url(url: &str) -> bool {
    url.get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
        || url
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
}

/// Full lowercase commit ids only, as stored and compared.
fn valid_commit_id(id: &str) -> bool {
    crate::browse::is_commit_id(id) && !id.bytes().any(|b| b.is_ascii_uppercase())
}

const STATUS_JSON_COLUMNS: &str = "id, repository_id, commit_id, context, state, description, \
 target_url, creator_id, seq, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

/// Raw columns for the DISTINCT ON subquery; `created_at` stays a timestamp
/// there so the outer `to_char` aliases it back to the JSON shape.
const STATUS_RAW_COLUMNS: &str = "id, repository_id, commit_id, context, state, description, \
 target_url, creator_id, seq, created_at";

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct StatusJson {
    pub(crate) id: Uuid,
    pub(crate) repository_id: Uuid,
    pub(crate) commit_id: String,
    pub(crate) context: String,
    pub(crate) state: String,
    pub(crate) description: Option<String>,
    pub(crate) target_url: Option<String>,
    pub(crate) creator_id: Uuid,
    pub(crate) seq: i64,
    pub(crate) created_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewStatus {
    context: String,
    state: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

fn new_status(body: &[u8]) -> Result<NewStatus, ApiError> {
    let input: NewStatus = serde_json::from_slice(body).map_err(|_| invalid())?;
    if has_surrounding_whitespace(&input.context) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "context must not have leading or trailing whitespace",
        ));
    }
    if input.context == LEGACY_REQUIRE_CHECKS_CONTEXT {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "context is reserved and cannot be posted",
        ));
    }
    if !valid_context(&input.context)
        || !valid_state(&input.state)
        || input
            .description
            .as_ref()
            .is_some_and(|d| d.chars().count() > 65536 || has_nul(d))
        || input
            .target_url
            .as_ref()
            .is_some_and(|u| u.chars().count() > 4096 || has_nul(u))
    {
        return Err(invalid());
    }
    if let Some(ref url) = input.target_url
        && !valid_target_url(url)
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "target_url must use an http or https scheme",
        ));
    }
    Ok(input)
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, sha)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<StatusJson>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    if !valid_commit_id(&sha) {
        return Err(invalid());
    }
    let input = new_status(&body)?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/commits/{sha}/statuses");
    let state = &state;
    let body = &body;
    let (status, value) = idempotency::run(
        &state.pool,
        actor.id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
                    .await?;
            // Posting a status is a write: the `status.create` action sits in
            // the "write" group, so a read-only token or reader is refused.
            repository_api::require(state, &loaded, Action::StatusCreate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(issues::not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(issues::not_active());
            }
            let id = Uuid::new_v4();
            let status: StatusJson = sqlx::query_as(&format!(
                "INSERT INTO commit_statuses \
                 (id, repository_id, commit_id, context, state, description, target_url, creator_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {STATUS_JSON_COLUMNS}"
            ))
            .bind(id)
            .bind(loaded.repo.id)
            .bind(&sha)
            .bind(&input.context)
            .bind(&input.state)
            .bind(&input.description)
            .bind(&input.target_url)
            .bind(actor.id)
            .fetch_one(&mut *tx)
            .await?;
            event_with(
                &mut tx,
                Some(loaded.org_id),
                actor.id,
                "commit_status.created",
                serde_json::json!({
                    "repository_id": loaded.repo.id,
                    "commit_id": &sha,
                    "context": &input.context,
                    "state": &input.state,
                }),
            )
            .await?;
            let created = StatusCode::CREATED;
            let response = serde_json::to_value(&status).unwrap();
            if !idem.store(&mut tx, created, &response).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            // A status may have just satisfied the last required check on a
            // pull request with a pending auto-merge: re-drive it now so the
            // intent is raised without waiting for another review or push.
            let _ = crate::auto_merge::on_status(state, loaded.repo.id, &sha, actor.id).await;
            Ok((created, response))
        },
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, sha)): Path<(String, String, String)>,
) -> Result<Json<Vec<StatusJson>>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    if !valid_commit_id(&sha) {
        return Err(invalid());
    }
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    repository_api::require(&state, &loaded, Action::RepositoryRead)?;
    issues::require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, StatusJson>(&format!(
        "SELECT {STATUS_JSON_COLUMNS} FROM commit_statuses \
         WHERE repository_id = $1 AND commit_id = $2 ORDER BY seq DESC"
    ))
    .bind(loaded.repo.id)
    .bind(&sha)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(rows))
}

/// The latest status for each context at `commit_id`, newest `seq` per context,
/// ordered newest first. Used to render the pull request page.
pub(crate) async fn latest_by_context(
    conn: &mut PgConnection,
    repository_id: Uuid,
    commit_id: &str,
) -> Result<Vec<StatusJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {STATUS_JSON_COLUMNS} FROM (\
           SELECT DISTINCT ON (context) {STATUS_RAW_COLUMNS} FROM commit_statuses \
           WHERE repository_id = $1 AND commit_id = $2 \
           ORDER BY context, seq DESC\
         ) latest ORDER BY seq DESC"
    ))
    .bind(repository_id)
    .bind(commit_id)
    .fetch_all(&mut *conn)
    .await
}

/// The latest `(context, state)` for each of `required` at `commit_id`, ordered
/// by context (deterministic, for the checks/approvals digest). A context with
/// no status is simply absent.
pub(crate) async fn latest_required_states(
    conn: &mut PgConnection,
    repository_id: Uuid,
    commit_id: &str,
    required: &[String],
) -> Result<Vec<(String, String)>, sqlx::Error> {
    if required.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT context, state FROM (\
           SELECT DISTINCT ON (context) context, state FROM commit_statuses \
           WHERE repository_id = $1 AND commit_id = $2 AND context = ANY($3) \
           ORDER BY context, seq DESC\
         ) latest ORDER BY context",
    )
    .bind(repository_id)
    .bind(commit_id)
    .bind(required)
    .fetch_all(&mut *conn)
    .await
}

/// Whether every `required` context has a latest status of `success` at
/// `commit_id` (vacuously true when nothing is required).
pub(crate) async fn checks_passed(
    conn: &mut PgConnection,
    repository_id: Uuid,
    commit_id: &str,
    required: &[String],
) -> Result<bool, sqlx::Error> {
    let states = latest_required_states(conn, repository_id, commit_id, required).await?;
    Ok(all_success(required, &states))
}

/// Pure helper: every required context appears in `states` with `success`.
pub(crate) fn all_success(required: &[String], states: &[(String, String)]) -> bool {
    required.iter().all(|c| {
        states
            .iter()
            .any(|(ctx, state)| ctx == c && state == "success")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_and_state_validation() {
        assert!(valid_context("ci/build"));
        assert!(valid_context("x"));
        assert!(valid_context("ci build"));
        assert!(!valid_context(""));
        assert!(!valid_context("   "));
        assert!(!valid_context(" ci"));
        assert!(!valid_context("ci "));
        assert!(!valid_context(" ci "));
        assert!(!valid_context(&"x".repeat(256)));

        for state in ["pending", "success", "failure", "error"] {
            assert!(valid_state(state));
        }
        assert!(!valid_state("running"));
        assert!(!valid_state(""));
    }

    #[test]
    fn context_length_is_measured_in_characters_not_bytes() {
        // 100 CJK characters are 300 UTF-8 bytes but 100 characters, so
        // migration 0023's CHECK (1..=255 chars) accepts them. The validator
        // must agree instead of rejecting the context for its byte length.
        let cjk: String = "界".repeat(100);
        assert_eq!(cjk.chars().count(), 100);
        assert!(cjk.len() > 255);
        assert!(valid_context(&cjk));
        // The upper bound is still 255 characters.
        assert!(!valid_context(&"界".repeat(256)));
    }

    #[test]
    fn commit_ids_are_full_lowercase_hex() {
        let sha40 = "a".repeat(40);
        let sha64 = "f".repeat(64);
        assert!(valid_commit_id(&sha40));
        assert!(valid_commit_id(&sha64));
        assert!(!valid_commit_id(&"A".repeat(40)));
        assert!(!valid_commit_id("main"));
        assert!(!valid_commit_id(&"a".repeat(39)));
        assert!(!valid_commit_id(&"g".repeat(40)));
    }

    #[test]
    fn status_payloads_validate() {
        let ok = |v: serde_json::Value| new_status(v.to_string().as_bytes()).is_ok();
        assert!(ok(serde_json::json!({"context": "ci", "state": "success"})));
        assert!(ok(serde_json::json!({
            "context": "ci", "state": "failure",
            "description": "tests failed", "target_url": "https://ci.example/1"
        })));
        assert!(ok(serde_json::json!({
            "context": "ci", "state": "failure", "target_url": "http://ci.example/1"
        })));
        assert!(!ok(serde_json::json!({"context": "", "state": "success"})));
        assert!(!ok(serde_json::json!({"context": "ci", "state": "nope"})));
        assert!(!ok(serde_json::json!({"context": "ci"})));
        assert!(!ok(
            serde_json::json!({"context": "ci", "state": "success", "extra": 1})
        ));
        // The reserved legacy context is never postable.
        assert!(!ok(serde_json::json!({
            "context": LEGACY_REQUIRE_CHECKS_CONTEXT, "state": "success"
        })));
    }

    #[test]
    fn target_url_scheme_is_restricted_to_http_and_https() {
        assert!(valid_target_url("https://ci.example/1"));
        assert!(valid_target_url("http://ci.example/1"));
        assert!(valid_target_url("HTTPS://ci.example/1"));
        assert!(!valid_target_url("javascript:alert(1)"));
        assert!(!valid_target_url("ftp://ci.example/1"));
        assert!(!valid_target_url("data:text/html,hello"));
        assert!(!valid_target_url(""));
        assert!(!valid_target_url("http"));
    }

    #[test]
    fn legacy_context_payload_returns_unprocessable() {
        let err = new_status(
            serde_json::json!({"context": LEGACY_REQUIRE_CHECKS_CONTEXT, "state": "success"})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn surrounding_whitespace_context_returns_unprocessable() {
        for context in [" ci", "ci ", " ci "] {
            let err = new_status(
                serde_json::json!({"context": context, "state": "success"})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap_err();
            assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn unsafe_target_url_returns_unprocessable() {
        let err = new_status(
            serde_json::json!({"context": "ci", "state": "success", "target_url": "javascript:alert(1)"})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn legacy_reserved_context_never_passes_checks() {
        // A rule migrated from require_checks=true requires the reserved
        // context, and no status can ever be posted under it, so it blocks the
        // merge even when other contexts pass.
        let required = [LEGACY_REQUIRE_CHECKS_CONTEXT.to_owned()];
        assert!(!all_success(&required, &[]));
        assert!(!all_success(
            &[LEGACY_REQUIRE_CHECKS_CONTEXT.to_owned(), "ci".to_owned()],
            &[("ci".into(), "success".into())],
        ));
    }

    #[test]
    fn all_success_requires_every_context_to_be_success() {
        let required = ["ci".to_owned(), "lint".to_owned()];
        assert!(all_success(
            &required,
            &[
                ("ci".into(), "success".into()),
                ("lint".into(), "success".into()),
            ]
        ));
        assert!(!all_success(
            &required,
            &[
                ("ci".into(), "success".into()),
                ("lint".into(), "failure".into()),
            ]
        ));
        // A missing context is not success.
        assert!(!all_success(&required, &[("ci".into(), "success".into())]));
        // Nothing required is vacuously passed.
        assert!(all_success(&[], &[]));
    }
}
