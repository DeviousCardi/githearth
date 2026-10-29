//! Merge intents (M3, Person A side of ADR 0004).
//!
//! An intent is the durable application-side record that stands in for an
//! atomic commit across PostgreSQL and Git. Creating it validates the pull
//! request, authorizes the merge through Cedar (with branch protection as
//! merge facts), and records the policy generation, the target tip it is
//! expected to be at, and a digest binding approvals/checks to the exact source
//! commit. The Git side (Person B) claims the intent, builds the candidate and
//! moves the ref; `record_merge_result` turns the journaled merge back into a
//! merged intent, idempotently.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Merge, PullRequest, Ref, Resource};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, browse, idempotency, invalid, issues, pulls, repositories,
    repository_api::{self, Loaded},
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge",
        axum::routing::post(create).get(read).delete(cancel),
    )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "merge intent not found")
}
fn not_open() -> ApiError {
    ApiError(StatusCode::CONFLICT, "pull request is not open")
}
fn head_moved() -> ApiError {
    ApiError(StatusCode::CONFLICT, "commit is not the pull request head")
}
fn branch_missing() -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, "branch not found")
}
fn already_queued() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "an open merge intent already exists for this pull request",
    )
}
fn executing() -> ApiError {
    ApiError(StatusCode::CONFLICT, "merge intent is already executing")
}

const INTENT_COLUMNS: &str = "operation_id, repository_id, pull_request_id, source_ref, source_oid, \
 target_ref, expected_target_oid, method, candidate_oid, policy_generation, checks_approvals_digest, \
 actor_id, state, lease_holder, lease_attempt, \
 to_char(lease_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS lease_expires_at, \
 result_oid, journal_sequence, failure_reason, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct MergeIntentJson {
    pub(crate) operation_id: Uuid,
    pub(crate) repository_id: Uuid,
    pub(crate) pull_request_id: Uuid,
    pub(crate) source_ref: String,
    pub(crate) source_oid: String,
    pub(crate) target_ref: String,
    pub(crate) expected_target_oid: String,
    pub(crate) method: String,
    pub(crate) candidate_oid: Option<String>,
    pub(crate) policy_generation: i64,
    pub(crate) checks_approvals_digest: String,
    pub(crate) actor_id: Uuid,
    pub(crate) state: String,
    pub(crate) lease_holder: Option<Uuid>,
    pub(crate) lease_attempt: Option<i32>,
    pub(crate) lease_expires_at: Option<String>,
    pub(crate) result_oid: Option<String>,
    pub(crate) journal_sequence: Option<i64>,
    pub(crate) failure_reason: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMerge {
    method: String,
    expected_head_oid: String,
}

fn new_merge(body: &[u8]) -> Result<NewMerge, ApiError> {
    let input: NewMerge = serde_json::from_slice(body).map_err(|_| invalid())?;
    let valid_method = matches!(
        input.method.as_str(),
        "merge" | "squash" | "rebase" | "ff_only"
    );
    let valid_oid = browse::is_commit_id(&input.expected_head_oid)
        && !input
            .expected_head_oid
            .bytes()
            .any(|b| b.is_ascii_uppercase());
    if !valid_method || !valid_oid {
        return Err(invalid());
    }
    Ok(input)
}

/// The reviewer ids whose latest non-comment review is at `source_oid`, sorted
/// by id, so the digest is independent of insertion order.
async fn latest_review_ids(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
    source_oid: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM (\
           SELECT DISTINCT ON (reviewer_id) id FROM pull_request_reviews \
           WHERE pull_request_id = $1 AND commit_id = $2 AND verdict <> 'commented' \
           ORDER BY reviewer_id, seq DESC\
         ) latest ORDER BY id",
    )
    .bind(pull_request_id)
    .bind(source_oid)
    .fetch_all(&mut *conn)
    .await
}

/// SHa256 over the source oid and the ids of each reviewer's latest non-comment
/// review at that oid, sorted. A new approval or change request (or a moved
/// head) changes the digest, making the intent stale. Check results contribute
/// nothing in M3 (no check system yet), so `require_checks` refuses every merge.
fn checks_approvals_digest(source_oid: &str, review_ids: &[Uuid]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_oid.as_bytes());
    for id in review_ids {
        hasher.update(id.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The number of approvals at `source_oid`, using the same latest-verdict logic
/// as the pull request detail.
async fn approvals_at(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
    source_oid: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM (\
           SELECT DISTINCT ON (reviewer_id) verdict FROM pull_request_reviews \
           WHERE pull_request_id = $1 AND commit_id = $2 AND verdict <> 'commented' \
           ORDER BY reviewer_id, seq DESC\
         ) latest WHERE verdict = 'approved'",
    )
    .bind(pull_request_id)
    .bind(source_oid)
    .fetch_one(&mut *conn)
    .await
}

/// Authorize the merge with the matching target-branch protection as merge
/// facts: approvals at the head and (in M3, always false) checks.
fn require_merge(
    state: &AppState,
    loaded: &Loaded,
    pull: &pulls::PullJson,
    protection: Option<githearth_authz::Protection>,
    approvals: u32,
    checks_passed: bool,
) -> Result<(), ApiError> {
    let authz_repo = repositories::authz_repository(&loaded.repo);
    let entity = PullRequest {
        id: pull.id,
        repository: loaded.repo.id,
        author: pull.author_id,
        target: Ref {
            repository: loaded.repo.id,
            name: format!("refs/heads/{}", pull.target_branch),
            protection,
            exists: true,
        },
    };
    let request = githearth_authz::Request {
        principal: githearth_authz::Principal::Account(&loaded.facts.account),
        teams: &loaded.facts.teams,
        grants: &loaded.facts.grants,
        action: Action::PullRequestMerge,
        organization: &loaded.facts.organization,
        resource: Resource::PullRequest(&authz_repo, &entity),
        merge: Some(Merge {
            approvals,
            checks_passed,
        }),
        auth_age: Some(Duration::from_secs(loaded.auth_age)),
    };
    let decision = state.authz.authorize(&request);
    if decision.is_allowed() {
        return Ok(());
    }
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

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<MergeIntentJson>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/merge");
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
            let input = new_merge(body)?;
            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
                    .await?;
            issues::require_readable_repo(&loaded)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(issues::not_active());
            }

            let mut conn = state.pool.acquire().await?;
            let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
                .await?
                .ok_or_else(not_found)?;
            // One snapshot, so the digest never pairs old rules with a new generation.
            let (rules, generation) =
                crate::protection::load_rules_with_generation(&mut conn, loaded.repo.id).await?;
            drop(conn);

            if pull.state != "open" {
                return Err(not_open());
            }

            let head = pulls::branch_tip(state, loaded.repo.id, &pull.source_branch).await?;
            let base = pulls::branch_tip(state, loaded.repo.id, &pull.target_branch).await?;
            let (Some(head), Some(base)) = (head, base) else {
                return Err(branch_missing());
            };
            if head != input.expected_head_oid {
                return Err(head_moved());
            }

            // Approvals and digest are computed at the exact source oid.
            let mut conn = state.pool.acquire().await?;
            let approvals = approvals_at(&mut conn, pull.id, &head).await?;
            let review_ids = latest_review_ids(&mut conn, pull.id, &head).await?;
            drop(conn);

            let protection =
                crate::protection::matching(&rules, &format!("refs/heads/{}", pull.target_branch));
            require_merge(state, &loaded, &pull, protection, approvals as u32, false)?;

            let digest = checks_approvals_digest(&head, &review_ids);
            let operation_id = Uuid::new_v4();

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(issues::not_active());
            }
            let inserted = sqlx::query(
                "INSERT INTO merge_intents \
                 (operation_id, repository_id, pull_request_id, source_ref, source_oid, \
                  target_ref, expected_target_oid, method, policy_generation, \
                  checks_approvals_digest, actor_id, state) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'queued')",
            )
            .bind(operation_id)
            .bind(loaded.repo.id)
            .bind(pull.id)
            .bind(&pull.source_branch)
            .bind(&head)
            .bind(&pull.target_branch)
            .bind(&base)
            .bind(&input.method)
            .bind(generation)
            .bind(&digest)
            .bind(actor.id)
            .execute(&mut *tx)
            .await;
            if let Err(e) = inserted {
                if e.as_database_error()
                    .is_some_and(|d| d.code().as_deref() == Some("23505"))
                {
                    return Err(already_queued());
                }
                return Err(e.into());
            }

            crate::event(
                &mut tx,
                Some(loaded.org_id),
                actor.id,
                "pull_request.merge_intent_created",
            )
            .await?;

            let intent: MergeIntentJson = sqlx::query_as(&format!(
                "SELECT {INTENT_COLUMNS} FROM merge_intents WHERE operation_id = $1"
            ))
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&intent).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

async fn find_intent_json(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
) -> Result<Option<MergeIntentJson>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {INTENT_COLUMNS} FROM merge_intents WHERE pull_request_id = $1 \
         ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(pull_request_id)
    .fetch_optional(&mut *conn)
    .await
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<MergeIntentJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded =
        repository_api::load_repo(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    issues::require_readable_repo(&loaded)?;
    let mut conn = state.pool.acquire().await?;
    let pull = pulls::find_pull(&mut conn, loaded.repo.id, number)
        .await?
        .ok_or_else(not_found)?;
    pulls::require_pull(&state, &loaded, &pull, Action::PullRequestRead)?;
    let intent = find_intent_json(&mut conn, pull.id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(intent))
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
) -> Result<Json<MergeIntentJson>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/merge");
    let state = &state;
    let (_, value) = idempotency::run(
        &state.pool,
        actor.id,
        "DELETE",
        &path_str,
        b"",
        None,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
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
            let locked: Option<(Uuid, String)> = sqlx::query_as(
                "SELECT operation_id, state FROM merge_intents \
                 WHERE pull_request_id = $1 AND state IN ('queued', 'validating', 'executing') \
                 ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
            )
            .bind(pull.id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((operation_id, state)) = locked else {
                return Err(not_found());
            };
            if state == "executing" {
                return Err(executing());
            }
            sqlx::query(
                "UPDATE merge_intents SET state = 'cancelled', updated_at = now() \
                         WHERE operation_id = $1 AND state IN ('queued', 'validating')",
            )
            .bind(operation_id)
            .execute(&mut *tx)
            .await?;
            crate::event(
                &mut tx,
                Some(loaded.org_id),
                actor.id,
                "pull_request.merge_intent_cancelled",
            )
            .await?;
            let intent: MergeIntentJson = sqlx::query_as(&format!(
                "SELECT {INTENT_COLUMNS} FROM merge_intents WHERE operation_id = $1"
            ))
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::OK;
            let body = serde_json::to_value(&intent).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok(Json(serde_json::from_value(value).unwrap()))
}

/// Records a merge whose Git ref move was journaled with this operation id.
/// Idempotent, keyed by `(repository_id, journal_sequence)`: a second call
/// changes nothing and returns Ok. In one transaction it:
///  - marks the intent `merged` with its result oid and journal sequence;
///  - marks the pull request `merged` with `merged_at`/`merged_by`;
///  - writes the ref-update, audit and outbox rows in the same style as pushes.
#[allow(clippy::too_many_arguments)]
pub async fn record_merge_result(
    conn: &mut PgConnection,
    repository_id: Uuid,
    journal_sequence: u64,
    operation_id: Uuid,
    actor: Uuid,
    old_oid: &str,
    new_oid: &str,
    ref_name: &str,
) -> Result<(), sqlx::Error> {
    let sequence = journal_sequence as i64;

    let mut tx = conn.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO repository_pushes (repository_id, journal_sequence, operation_id, actor_id) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(repository_id)
    .bind(sequence)
    .bind(operation_id)
    .bind(actor)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;

    if inserted {
        sqlx::query(
            "INSERT INTO ref_updates (repository_id, journal_sequence, ref_name, old_oid, new_oid) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(repository_id)
        .bind(sequence)
        .bind(ref_name)
        .bind(old_oid)
        .bind(new_oid)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE merge_intents SET state = 'merged', result_oid = $1, journal_sequence = $2, \
             updated_at = now() WHERE operation_id = $3 AND repository_id = $4",
        )
        .bind(new_oid)
        .bind(sequence)
        .bind(operation_id)
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE pull_requests SET state = 'merged', merged_at = now(), merged_by = $1, \
             updated_at = now() WHERE id = (SELECT pull_request_id FROM merge_intents \
             WHERE operation_id = $2) AND state = 'open'",
        )
        .bind(actor)
        .bind(operation_id)
        .execute(&mut *tx)
        .await?;

        let audit_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_events (id, organization_id, actor_id, kind) \
             SELECT $1, r.organization_id, $2, 'pull_request.merged' \
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(actor)
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO outbox_events (id, organization_id, kind, payload) \
             SELECT $1, r.organization_id, 'pull_request.merged', $2 \
             FROM repositories r WHERE r.id = $3",
        )
        .bind(audit_id)
        .bind(serde_json::json!({
            "repository_id": repository_id,
            "journal_sequence": sequence,
            "operation_id": operation_id,
            "actor_id": actor,
            "ref": ref_name,
            "old": old_oid,
            "new": new_oid,
        }))
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}
