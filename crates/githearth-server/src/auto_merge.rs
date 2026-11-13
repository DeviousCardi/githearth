//! Auto-merge (M3).
//!
//! A user with merge permission enables auto-merge on an open pull request,
//! choosing a merge method. The server raises a merge intent automatically once
//! the pull request becomes mergeable (approvals, required checks, and, when a
//! branch protection rule demands it, a merge-queue entry). Auto-merge is
//! cancelled when the head changes by someone other than the enabler, when the
//! pull request closes, or when the enabler loses permission; a cancelled (or
//! merged) auto-merge records an audit event.
//!
//! Raising the intent reuses [`crate::merge_intents::create_intent`], so the
//! intent is validated, authorized and audited exactly like a manual merge. The
//! digest recorded by that path still binds the approvals/checks to the exact
//! source head, so a stale approval can never authorize a moved head.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, Transaction};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, idempotency, invalid, issues, merge_intents, pulls,
    repositories, repository_api,
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/auto_merge",
        axum::routing::post(enable).get(read).delete(cancel),
    )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "auto-merge not found")
}
fn branch_missing() -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, "branch not found")
}

const AUTO_MERGE_COLUMNS: &str = "pull_request_id, method, source_oid, enabled_by, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct AutoMergeJson {
    pub(crate) pull_request_id: Uuid,
    pub(crate) method: String,
    pub(crate) source_oid: String,
    pub(crate) enabled_by: Uuid,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAutoMerge {
    method: String,
}

fn new_auto_merge(body: &[u8]) -> Result<NewAutoMerge, ApiError> {
    let input: NewAutoMerge = serde_json::from_slice(body).map_err(|_| invalid())?;
    if !matches!(
        input.method.as_str(),
        "merge" | "squash" | "rebase" | "ff_only"
    ) {
        return Err(invalid());
    }
    Ok(input)
}

pub(crate) async fn find_auto_merge(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
) -> Result<Option<AutoMergeJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {AUTO_MERGE_COLUMNS} FROM auto_merges WHERE pull_request_id = $1"
    ))
    .bind(pull_request_id)
    .fetch_optional(&mut *conn)
    .await
}

/// Whether `actor_id` may enable auto-merge on this pull request right now:
/// merge permission, checked independent of the current approvals (which is
/// what `merge_intents::may_merge` folds in). Reports false for a closed or
/// merged pull request too.
pub(crate) async fn may_enable(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
) -> Result<bool, ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    issues::require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    if pull.state != "open" {
        return Ok(false);
    }
    if pull.draft {
        return Ok(false);
    }
    let (rules, _generation) =
        crate::protection::load_rules_with_generation(&mut conn, loaded.repo.id).await?;
    drop(conn);

    // A deleted source branch has no head; enabling auto-merge would refuse
    // with 422 "branch not found", so the form must not offer it.
    if pulls::branch_tip(state, pull.source_repository_id, &pull.source_branch)
        .await?
        .is_none()
    {
        return Ok(false);
    }

    let target_ref = format!("refs/heads/{}", pull.target_branch);
    let protection = crate::protection::matching(&rules, &target_ref);
    let required = protection.as_ref().map_or(0, |p| p.required_approvals);
    Ok(merge_intents::merge_allowed(
        state,
        &loaded.facts,
        &loaded.repo,
        &pull,
        protection,
        required,
        true,
        true,
        Some(Duration::from_secs(auth_age)),
    ))
}

/// The merge permission check for enabling: satisfied approval/check facts, so
/// the decision hinges on the account's grants alone. A denial reads as 403 to
/// a reader and 404 to anyone else, matching [`merge_intents::require_merge`].
fn require_enable_permission(
    state: &AppState,
    loaded: &repository_api::Loaded,
    pull: &pulls::PullJson,
    protection: Option<githearth_authz::Protection>,
) -> Result<(), ApiError> {
    let required = protection.as_ref().map_or(0, |p| p.required_approvals);
    if merge_intents::merge_allowed(
        state,
        &loaded.facts,
        &loaded.repo,
        pull,
        protection,
        required,
        true,
        true,
        Some(Duration::from_secs(loaded.auth_age)),
    ) {
        return Ok(());
    }
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let read_ok = state
        .authz
        .authorize(&loaded.facts.request(
            Action::RepositoryRead,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed();
    if read_ok {
        Err(ApiError(StatusCode::FORBIDDEN, "not permitted"))
    } else {
        Err(not_found())
    }
}

async fn enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<AutoMergeJson>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, am) = enable_auto_merge(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, &body, key,
    )
    .await?;
    Ok((status, Json(am)))
}

/// Enables (or re-enables, choosing a fresh method and head) auto-merge as
/// `actor_id`. Shared by the JSON handler and the browser form.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enable_auto_merge(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, AutoMergeJson), ApiError> {
    let input = new_auto_merge(body)?;
    let path_str = format!(
        "/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/auto_merge"
    );
    let (status, value) = idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            issues::require_readable_repo(&loaded)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(issues::not_active());
            }

            let mut conn = state.pool.acquire().await?;
            let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
                .await?
                .ok_or_else(not_found)?;
            drop(conn);
            if pull.state != "open" {
                return Err(pulls::not_open());
            }
            if pull.draft {
                return Err(pulls::not_ready());
            }

            let mut conn = state.pool.acquire().await?;
            let (rules, _generation) =
                crate::protection::load_rules_with_generation(&mut conn, loaded.repo.id).await?;
            drop(conn);
            let target_ref = format!("refs/heads/{}", pull.target_branch);
            let protection = crate::protection::matching(&rules, &target_ref);
            require_enable_permission(state, &loaded, &pull, protection)?;

            let head =
                pulls::branch_tip(state, pull.source_repository_id, &pull.source_branch).await?;
            let Some(head) = head else {
                return Err(branch_missing());
            };

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(issues::not_active());
            }
            pulls::require_open(&mut tx, pull.id).await?;
            sqlx::query(
                "INSERT INTO auto_merges (pull_request_id, method, source_oid, enabled_by) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (pull_request_id) DO UPDATE \
                 SET method = $2, source_oid = $3, enabled_by = $4, updated_at = now()",
            )
            .bind(pull.id)
            .bind(&input.method)
            .bind(&head)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?;
            crate::event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "pull_request.auto_merge_enabled",
            )
            .await?;

            let row: AutoMergeJson = sqlx::query_as(&format!(
                "SELECT {AUTO_MERGE_COLUMNS} FROM auto_merges WHERE pull_request_id = $1"
            ))
            .bind(pull.id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&row).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    let am: AutoMergeJson = serde_json::from_value(value).unwrap();
    // If the pull request is already mergeable, raise the intent right away.
    let _ = maybe_auto_merge(state, actor_id, am.pull_request_id).await;
    Ok((status, am))
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<AutoMergeJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    issues::require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    pulls::require_pull(&state, &loaded, &pull, Action::PullRequestRead)?;
    let am = find_auto_merge(&mut conn, pull.id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(am))
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<AutoMergeJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    cancel_auto_merge(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, None,
    )
    .await
    .map(Json)
}

/// Cancels the auto-merge on `number` as `actor_id`. Shared by the JSON DELETE
/// handler and the browser cancel form (which uses the same idempotency layer
/// with its per-form key).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cancel_auto_merge(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    key: Option<&str>,
) -> Result<AutoMergeJson, ApiError> {
    let path_str = format!(
        "/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/auto_merge"
    );
    let (_, value) = idempotency::run(
        &state.pool,
        actor_id,
        "DELETE",
        &path_str,
        b"",
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            issues::require_readable_repo(&loaded)?;

            let mut conn = state.pool.acquire().await?;
            let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
                .await?
                .ok_or_else(not_found)?;
            drop(conn);
            pulls::require_pull(state, &loaded, &pull, Action::PullRequestClose)?;

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(issues::not_active());
            }
            let row: Option<AutoMergeJson> = sqlx::query_as(&format!(
                "SELECT {AUTO_MERGE_COLUMNS} FROM auto_merges WHERE pull_request_id = $1 FOR UPDATE"
            ))
            .bind(pull.id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return Err(not_found());
            };
            cancel_in_tx(&mut tx, loaded.org_id, actor_id, pull.id, None).await?;

            let status = StatusCode::OK;
            let body = serde_json::to_value(&row).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok(serde_json::from_value(value).unwrap())
}

/// Deletes the auto-merge row and records the cancellation audit event, all in
/// the caller's transaction. Returns whether a row was actually deleted.
/// `reason`, when set, names why the auto-merge was cancelled; an
/// auto-cancellation passes it, while a user's manual cancel passes `None`.
pub(crate) async fn cancel_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    actor: Uuid,
    pull_request_id: Uuid,
    reason: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM auto_merges WHERE pull_request_id = $1")
        .bind(pull_request_id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        > 0;
    if deleted {
        let mut extra = serde_json::json!({ "pull_request_id": pull_request_id });
        if let Some(reason) = reason {
            extra["reason"] = serde_json::json!(reason);
        }
        crate::event_with(
            tx,
            Some(org_id),
            actor,
            "pull_request.auto_merge_cancelled",
            extra,
        )
        .await?;
    }
    Ok(deleted)
}

/// The state-change trigger for auto-merge: called after enabling, after a
/// review lands, and after the enabler's own push re-targets the recorded head.
/// `actor` is the account whose action triggered the re-check; it is named in
/// any auto-cancellation audit event. It raises the intent when the pull
/// request is now mergeable, and cancels the auto-merge when it is no longer
/// eligible. Best-effort on purpose: every side effect here is also re-checked
/// by the merge worker, so a failure just leaves the work to the next trigger.
pub(crate) async fn maybe_auto_merge(
    state: &AppState,
    actor: Uuid,
    pull_request_id: Uuid,
) -> Result<(), ApiError> {
    let mut conn = state.pool.acquire().await?;
    let am: Option<(String, String, Uuid)> = sqlx::query_as(
        "SELECT method, source_oid, enabled_by FROM auto_merges WHERE pull_request_id = $1",
    )
    .bind(pull_request_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((method, source_oid, enabled_by)) = am else {
        return Ok(());
    };
    let repository_id: Option<Uuid> =
        sqlx::query_scalar("SELECT repository_id FROM pull_requests WHERE id = $1")
            .bind(pull_request_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(repository_id) = repository_id else {
        return Ok(());
    };
    let Some(repo) = repositories::get(&mut conn, repository_id).await? else {
        return Ok(());
    };
    let org_slug: Option<String> =
        sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
            .bind(repo.organization_id)
            .fetch_optional(&mut *conn)
            .await?;
    drop(conn);
    let Some(org_slug) = org_slug else {
        return Ok(());
    };

    let mut conn = state.pool.acquire().await?;
    let Some(pull) = pulls::fetch_pull(&mut conn, pull_request_id).await? else {
        return Ok(());
    };
    let number = pull.number;
    drop(conn);

    // A closed or merged pull request no longer has a pending auto-merge.
    if pull.state != "open" {
        let mut tx = state.pool.begin().await?;
        cancel_in_tx(
            &mut tx,
            repo.organization_id,
            actor,
            pull_request_id,
            Some("pull request closed"),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    // A draft is not ready to merge; cancel any pending auto-merge, like a
    // close (ADR 0009). `set_draft` also cancels it, so this is a backstop for
    // a ready-to-draft toggle that raced a trigger.
    if pull.draft {
        let mut tx = state.pool.begin().await?;
        cancel_in_tx(
            &mut tx,
            repo.organization_id,
            actor,
            pull_request_id,
            Some("pull request is a draft"),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    // A head that moved away from the recorded source oid (source deleted, or
    // a push by someone other than the enabler) cancels the auto-merge. The
    // enabler's own push re-targets the row in `on_push`, so it never lands
    // here as a mismatch.
    let head = pulls::branch_tip(state, pull.source_repository_id, &pull.source_branch).await?;
    if head.as_deref() != Some(source_oid.as_str()) {
        let mut tx = state.pool.begin().await?;
        cancel_in_tx(
            &mut tx,
            repo.organization_id,
            actor,
            pull_request_id,
            Some("head changed"),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    let mut conn = state.pool.acquire().await?;
    let (rules, _generation) =
        crate::protection::load_rules_with_generation(&mut conn, repository_id).await?;
    drop(conn);
    let target_ref = format!("refs/heads/{}", pull.target_branch);
    let protection = crate::protection::matching(&rules, &target_ref);
    let required = protection.as_ref().map_or(0, |p| p.required_approvals);

    let mut conn = state.pool.acquire().await?;
    let Some(facts) = crate::facts::load(&mut conn, enabled_by, repo.organization_id).await? else {
        // The enabler no longer resolves in this organization.
        drop(conn);
        let mut tx = state.pool.begin().await?;
        cancel_in_tx(
            &mut tx,
            repo.organization_id,
            actor,
            pull_request_id,
            Some("enabler removed"),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    };
    drop(conn);

    // Lost permission (independent of approvals/checks): cancel.
    if !merge_intents::merge_allowed(
        state, &facts, &repo, &pull, protection, required, true, true, None,
    ) {
        let mut tx = state.pool.begin().await?;
        cancel_in_tx(
            &mut tx,
            repo.organization_id,
            actor,
            pull_request_id,
            Some("permission lost"),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }

    // Approvals/checks may still be unsatisfied; `create_intent` raises the
    // intent exactly when they are, and every other refusal (already queued,
    // head raced, branch missing) just means "not now".
    let Some(head) = head else {
        return Ok(());
    };
    let input = merge_intents::NewMerge {
        method,
        expected_head_oid: head.clone(),
    };
    let body = serde_json::json!({
        "method": &input.method,
        "expected_head_oid": &input.expected_head_oid,
    })
    .to_string();
    // A conflicting merge is recorded once as a `conflicted` intent and later
    // triggers reuse it (see `create_intent`); the auto-merge row stays
    // enabled so a push that resolves the conflict raises a fresh intent.
    let _ = merge_intents::create_intent(
        state,
        enabled_by,
        0,
        &org_slug,
        &repo.name,
        number,
        body.as_bytes(),
        None,
        &input,
    )
    .await;
    Ok(())
}

/// A commit status landed for `oid`. Re-drives auto-merge for every open pull
/// request whose recorded source head is that commit, because a required check
/// may have just satisfied the last remaining requirement; this raises the
/// intent now instead of waiting for the next review or push. Best-effort like
/// the other triggers: the merge worker re-checks as a backstop.
pub(crate) async fn on_status(
    state: &AppState,
    repository_id: Uuid,
    oid: &str,
    actor: Uuid,
) -> Result<(), ApiError> {
    let mut conn = state.pool.acquire().await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT am.pull_request_id FROM auto_merges am \
         JOIN pull_requests pr ON pr.id = am.pull_request_id \
         WHERE pr.repository_id = $1 AND pr.state = 'open' AND NOT pr.draft \
           AND am.source_oid = $2",
    )
    .bind(repository_id)
    .bind(oid)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);
    for id in ids {
        let _ = maybe_auto_merge(state, actor, id).await;
    }
    Ok(())
}

/// A push landed on `ref_name`. For every open pull request in this repository
/// whose source branch is that ref with a pending auto-merge: the enabler's own
/// push re-targets the recorded head; anyone else's push cancels the auto-merge.
/// Returns the pull request ids whose auto-merge was re-targeted (the enabler's
/// own push), so the caller can re-drive [`maybe_auto_merge`] afterward.
pub(crate) async fn on_push(
    tx: &mut Transaction<'_, Postgres>,
    repository_id: Uuid,
    ref_name: &str,
    new_oid: &str,
    actor: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let Some(branch) = ref_name.strip_prefix("refs/heads/") else {
        return Ok(Vec::new());
    };
    let retargeted: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE auto_merges SET source_oid = $1, updated_at = now() \
         WHERE pull_request_id IN (SELECT id FROM pull_requests pr \
             WHERE pr.source_repository_id = $2 AND pr.source_branch = $3 \
               AND pr.state = 'open' AND NOT pr.draft) \
           AND enabled_by = $4 \
         RETURNING pull_request_id",
    )
    .bind(new_oid)
    .bind(repository_id)
    .bind(branch)
    .bind(actor)
    .fetch_all(&mut **tx)
    .await?;

    let cancelled: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM auto_merges \
         WHERE pull_request_id IN (SELECT id FROM pull_requests pr \
             WHERE pr.source_repository_id = $1 AND pr.source_branch = $2 \
               AND pr.state = 'open' AND NOT pr.draft) \
           AND enabled_by <> $3 \
         RETURNING pull_request_id",
    )
    .bind(repository_id)
    .bind(branch)
    .bind(actor)
    .fetch_all(&mut **tx)
    .await?;

    if !cancelled.is_empty() {
        for pull_request_id in cancelled {
            // Audit under the pull request's own (target) repository's
            // organization: for a cross-repository pull the push lands on the
            // fork, a different organization than the pull belongs to.
            let org_id: Uuid = sqlx::query_scalar(
                "SELECT r.organization_id FROM pull_requests pr \
                 JOIN repositories r ON r.id = pr.repository_id WHERE pr.id = $1",
            )
            .bind(pull_request_id)
            .fetch_one(&mut **tx)
            .await?;
            crate::event_with(
                tx,
                Some(org_id),
                actor,
                "pull_request.auto_merge_cancelled",
                serde_json::json!({ "pull_request_id": pull_request_id, "reason": "head changed" }),
            )
            .await?;
        }
    }
    Ok(retargeted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_merge_accepts_only_known_methods() {
        for method in ["merge", "squash", "rebase", "ff_only"] {
            let body = serde_json::json!({ "method": method }).to_string();
            assert!(new_auto_merge(body.as_bytes()).is_ok(), "{method}");
        }
        for body in [
            r#"{"method":"fast-forward"}"#,
            r#"{"method":""}"#,
            r#"{"method":"MERGE"}"#,
            r#"{"unexpected":true}"#,
        ] {
            assert!(new_auto_merge(body.as_bytes()).is_err(), "{body}");
        }
    }
}
