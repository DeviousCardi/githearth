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
    statuses,
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
 actor_id, state, in_queue, lease_holder, lease_attempt, \
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
    pub(crate) in_queue: bool,
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
pub(crate) struct NewMerge {
    pub(crate) method: String,
    pub(crate) expected_head_oid: String,
}

pub(crate) fn new_merge(body: &[u8]) -> Result<NewMerge, ApiError> {
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
pub(crate) async fn latest_review_ids(
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

/// SHA256 over the source oid, the ids of each reviewer's latest non-comment
/// review at that oid (sorted), and the latest `(context, state)` of each
/// required status check at that oid (sorted). A new approval or change
/// request, a moved head, or a change to any required check result changes the
/// digest, making the intent stale.
pub(crate) fn checks_approvals_digest(
    source_oid: &str,
    review_ids: &[Uuid],
    check_states: &[(String, String)],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_oid.as_bytes());
    for id in review_ids {
        hasher.update(id.as_bytes());
    }
    for (context, state) in check_states {
        hasher.update(context.as_bytes());
        hasher.update(state.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The number of approvals at `source_oid`, using the same latest-verdict logic
/// as the pull request detail.
pub(crate) async fn approvals_at(
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

/// Builds and evaluates the `pull_request.merge` request shared by the API
/// handler and [`crate::merge::execute_intent`]. The merge facts are the
/// approvals at the head and whether every required status check is `success`
/// at the head, with the matching target-branch protection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn merge_allowed(
    state: &AppState,
    facts: &crate::facts::Facts,
    repo: &repositories::RepositoryRow,
    pull: &pulls::PullJson,
    protection: Option<githearth_authz::Protection>,
    approvals: u32,
    checks_passed: bool,
    auth_age: Option<Duration>,
) -> bool {
    let authz_repo = repositories::authz_repository(repo);
    let entity = PullRequest {
        id: pull.id,
        repository: repo.id,
        author: pull.author_id,
        target: Ref {
            repository: repo.id,
            name: format!("refs/heads/{}", pull.target_branch),
            protection,
            exists: true,
        },
    };
    let request = githearth_authz::Request {
        principal: githearth_authz::Principal::Account(&facts.account),
        teams: &facts.teams,
        grants: &facts.grants,
        action: Action::PullRequestMerge,
        organization: &facts.organization,
        resource: Resource::PullRequest(&authz_repo, &entity),
        merge: Some(Merge {
            approvals,
            checks_passed,
        }),
        auth_age,
        read_only: facts.read_only,
    };
    state.authz.authorize(&request).is_allowed()
}

/// Authorize the merge with the matching target-branch protection as merge
/// facts: approvals at the head and whether every required status check is
/// `success` at the head.
fn require_merge(
    state: &AppState,
    loaded: &Loaded,
    pull: &pulls::PullJson,
    protection: Option<githearth_authz::Protection>,
    approvals: u32,
    checks_passed: bool,
) -> Result<(), ApiError> {
    if merge_allowed(
        state,
        &loaded.facts,
        &loaded.repo,
        pull,
        protection,
        approvals,
        checks_passed,
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

/// Whether `actor_id` may merge this pull request right now, for the browser
/// merge form: it reports false for a closed or merged pull request and for a
/// viewer whose grant does not permit `pull_request.merge`. Branch protection
/// merge facts are evaluated exactly as the create path does.
pub(crate) async fn may_merge(
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
    let (rules, _generation) =
        crate::protection::load_rules_with_generation(&mut conn, loaded.repo.id).await?;
    drop(conn);

    let head = pulls::branch_tip(state, loaded.repo.id, &pull.source_branch).await?;
    let target_ref = format!("refs/heads/{}", pull.target_branch);
    let required_checks = crate::protection::required_status_checks(&rules, &target_ref);
    // A deleted source branch has no head; create_intent would refuse this
    // merge with 422 "branch not found", so the merge form must not offer it.
    let Some(head) = head.as_deref() else {
        return Ok(false);
    };
    let mut conn = state.pool.acquire().await?;
    let approvals = approvals_at(&mut conn, pull.id, head).await? as u32;
    let checks_passed =
        statuses::checks_passed(&mut conn, loaded.repo.id, head, &required_checks).await?;
    drop(conn);
    let protection = crate::protection::matching(&rules, &target_ref);
    Ok(merge_allowed(
        state,
        &loaded.facts,
        &loaded.repo,
        &pull,
        protection,
        approvals,
        checks_passed,
        Some(Duration::from_secs(auth_age)),
    ))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, number)): Path<(String, String, i32)>,
    body: Bytes,
) -> Result<(StatusCode, Json<MergeIntentJson>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let input = new_merge(&body)?;
    let (status, intent) = create_intent(
        &state, actor.id, auth_age, &org_slug, &repo_name, number, &body, key, &input,
    )
    .await?;
    Ok((status, Json(intent)))
}

/// Creates (or replays, through the idempotency key) a merge intent, shared by
/// the JSON handler and the browser merge form. Validation happens only in
/// [`new_merge`]; behaviour is identical for both callers.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_intent(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    number: i32,
    body: &[u8],
    key: Option<&str>,
    input: &NewMerge,
) -> Result<(StatusCode, MergeIntentJson), ApiError> {
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls/{number}/merge");
    let state = &state;
    let body = &body;
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
            // A closed or merged pull request must fail here, before branch
            // lookups or merge authorization, with the shared "not open" error.
            if pull.state != "open" {
                return Err(pulls::not_open());
            }
            // One snapshot, so the digest never pairs old rules with a new generation.
            let (rules, generation) =
                crate::protection::load_rules_with_generation(&mut conn, loaded.repo.id).await?;
            drop(conn);

            let head = pulls::branch_tip(state, loaded.repo.id, &pull.source_branch).await?;
            let base = pulls::branch_tip(state, loaded.repo.id, &pull.target_branch).await?;
            let (Some(head), Some(base)) = (head, base) else {
                return Err(branch_missing());
            };
            if head != input.expected_head_oid {
                return Err(head_moved());
            }

            // Approvals, required-check results and the digest are computed at
            // the exact source oid, so a status for a different commit can
            // never authorize this merge.
            let target_ref = format!("refs/heads/{}", pull.target_branch);
            let required_checks = crate::protection::required_status_checks(&rules, &target_ref);
            let mut conn = state.pool.acquire().await?;
            let approvals = approvals_at(&mut conn, pull.id, &head).await?;
            let review_ids = latest_review_ids(&mut conn, pull.id, &head).await?;
            let check_states = statuses::latest_required_states(
                &mut conn,
                loaded.repo.id,
                &head,
                &required_checks,
            )
            .await?;
            drop(conn);

            let protection = crate::protection::matching(&rules, &target_ref);
            let checks_passed = statuses::all_success(&required_checks, &check_states);
            require_merge(
                state,
                &loaded,
                &pull,
                protection,
                approvals as u32,
                checks_passed,
            )?;

            // A branch behind a merge queue enqueues its intent instead of
            // merging directly; the worker re-targets it as earlier entries merge.
            let in_queue = crate::protection::requires_queue(&rules, &target_ref);

            let digest = checks_approvals_digest(&head, &review_ids, &check_states);
            let operation_id = Uuid::new_v4();

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(issues::not_active());
            }
            // Re-check under the pull request row lock that a concurrent close
            // could not slip in between the earlier read and this insert.
            pulls::require_open(&mut tx, pull.id).await?;
            let inserted = sqlx::query(
                "INSERT INTO merge_intents \
                 (operation_id, repository_id, pull_request_id, source_ref, source_oid, \
                  target_ref, expected_target_oid, method, policy_generation, \
                  checks_approvals_digest, actor_id, state, in_queue) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'queued', $12)",
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
            .bind(actor_id)
            .bind(in_queue)
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
                actor_id,
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
    Ok((status, serde_json::from_value(value).unwrap()))
}

pub(crate) async fn find_intent_json(
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

/// The 1-based position of this pull request's live merge-queue entry among the
/// other live queue entries for the same target branch, ordered by creation
/// time with `operation_id` as a deterministic tie-break for entries created in
/// the same instant. `None` when the pull request has no live queue entry.
pub(crate) async fn queue_position(
    conn: &mut PgConnection,
    pull_request_id: Uuid,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM merge_intents other \
            WHERE other.repository_id = cur.repository_id \
              AND other.target_ref = cur.target_ref AND other.in_queue \
              AND other.state IN ('queued', 'validating', 'executing') \
              AND (other.created_at, other.operation_id) \
                  <= (cur.created_at, cur.operation_id)) \
         FROM merge_intents cur \
         WHERE cur.pull_request_id = $1 AND cur.in_queue \
           AND cur.state IN ('queued', 'validating', 'executing')",
    )
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
///  - consumes any pending auto-merge, recording its completion audit event;
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

        // The ref has moved, so Git won: a close that committed after
        // execute_intent revalidated still ends with the pull request merged.
        // Capture the pull request id and whether it actually transitioned to
        // `merged`, so the notification job is enqueued only on a real merge.
        let merged_pr: Option<Uuid> = sqlx::query_scalar(
            "UPDATE pull_requests SET state = 'merged', merged_at = now(), merged_by = $1, \
             updated_at = now() WHERE id = (SELECT pull_request_id FROM merge_intents \
             WHERE operation_id = $2) AND state IN ('open', 'closed') RETURNING id",
        )
        .bind(actor)
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await?;

        let organization_id: Option<Uuid> =
            sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id = $1")
                .bind(repository_id)
                .fetch_one(&mut *tx)
                .await?;

        // A merged pull request no longer has a pending auto-merge; record its
        // completion so the auto-merge audit trail ends with an event rather
        // than a silently vanished row.
        let completed_auto_merge: Option<Uuid> = sqlx::query_scalar(
            "DELETE FROM auto_merges WHERE pull_request_id = \
             (SELECT pull_request_id FROM merge_intents WHERE operation_id = $1) \
             RETURNING pull_request_id",
        )
        .bind(operation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(pull_request_id) = completed_auto_merge {
            crate::event_with(
                &mut tx,
                organization_id,
                actor,
                "pull_request.auto_merge_completed",
                serde_json::json!({ "pull_request_id": pull_request_id }),
            )
            .await?;
        }

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

        // Notification: only when this transaction actually merged the pull
        // request, and only for a pull request that exists at this point.
        if let (Some(org_id), Some(pull_request_id)) = (organization_id, merged_pr) {
            crate::jobs::enqueue(
                &mut tx,
                Some(org_id),
                "notification.pull_request_merged",
                serde_json::json!({
                    "event_id": audit_id,
                    "pull_request_id": pull_request_id,
                }),
                &audit_id.to_string(),
                None,
            )
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn create_locks_and_rejects_a_closed_pull_request() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("mi-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(format!("o-{}", Uuid::new_v4().simple()))
        .execute(&pool)
        .await
        .unwrap();
        let repo = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
             VALUES ($1,$2,'r',$3,$4,'active','private')",
        )
        .bind(repo)
        .bind(org)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        let pull = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
             VALUES ($1,$2,1,'t','',$3,'src','dst')",
        )
        .bind(pull)
        .bind(repo)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();

        // The pull request is open, so the re-check passes.
        let mut conn = pool.acquire().await.unwrap();
        pulls::require_open(&mut conn, pull).await.unwrap();

        // A close committing before the intent insert leaves the pull request
        // closed; the row-locked re-check must refuse it rather than queueing.
        sqlx::query("UPDATE pull_requests SET state = 'closed', closed_at = now() WHERE id = $1")
            .bind(pull)
            .execute(&pool)
            .await
            .unwrap();
        let error = pulls::require_open(&mut conn, pull).await.unwrap_err();
        assert_eq!(error.status_code(), StatusCode::CONFLICT);
        assert_eq!(error.message(), "pull request is not open");

        let intents: i64 =
            sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE pull_request_id = $1")
                .bind(pull)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(intents, 0);

        // close() waits for every checked-out connection, so return ours first.
        drop(conn);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn queue_position_orders_live_entries_per_target_branch() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("q-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(format!("o-{}", Uuid::new_v4().simple()))
        .execute(&pool)
        .await
        .unwrap();
        let repo = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
             VALUES ($1,$2,'r',$3,$4,'active','private')",
        )
        .bind(repo)
        .bind(org)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        async fn pull(
            pool: &sqlx::PgPool,
            repo: Uuid,
            account: Uuid,
            src: &str,
            number: i32,
        ) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
                 VALUES ($1,$2,$3,'t','',$4,$5,'main')",
            )
            .bind(id)
            .bind(repo)
            .bind(number)
            .bind(account)
            .bind(src)
            .execute(pool)
            .await
            .unwrap();
            id
        }
        async fn intent(
            pool: &sqlx::PgPool,
            pull: Uuid,
            repo: Uuid,
            account: Uuid,
            in_queue: bool,
        ) {
            sqlx::query(
                "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
                 source_oid, target_ref, expected_target_oid, method, policy_generation, \
                 checks_approvals_digest, actor_id, state, in_queue) \
                 VALUES ($1,$2,$3,$4,$5,'main',$5,'merge',0,'d',$6,'queued',$7)",
            )
            .bind(Uuid::new_v4())
            .bind(repo)
            .bind(pull)
            .bind("src")
            .bind("a".repeat(40))
            .bind(account)
            .bind(in_queue)
            .execute(pool)
            .await
            .unwrap();
        }

        // Two live queue entries on main, plus a non-queue entry on main that
        // must not count, and a queue entry on another target.
        let a = pull(&pool, repo, account, "a", 1).await;
        let b = pull(&pool, repo, account, "b", 2).await;
        let c = pull(&pool, repo, account, "c", 3).await;
        let d = pull(&pool, repo, account, "d", 4).await;
        intent(&pool, a, repo, account, true).await;
        intent(&pool, b, repo, account, true).await;
        intent(&pool, c, repo, account, false).await;
        sqlx::query(
            "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
             source_oid, target_ref, expected_target_oid, method, policy_generation, \
             checks_approvals_digest, actor_id, state, in_queue) \
             VALUES ($1,$2,$3,'src',$4,'other',$4,'merge',0,'d',$5,'queued',true)",
        )
        .bind(Uuid::new_v4())
        .bind(repo)
        .bind(d)
        .bind("a".repeat(40))
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(queue_position(&mut conn, a).await.unwrap(), Some(1));
        assert_eq!(queue_position(&mut conn, b).await.unwrap(), Some(2));
        // `c` has no queue entry.
        assert_eq!(queue_position(&mut conn, c).await.unwrap(), None);
        // `d` is on a different target, alone in its own queue.
        assert_eq!(queue_position(&mut conn, d).await.unwrap(), Some(1));
        drop(conn);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn queue_position_tie_breaks_entries_with_the_same_timestamp() {
        use sqlx::{ConnectOptions, postgres::PgPoolOptions};
        use std::str::FromStr;

        let url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let options = sqlx::postgres::PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();

        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
            .bind(account)
            .bind(format!("t-{}", Uuid::new_v4().simple()))
            .execute(&pool)
            .await
            .unwrap();
        let org = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')",
        )
        .bind(org)
        .bind(format!("o-{}", Uuid::new_v4().simple()))
        .execute(&pool)
        .await
        .unwrap();
        let repo = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
             VALUES ($1,$2,'r',$3,$4,'active','private')",
        )
        .bind(repo)
        .bind(org)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        async fn pull(pool: &sqlx::PgPool, repo: Uuid, account: Uuid, number: i32) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
                 VALUES ($1,$2,$3,'t','',$4,$5,'main')",
            )
            .bind(id)
            .bind(repo)
            .bind(number)
            .bind(account)
            .bind(format!("src-{number}"))
            .execute(pool)
            .await
            .unwrap();
            id
        }

        async fn intent_at(
            pool: &sqlx::PgPool,
            pull: Uuid,
            repo: Uuid,
            account: Uuid,
            operation: Uuid,
        ) {
            sqlx::query(
                "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
                 source_oid, target_ref, expected_target_oid, method, policy_generation, \
                 checks_approvals_digest, actor_id, state, in_queue, created_at) \
                 VALUES ($1,$2,$3,'src',$4,'main',$4,'merge',0,'d',$5,'queued',true, \
                         '2024-01-01T00:00:00Z'::timestamptz)",
            )
            .bind(operation)
            .bind(repo)
            .bind(pull)
            .bind("a".repeat(40))
            .bind(account)
            .execute(pool)
            .await
            .unwrap();
        }

        // Two live queue entries on main created in the same instant must still
        // get distinct positions (tie-broken by `operation_id`).
        let a = pull(&pool, repo, account, 1).await;
        let b = pull(&pool, repo, account, 2).await;
        intent_at(&pool, a, repo, account, Uuid::new_v4()).await;
        intent_at(&pool, b, repo, account, Uuid::new_v4()).await;

        let mut conn = pool.acquire().await.unwrap();
        let pa = queue_position(&mut conn, a).await.unwrap().unwrap();
        let pb = queue_position(&mut conn, b).await.unwrap().unwrap();
        let mut positions = [pa, pb];
        positions.sort();
        assert_eq!(
            positions,
            [1, 2],
            "same-timestamp entries must tie-break to distinct positions"
        );
        drop(conn);
        pool.close().await;
    }
}
