//! Repository transfer between organizations (M4.1 U07, ADR 0003).
//!
//! A transfer moves a repository to an organization the actor administers at
//! the source (`repository.transfer` on the repository) and may create in at
//! the target (`repository.create` on the target organization). Nothing on disk
//! moves: paths derive from the repository UUID, so the ID, the history and the
//! storage directory are the same before and after. What changes is the owner,
//! the grants that reach the repository, and what the old `owner/name` answers.
//!
//! In one transaction:
//!
//! - **Fork pull requests first.** A fork pull request's head lives in a fork of
//!   the repository, so its base moves out from under it. The operator chooses
//!   `refuse` (any open fork pull request refuses the move, nothing changes) or
//!   `close` (each is closed with a stated reason and audited; reviews stay).
//!   Same-repository pull requests are untouched either way.
//! - **An `executing` merge intent blocks the move**; a `queued`/`validating`
//!   one is cancelled, since its approved head and checks were computed for the
//!   source organization.
//! - **Every grant is revoked** — the repository's own grants and every
//!   organization grant of the source — with one `repository.transferred` audit
//!   record that names the count and the policy, because a grant's subject must
//!   belong to the organization that owns the repository (ADR 0002) and after
//!   the move none of the source subjects do.
//! - **The old path is gone.** The source `owner/name` is recorded in
//!   `repository_redirects` with `transferred_at`, and every lookup (API,
//!   browser, Git HTTP, SSH) refuses such a path (`410` on the API and the page)
//!   before it can fall back to a redirect or to another repository that later
//!   takes the name.
//! - **A failure moves nothing.** The only step outside the transaction is a
//!   read-only storage check taken before the lock: a missing or unreadable Git
//!   directory refuses the move (`503`). Nothing on disk is written, so no
//!   storage step can fail after the metadata committed.
//! - **A name collision at the target is refused** (`409`), nothing changed.
//!   The move keeps the repository's name and never picks another (U07, ADR
//!   0003); an operator who wants a different name renames at the source first
//!   and transfers second. The requested name is locked inside the same
//!   transaction, before the source row is locked, so a concurrent create or
//!   rename cannot take it between the check and the commit.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Resource};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, event_with, facts, idempotency, invalid,
    repositories::{self, RepositoryRow},
    repository_api::{self, Loaded, TargetAccess},
};

/// The operator's choice for the fork pull requests a move leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPolicy {
    Refuse,
    Close,
}

impl ForkPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Close => "close",
        }
    }

    /// A fork pull request closed by a transfer says why in `closed_reason`, the
    /// column 0038 added for system closes.
    pub fn close_reason(self) -> &'static str {
        "repository transferred to another organization"
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferBody {
    organization: String,
    /// `close` or `refuse`; required, so a client never picks one by omission.
    fork_pull_requests: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TransferJson {
    pub(crate) id: Uuid,
    pub(crate) organization_id: Uuid,
    pub(crate) organization: String,
    /// The name at the target: the previous name, since a collision is refused.
    pub(crate) name: String,
    pub(crate) previous_organization_id: Uuid,
    pub(crate) previous_organization: String,
    pub(crate) previous_name: String,
    /// The audited policy, echoed so the caller sees what was recorded.
    pub(crate) fork_pull_requests: String,
    pub(crate) revoked_grants: i64,
    pub(crate) closed_fork_pull_requests: i64,
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
fn same_organization() -> ApiError {
    ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "the target organization is the current one; rename instead",
    )
}
fn executing_merge() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "a merge is executing; retry after it finishes",
    )
}
fn fork_pulls_open() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "open fork pull requests; retry with fork_pull_requests=close",
    )
}
fn name_taken_at_target() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "the target organization already has a repository with this name; rename first, then transfer",
    )
}
fn storage_unavailable() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "repository storage unavailable",
    )
}

const TRANSFERRED: &str = "repository.transferred";

pub(crate) fn router() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/api/v1/organizations/{org}/repositories/{name}/transfer",
        axum::routing::post(transfer),
    )
}

/// The one transaction, shared by the JSON API and the browser page. `org_slug`
/// and `name` address the source; `target` is the target organization's slug.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    state: &AppState,
    actor: Uuid,
    auth_age: u64,
    org_slug: &str,
    name: &str,
    target: &str,
    fork_policy: ForkPolicy,
    body: &[u8],
    key: Option<&str>,
) -> Result<TransferJson, ApiError> {
    let path = format!("/api/v1/organizations/{org_slug}/repositories/{name}/transfer");
    let (_, value) = idempotency::run(
        &state.pool,
        actor,
        "POST",
        &path,
        body,
        key,
        |idem| async move {
            let loaded =
                repository_api::load_current(state, actor, auth_age, org_slug, name).await?;
            // The source-side decision is the repository's own administration.
            repository_api::require(state, &loaded, Action::RepositoryTransfer)?;
            if loaded.repo.status != githearth_core::RepositoryStatus::Active {
                return Err(not_active());
            }

            // Resolve the target and decide the target side before anything is
            // claimed, so a caller who may not create there learns nothing and
            // leaves nothing behind.
            let mut conn = state.pool.acquire().await?;
            let target_org: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
                    .bind(target)
                    .fetch_optional(&mut *conn)
                    .await?;
            let Some(target_org) = target_org else {
                return Err(not_found());
            };
            if target_org == loaded.org_id {
                // A rename inside one organization is not a transfer: it
                // revokes nothing, and it is spelled
                // `PATCH .../repositories/{name}`.
                return Err(same_organization());
            }
            let access =
                repository_api::target_access(&mut conn, state, actor, auth_age, target_org, false)
                    .await?;
            if access != TargetAccess::MayCreate {
                return Err(repository_api::target_refusal(access));
            }
            drop(conn);

            // The storage directory is the repository's history, and the move
            // cannot repair it. Refuse before writing anything. The check is
            // read-only and holds the repository lock so a concurrent delete
            // cannot move the directory underneath it.
            let repo_id = loaded.repo.id;
            let storage = state.storage.clone();
            let guard = storage.repository_lock(repo_id).lock_owned().await;
            let verdict = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                storage.repository_paths_ok(repo_id)
            })
            .await
            .map_err(|_| storage_unavailable())?;
            if verdict.is_err() {
                return Err(storage_unavailable());
            }

            let mut tx = state.pool.begin().await?;
            // Whether the target already holds the name, decided before the
            // claim so a collision is refused without waiting on the claim's
            // upsert. This is the refusal U07 and ADR 0003 require.
            let requested = loaded.repo.name.clone();
            let free = repositories::transfer_name_free(&mut tx, target_org, &requested).await?;
            // Claim the name this move wants at the target now, before the
            // source row is locked (the documented lock order: storage, then
            // the row). Only a name this move checked as free is claimed. The
            // claim takes the (organization, name) advisory lock that creates
            // and renames also take, so no other transaction can take the name
            // between this check and the commit; the row itself is rolled back
            // with a refusal and deleted with the move.
            if free {
                repositories::claim_name(&mut tx, target_org, &requested).await?;
            }
            if !repositories::lock_active(&mut tx, repo_id, true).await? {
                return Err(not_active());
            }
            // Re-authorize the source side against the locked state: a demotion
            // or grant revocation that committed while this request waited must
            // not let it finish with the authority it had before the lock.
            let Some(facts) = facts::load_in_transaction(&mut tx, actor, loaded.org_id).await?
            else {
                return Err(not_found());
            };
            let authz_repo = repositories::authz_repository(&loaded.repo);
            let request = facts.request(
                Action::RepositoryTransfer,
                Resource::Repository(&authz_repo),
                Some(Duration::from_secs(auth_age)),
            );
            let decision = state.authz.authorize(&request);
            if !decision.is_allowed() {
                if decision.denied_for_fresh_auth() && state.authz.allows_if_fresh(&request) {
                    return Err(crate::reauthentication_required());
                }
                return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
            }
            // And the target: a membership or create grant revoked meanwhile is
            // a refusal, not a move into an organization the caller has left.
            let access =
                repository_api::target_access(&mut tx, state, actor, auth_age, target_org, true)
                    .await?;
            if access != TargetAccess::MayCreate {
                return Err(repository_api::target_refusal(access));
            }

            // An in-flight merge must finish before the repository changes
            // hands. Lock every live intent first: the merge worker commits
            // `validating`, then `executing`, on another connection, so a bare
            // existence check could miss the transition and the cancel below
            // (which only covers queued/validating) would let the move commit
            // under a live merge. With the rows locked, a claim that is
            // transitioning either replays as `executing` (refuse) or is still
            // queued/validating (and is cancelled by `TransferRows`).
            sqlx::query(
                "SELECT operation_id FROM merge_intents \
                 WHERE repository_id = $1 AND state IN ('queued','validating','executing') \
                 FOR UPDATE",
            )
            .bind(repo_id)
            .fetch_all(&mut *tx)
            .await?;
            let executing: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM merge_intents WHERE repository_id = $1 AND state = 'executing')",
            )
            .bind(repo_id)
            .fetch_one(&mut *tx)
            .await?;
            if executing {
                return Err(executing_merge());
            }

            // Fork pull requests: closed under the chosen policy, or refused.
            // Same-repository pulls are not touched either way.
            let pulls = repositories::open_pulls(&mut tx, repo_id).await?;
            let fork_pulls: Vec<_> = pulls
                .iter()
                .filter(|p| p.source_repository_id != repo_id)
                .collect();
            let mut closed = 0_i64;
            if !fork_pulls.is_empty() {
                match fork_policy {
                    ForkPolicy::Refuse => return Err(fork_pulls_open()),
                    ForkPolicy::Close => {
                        for pull in &fork_pulls {
                            repositories::close_pull(
                                &mut tx,
                                pull,
                                actor,
                                fork_policy.close_reason(),
                                fork_policy.as_str(),
                            )
                            .await?;
                            closed += 1;
                        }
                    }
                }
            }

            // A name collision at the target refuses the move and picks
            // nothing: nothing is renamed, and the named repository, its
            // grants, its pulls and its forks are all untouched. Renaming first
            // is the operator's step, never the transfer's.
            if !free {
                return Err(name_taken_at_target());
            }
            let new_name = requested.clone();

            let applied = repositories::TransferRows {
                old_name: requested.clone(),
                new_name: new_name.clone(),
                target_org,
                source_org: loaded.org_id,
                repository_id: repo_id,
                actor,
            }
            .apply(&mut tx)
            .await;
            let revoked = match applied {
                Ok(revoked) => revoked,
                Err(e) => {
                    // A create or another transfer took the name between the
                    // free check and this update. The unique index refuses the
                    // move; it is the same collision a pre-checked name gives,
                    // not a 500.
                    if e.as_database_error()
                        .is_some_and(|d| d.code().as_deref() == Some("23505"))
                    {
                        return Err(name_taken_at_target());
                    }
                    return Err(e.into());
                }
            };

            // One audit record for the move and for the grant revocation it
            // carries: the policy, the grants it revoked, the pulls it closed.
            // It is filed under the organization that still owns the repository,
            // so the transfer stays in the source's audit chain (which keeps
            // verifying) and the target's chain starts at its own first event.
            event_with(
                &mut tx,
                Some(loaded.org_id),
                actor,
                TRANSFERRED,
                serde_json::json!({
                    "repository_id": repo_id,
                    "from_organization_id": loaded.org_id,
                    "from_name": requested,
                    "to_organization_id": target_org,
                    "to_name": new_name,
                    "fork_pull_requests": fork_policy.as_str(),
                    "fork_pull_requests_closed": closed,
                    "revoked_grants": revoked,
                }),
            )
            .await?;

            let result = TransferJson {
                id: repo_id,
                organization_id: target_org,
                organization: target.to_owned(),
                name: new_name,
                previous_organization_id: loaded.org_id,
                previous_organization: org_slug.to_owned(),
                previous_name: requested,
                fork_pull_requests: fork_policy.as_str().to_owned(),
                revoked_grants: revoked,
                closed_fork_pull_requests: closed,
            };
            let status = StatusCode::OK;
            let body = serde_json::to_value(&result).unwrap();
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

/// `POST /api/v1/organizations/{org}/repositories/{name}/transfer`
pub(crate) async fn transfer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, name)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<TransferJson>, ApiError> {
    let input: TransferBody = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !crate::valid_path_param(&org_slug)
        || !crate::valid_path_param(&name)
        || !crate::valid_path_param(&input.organization)
        || input.organization.is_empty()
    {
        return Err(invalid());
    }
    let policy = match input.fork_pull_requests.as_str() {
        "refuse" => ForkPolicy::Refuse,
        "close" => ForkPolicy::Close,
        _ => return Err(invalid()),
    };
    let (actor, auth_age) = crate::authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let result = run(
        &state,
        actor.id,
        auth_age,
        &org_slug,
        &name,
        &input.organization,
        policy,
        &body,
        key,
    )
    .await?;
    Ok(Json(result))
}

/// Which organizations the browser page offers, and what the choice costs.
pub(crate) struct PageModel {
    pub(crate) repo: RepositoryRow,
    /// Targets the caller may create in, by slug and display name.
    pub(crate) targets: Vec<(String, String)>,
    /// Open fork pull requests, so the page can explain the policy choice.
    pub(crate) open_fork_pulls: i64,
}

pub(crate) async fn page_model(state: &AppState, loaded: &Loaded) -> Result<PageModel, ApiError> {
    let mut conn = state.pool.acquire().await?;
    // The caller's own organizations are the only reachable targets today: a
    // `repository.create` grant on an organization one is not a member of is
    // not loadable facts. Each is asked, not assumed: the same decision the move
    // makes, so the page never offers a target the move would refuse.
    let candidates: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT o.id, o.slug, o.name FROM organizations o JOIN memberships m ON m.organization_id = o.id \
         WHERE m.account_id = $1 AND o.suspended = false AND o.id <> $2 ORDER BY o.slug",
    )
    .bind(loaded.actor_id)
    .bind(loaded.org_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut targets = Vec::new();
    for (org_id, slug, name) in candidates {
        if repository_api::target_access(
            &mut conn,
            state,
            loaded.actor_id,
            loaded.auth_age,
            org_id,
            false,
        )
        .await?
            == TargetAccess::MayCreate
        {
            targets.push((slug, name));
        }
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    let open_fork_pulls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pull_requests WHERE repository_id = $1 AND state = 'open' \
         AND source_repository_id <> $1",
    )
    .bind(loaded.repo.id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(PageModel {
        repo: loaded.repo.clone(),
        targets,
        open_fork_pulls,
    })
}
