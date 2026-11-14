use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, Protection};
use githearth_core::RepositoryStatus;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, event, idempotency, invalid, repositories, repository_api,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/protection",
            axum::routing::post(create).get(list),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/protection/{id}",
            axum::routing::patch(update).delete(delete),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "protection rule not found")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
fn duplicate() -> ApiError {
    ApiError(StatusCode::CONFLICT, "protection rule already exists")
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProtectionRule {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub pattern: String,
    pub allow_force_push: bool,
    pub allow_deletion: bool,
    pub require_pull_request: bool,
    pub required_approvals: i32,
    pub required_status_checks: Vec<String>,
    pub dismiss_stale_approvals: bool,
    pub require_signed_commits: bool,
    pub require_merge_queue: bool,
    pub require_code_owner_review: bool,
    pub created_at: String,
    pub updated_at: String,
}

const RULE_COLUMNS: &str = "id, repository_id, pattern, allow_force_push, allow_deletion, \
 require_pull_request, required_approvals, required_status_checks, \
 dismiss_stale_approvals, require_signed_commits, require_merge_queue, require_code_owner_review, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

/// A pattern is a concrete branch name, or a glob with one trailing `*`.
/// The `*` is a prefix wildcard, so `release/*` also protects `release/a/b`.
/// Returns the literal (`main`) and glob prefix (`release/`) parts, or `None`
/// when the pattern is not valid.
pub(crate) fn parse_pattern(pattern: &str) -> Option<(Option<&str>, Option<&str>)> {
    if pattern.is_empty() {
        return None;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        if prefix.is_empty()
            || prefix.contains('*')
            || !prefix.ends_with('/')
            || !valid_segments(prefix.strip_suffix('/').unwrap())
        {
            return None;
        }
        Some((None, Some(prefix)))
    } else if valid_segments(pattern) {
        Some((Some(pattern), None))
    } else {
        None
    }
}

pub(crate) fn valid_segments(segments: &str) -> bool {
    !segments.is_empty()
        && segments.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

/// Map a full ref name to the most specific matching rule's protection:
/// an exact branch name beats any glob, and among globs the longest prefix
/// (`refs/heads/release/v1` picks `release/v1/*` over `release/*`). A
/// trailing `*` is a prefix match, so `release/*` also protects `release/a/b`.
pub(crate) fn matching(rules: &[ProtectionRule], ref_name: &str) -> Option<Protection> {
    matching_rule(rules, ref_name).map(protection_of)
}

/// The most specific rule matching `ref_name`, using the same ranking as
/// [`matching`]. The rule (not just its protection payload) is exposed so
/// callers can read fields beyond the Cedar `Protection` payload, such as
/// `required_status_checks` or `require_merge_queue`.
pub(crate) fn matching_rule<'a>(
    rules: &'a [ProtectionRule],
    ref_name: &str,
) -> Option<&'a ProtectionRule> {
    let branch = ref_name.strip_prefix("refs/heads/")?;
    let mut best_glob: Option<(&str, &ProtectionRule)> = None;
    for rule in rules {
        // Skip only a malformed rule; `?` here would drop protection for every branch.
        let Some((exact, glob)) = parse_pattern(&rule.pattern) else {
            continue;
        };
        if let Some(name) = exact
            && name == branch
        {
            return Some(rule);
        }
        if let Some(prefix) = glob
            && branch.starts_with(prefix)
            && best_glob.is_none_or(|(existing, _)| prefix.len() > existing.len())
        {
            best_glob = Some((prefix, rule));
        }
    }
    best_glob.map(|(_, rule)| rule)
}

/// The required status check contexts of the rule matching `ref_name`, or the
/// empty list when no rule matches.
pub(crate) fn required_status_checks(rules: &[ProtectionRule], ref_name: &str) -> Vec<String> {
    matching_rule(rules, ref_name)
        .map(|rule| rule.required_status_checks.clone())
        .unwrap_or_default()
}

/// Whether the branch named by `ref_name` is behind a merge queue: the
/// matching rule, if any, demands `require_merge_queue`.
pub(crate) fn requires_queue(rules: &[ProtectionRule], ref_name: &str) -> bool {
    matching_rule(rules, ref_name).is_some_and(|rule| rule.require_merge_queue)
}

fn protection_of(rule: &ProtectionRule) -> Protection {
    Protection {
        allow_force_push: rule.allow_force_push,
        allow_deletion: rule.allow_deletion,
        require_pull_request: rule.require_pull_request,
        required_approvals: rule.required_approvals as u32,
        require_checks: !rule.required_status_checks.is_empty(),
        dismiss_stale_approvals: rule.dismiss_stale_approvals,
        require_signed_commits: rule.require_signed_commits,
        require_code_owner_review: rule.require_code_owner_review,
    }
}

/// Load every rule for a repository, ordered for deterministic listing.
pub(crate) async fn load_rules(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Vec<ProtectionRule>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM branch_protection_rules \
         WHERE repository_id = $1 ORDER BY pattern"
    ))
    .bind(repository_id)
    .fetch_all(&mut *conn)
    .await
}

/// Loads the rules and the generation as one snapshot, so a caller binding a
/// ruleset to a generation (such as the merge path) can never pair a stale
/// ruleset with a fresh generation. Both reads happen inside one
/// `REPEATABLE READ` read-only transaction.
pub(crate) async fn load_rules_with_generation(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<(Vec<ProtectionRule>, i64), sqlx::Error> {
    snapshot(conn, repository_id, async {}).await
}

/// `load_rules_with_generation`, awaiting `between` after the rules are read;
/// tests use it to commit a rule change between the two reads.
async fn snapshot(
    conn: &mut PgConnection,
    repository_id: Uuid,
    between: impl std::future::Future<Output = ()>,
) -> Result<(Vec<ProtectionRule>, i64), sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let rules = load_rules(&mut tx, repository_id).await?;
    between.await;
    let generation = current_generation(&mut tx, repository_id).await?;
    tx.commit().await?;
    Ok((rules, generation))
}

pub(crate) async fn current_generation(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT generation FROM protection_generations WHERE repository_id = $1")
        .bind(repository_id)
        .fetch_optional(&mut *conn)
        .await
        .map(|g| g.unwrap_or(0))
}

/// Bump the repository's protection generation to `generation + 1`, creating
/// the counter row on first use. Called inside the same transaction as the rule
/// mutation so a cached ruleset can detect staleness. Shared with tag
/// protection, whose rule changes must invalidate cached rulesets the same way.
pub(crate) async fn bump_generation(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO protection_generations (repository_id, generation) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET generation = protection_generations.generation + 1 \
         RETURNING generation",
    )
    .bind(repository_id)
    .fetch_one(&mut *conn)
    .await
}

#[derive(Serialize)]
struct ProtectionList {
    generation: i64,
    items: Vec<ProtectionRule>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
) -> Result<Json<ProtectionList>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let (items, generation, _) =
        list_rules(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    Ok(Json(ProtectionList { generation, items }))
}

/// Loads a repository's branch rules plus the current protection generation
/// for a read-capable actor. The third element reports whether the actor may
/// manage the rules (`RepositoryUpdate`, the same `maintain` gate as tag
/// protection), so the settings page can hide controls the viewer could not
/// submit. Shared by the JSON listing and the settings page.
pub(crate) async fn list_rules(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
) -> Result<(Vec<ProtectionRule>, i64, bool), ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    repository_api::require(state, &loaded, Action::RepositoryRead)?;
    if loaded.repo.status != RepositoryStatus::Active {
        return Err(not_found());
    }
    let can_manage = repository_api::require(state, &loaded, Action::RepositoryUpdate).is_ok();
    let mut conn = state.pool.acquire().await?;
    let (items, generation) = load_rules_with_generation(&mut conn, loaded.repo.id).await?;
    Ok((items, generation, can_manage))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRule {
    pattern: String,
    #[serde(default)]
    allow_force_push: bool,
    #[serde(default)]
    allow_deletion: bool,
    #[serde(default)]
    require_pull_request: bool,
    #[serde(default)]
    required_approvals: i32,
    #[serde(default)]
    required_status_checks: Vec<String>,
    /// Defaults ON for new rules: stale approvals are dismissed unless the
    /// rule explicitly opts out, matching the pre-flag semantics.
    #[serde(default = "default_true")]
    dismiss_stale_approvals: bool,
    /// Refuses pushes to the protected branch unless every new commit carries
    /// an SSH signature that verifies against a key registered to the
    /// committer's account. This gates client pushes only: server-created
    /// merges bypass it (ADR 0007 "Merge exception"), so a merge commit and the
    /// commits it brings in are not re-verified and a protected branch can
    /// still gain server-created merge history.
    #[serde(default)]
    require_signed_commits: bool,
    #[serde(default)]
    require_merge_queue: bool,
    #[serde(default)]
    require_code_owner_review: bool,
}

fn default_true() -> bool {
    true
}

fn validate_required_status_checks(checks: &[String]) -> Result<(), ApiError> {
    if checks.len() > 100 {
        return Err(invalid());
    }
    let mut seen = std::collections::HashSet::new();
    for context in checks {
        if context.as_str() == crate::statuses::LEGACY_REQUIRE_CHECKS_CONTEXT {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "required status checks cannot include the reserved context",
            ));
        }
        if crate::statuses::has_surrounding_whitespace(context) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "required status check context must not have leading or trailing whitespace",
            ));
        }
        if !seen.insert(context) {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "required status checks must not contain duplicates",
            ));
        }
        if !crate::statuses::valid_context(context) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_rule(input: &NewRule) -> Result<(), ApiError> {
    if parse_pattern(&input.pattern).is_none() {
        return Err(invalid());
    }
    if !(0..=10).contains(&input.required_approvals) {
        return Err(invalid());
    }
    validate_required_status_checks(&input.required_status_checks)
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<ProtectionRule>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, value) = create_rule(
        &state, actor.id, auth_age, &org_slug, &repo_name, &body, key,
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

/// Creates a branch protection rule as `actor_id`; shared by the JSON handler
/// and the browser settings page. Validation, authorization and audit happen
/// once here.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_rule(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let path_str = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/protection");
    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let input: NewRule = serde_json::from_slice(body).map_err(|_| invalid())?;
            validate_rule(&input)?;
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            repository_api::require(state, &loaded, Action::RepositoryUpdate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let id = Uuid::new_v4();
            let inserted = sqlx::query(
                "INSERT INTO branch_protection_rules \
                 (id, repository_id, pattern, allow_force_push, allow_deletion, \
                  require_pull_request, required_approvals, required_status_checks, \
                  dismiss_stale_approvals, require_signed_commits, require_merge_queue, \
                  require_code_owner_review, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .bind(&input.pattern)
            .bind(input.allow_force_push)
            .bind(input.allow_deletion)
            .bind(input.require_pull_request)
            .bind(input.required_approvals)
            .bind(&input.required_status_checks)
            .bind(input.dismiss_stale_approvals)
            .bind(input.require_signed_commits)
            .bind(input.require_merge_queue)
            .bind(input.require_code_owner_review)
            .bind(actor_id)
            .execute(&mut *tx)
            .await;
            if let Err(e) = inserted {
                if e.as_database_error()
                    .is_some_and(|d| d.code().as_deref() == Some("23505"))
                {
                    return Err(duplicate());
                }
                return Err(e.into());
            }
            bump_generation(&mut tx, loaded.repo.id).await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "branch_protection.created",
            )
            .await?;

            let rule: ProtectionRule = sqlx::query_as(&format!(
                "SELECT {RULE_COLUMNS} FROM branch_protection_rules WHERE id = $1"
            ))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&rule).unwrap();
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PatchRule {
    /// The documented field is `pattern`; `path` is accepted as a legacy alias.
    #[serde(default, alias = "path")]
    pattern: Option<String>,
    #[serde(default)]
    allow_force_push: Option<bool>,
    #[serde(default)]
    allow_deletion: Option<bool>,
    #[serde(default)]
    require_pull_request: Option<bool>,
    #[serde(default)]
    required_approvals: Option<i32>,
    #[serde(default)]
    required_status_checks: Option<Vec<String>>,
    #[serde(default)]
    dismiss_stale_approvals: Option<bool>,
    #[serde(default)]
    require_signed_commits: Option<bool>,
    #[serde(default)]
    require_merge_queue: Option<bool>,
    #[serde(default)]
    require_code_owner_review: Option<bool>,
}

async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, id)): Path<(String, String, Uuid)>,
    body: Bytes,
) -> Result<Json<ProtectionRule>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/protection/{id}");
    let state = &state;
    let body = &body;
    let (_, value) = idempotency::run(
        &state.pool,
        actor.id,
        "PATCH",
        &path_str,
        body,
        key,
        |idem| async move {
            let input: PatchRule = serde_json::from_slice(body).map_err(|_| invalid())?;
            if input.pattern.is_none()
                && input.allow_force_push.is_none()
                && input.allow_deletion.is_none()
                && input.require_pull_request.is_none()
                && input.required_approvals.is_none()
                && input.required_status_checks.is_none()
                && input.dismiss_stale_approvals.is_none()
                && input.require_signed_commits.is_none()
                && input.require_merge_queue.is_none()
                && input.require_code_owner_review.is_none()
            {
                return Err(invalid());
            }
            if let Some(ref p) = input.pattern
                && parse_pattern(p).is_none()
            {
                return Err(invalid());
            }
            if let Some(apps) = input.required_approvals
                && !(0..=10).contains(&apps)
            {
                return Err(invalid());
            }
            if let Some(ref checks) = input.required_status_checks {
                validate_required_status_checks(checks)?;
            }

            let loaded =
                repository_api::load_current(state, actor.id, auth_age, &org_slug, &repo_name)
                    .await?;
            repository_api::require(state, &loaded, Action::RepositoryUpdate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let locked: Option<ProtectionRule> = sqlx::query_as(&format!(
                "SELECT {RULE_COLUMNS} FROM branch_protection_rules \
                 WHERE id = $1 AND repository_id = $2 FOR UPDATE"
            ))
            .bind(id)
            .bind(loaded.repo.id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(locked) = locked else {
                return Err(not_found());
            };

            let mut updated_pattern = locked.pattern.clone();
            if let Some(ref p) = input.pattern {
                updated_pattern = p.clone();
            }
            let res = sqlx::query(
                "UPDATE branch_protection_rules SET pattern = $1, \
                 allow_force_push = $2, allow_deletion = $3, require_pull_request = $4, \
                 required_approvals = $5, required_status_checks = $6, \
                 dismiss_stale_approvals = $7, require_signed_commits = $8, \
                 require_merge_queue = $9, require_code_owner_review = $10, \
                 updated_at = now() WHERE id = $11",
            )
            .bind(updated_pattern)
            .bind(input.allow_force_push.unwrap_or(locked.allow_force_push))
            .bind(input.allow_deletion.unwrap_or(locked.allow_deletion))
            .bind(
                input
                    .require_pull_request
                    .unwrap_or(locked.require_pull_request),
            )
            .bind(
                input
                    .required_approvals
                    .unwrap_or(locked.required_approvals),
            )
            .bind(
                input
                    .required_status_checks
                    .unwrap_or(locked.required_status_checks),
            )
            .bind(
                input
                    .dismiss_stale_approvals
                    .unwrap_or(locked.dismiss_stale_approvals),
            )
            .bind(
                input
                    .require_signed_commits
                    .unwrap_or(locked.require_signed_commits),
            )
            .bind(
                input
                    .require_merge_queue
                    .unwrap_or(locked.require_merge_queue),
            )
            .bind(
                input
                    .require_code_owner_review
                    .unwrap_or(locked.require_code_owner_review),
            )
            .bind(id)
            .execute(&mut *tx)
            .await;
            if let Err(e) = res {
                if e.as_database_error()
                    .is_some_and(|d| d.code().as_deref() == Some("23505"))
                {
                    return Err(duplicate());
                }
                return Err(e.into());
            }
            bump_generation(&mut tx, loaded.repo.id).await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor.id,
                "branch_protection.updated",
            )
            .await?;

            let rule: ProtectionRule = sqlx::query_as(&format!(
                "SELECT {RULE_COLUMNS} FROM branch_protection_rules WHERE id = $1"
            ))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::OK;
            let body = serde_json::to_value(&rule).unwrap();
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

async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name, id)): Path<(String, String, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    delete_rule(&state, actor.id, auth_age, &org_slug, &repo_name, id, key).await
}

/// Deletes a branch protection rule as `actor_id`; shared by the JSON handler
/// and the browser settings page.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn delete_rule(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    id: Uuid,
    key: Option<&str>,
) -> Result<StatusCode, ApiError> {
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/protection/{id}");
    let (status, _) = idempotency::run(
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
            repository_api::require(state, &loaded, Action::RepositoryUpdate)?;
            if loaded.repo.status != RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let deleted = sqlx::query(
                "DELETE FROM branch_protection_rules WHERE id = $1 AND repository_id = $2",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .execute(&mut *tx)
            .await?;
            if deleted.rows_affected() == 0 {
                return Err(not_found());
            }
            bump_generation(&mut tx, loaded.repo.id).await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "branch_protection.deleted",
            )
            .await?;
            let status = StatusCode::NO_CONTENT;
            let body = serde_json::Value::Null;
            if !idem.store(&mut tx, status, &body).await? {
                return Err(idempotency::claim_lost());
            }
            tx.commit().await?;
            Ok((status, body))
        },
    )
    .await?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, overrides: &dyn Fn(&mut ProtectionRule)) -> ProtectionRule {
        let mut r = ProtectionRule {
            id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            pattern: pattern.to_owned(),
            allow_force_push: false,
            allow_deletion: false,
            require_pull_request: false,
            required_approvals: 0,
            required_status_checks: Vec::new(),
            dismiss_stale_approvals: true,
            require_signed_commits: false,
            require_merge_queue: false,
            require_code_owner_review: false,
            created_at: String::new(),
            updated_at: String::new(),
        };
        overrides(&mut r);
        r
    }

    #[test]
    fn pattern_parsing_accepts_and_rejects() {
        for (good, exact, glob) in [
            ("main", Some("main"), None),
            ("release/*", None, Some("release/")),
            ("release/v1/*", None, Some("release/v1/")),
            ("a.b-c_d", Some("a.b-c_d"), None),
            ("team/branch/name", Some("team/branch/name"), None),
        ] {
            assert_eq!(parse_pattern(good), Some((exact, glob)), "{good}");
        }
        for bad in [
            "",
            "/main",
            "main/",
            "release*",
            "*",
            "release/**",
            "a//b",
            "a/../b",
            "a/./b",
            "release/*/v1",
            "rel ease",
            "release/ *",
        ] {
            assert_eq!(parse_pattern(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_malformed_rule_does_not_disable_the_others() {
        let rules = [rule("bad/../x", &|_| {}), rule("main", &|_| {})];
        assert!(matching(&rules, "refs/heads/main").is_some());
    }

    #[test]
    fn required_status_checks_reject_reserved_duplicates_and_whitespace() {
        let reserved = crate::statuses::LEGACY_REQUIRE_CHECKS_CONTEXT.to_owned();
        for checks in [
            vec![reserved.clone()],
            vec!["ci".to_owned(), "ci".to_owned()],
            vec!["ci".to_owned(), "lint".to_owned(), "ci".to_owned()],
            vec![" ci".to_owned()],
            vec!["ci ".to_owned()],
            vec![" ci ".to_owned()],
        ] {
            let err = validate_required_status_checks(&checks).unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{checks:?}"
            );
        }

        // Structural failures stay 400: empty, all-whitespace, and oversized.
        let empty = validate_required_status_checks(&[String::new()]).unwrap_err();
        assert_eq!(empty.status_code(), StatusCode::BAD_REQUEST);
        let blank = validate_required_status_checks(&["   ".to_owned()]).unwrap_err();
        assert_eq!(blank.status_code(), StatusCode::BAD_REQUEST);
        let oversized = validate_required_status_checks(&["x".repeat(256)]).unwrap_err();
        assert_eq!(oversized.status_code(), StatusCode::BAD_REQUEST);
        let too_many = validate_required_status_checks(&vec!["ci".to_owned(); 101]).unwrap_err();
        assert_eq!(too_many.status_code(), StatusCode::BAD_REQUEST);

        assert!(validate_required_status_checks(&["ci".to_owned(), "lint".to_owned()]).is_ok());

        // A multi-byte context is bounded by character count, not bytes, so a
        // 100-CJK-character context (300 bytes) is a valid required check.
        assert!(validate_required_status_checks(&["界".repeat(100)]).is_ok());
        let overlong = validate_required_status_checks(&["界".repeat(256)]).unwrap_err();
        assert_eq!(overlong.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn requires_queue_reads_the_matching_rule() {
        let main = rule("main", &|r| r.require_merge_queue = true);
        let release = rule("release/*", &|r| r.require_merge_queue = false);
        let rules = [main, release];

        assert!(requires_queue(&rules, "refs/heads/main"));
        assert!(!requires_queue(&rules, "refs/heads/release/v1"));
        assert!(!requires_queue(&rules, "refs/heads/other"));
        assert!(!requires_queue(&[], "refs/heads/main"));
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn load_rules_with_generation_returns_a_consistent_pair() {
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
            .bind(format!("pg-{}", Uuid::new_v4().simple()))
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

        let mut conn = pool.acquire().await.unwrap();
        let (rules, generation) = load_rules_with_generation(&mut conn, repo).await.unwrap();
        assert_eq!(generation, 0);
        assert!(rules.is_empty());

        // Create a rule and bump the generation exactly as create/update/delete do.
        sqlx::query(
            "INSERT INTO branch_protection_rules \
             (id, repository_id, pattern, allow_force_push, allow_deletion, require_pull_request, required_approvals, required_status_checks, require_code_owner_review, created_by) \
             VALUES ($1, $2, 'main', false, false, true, 2, '{}', false, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(repo)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
        bump_generation(&mut conn, repo).await.unwrap();

        let (rules, generation) = load_rules_with_generation(&mut conn, repo).await.unwrap();
        assert_eq!(generation, 1);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern, "main");
        assert!(rules[0].require_pull_request);

        // Another bump changes the generation, not the rules.
        bump_generation(&mut conn, repo).await.unwrap();
        let (rules, generation) = load_rules_with_generation(&mut conn, repo).await.unwrap();
        assert_eq!(generation, 2);
        assert_eq!(rules.len(), 1);

        // A rule change committed from another connection between the two
        // reads must not pair the old rules with the new generation.
        let change = async {
            let mut other = pool.begin().await.unwrap();
            sqlx::query("DELETE FROM branch_protection_rules WHERE repository_id = $1")
                .bind(repo)
                .execute(&mut *other)
                .await
                .unwrap();
            bump_generation(&mut other, repo).await.unwrap();
            other.commit().await.unwrap();
        };
        let (rules, generation) = snapshot(&mut conn, repo, change).await.unwrap();
        assert_eq!((rules.len(), generation), (1, 2));
        let (rules, generation) = load_rules_with_generation(&mut conn, repo).await.unwrap();
        assert_eq!((rules.len(), generation), (0, 3));

        // close() waits for every checked-out connection, so return ours first.
        drop(conn);
        pool.close().await;
    }

    #[test]
    fn matching_exact_beats_glob_and_longest_prefix_wins() {
        let main = rule("main", &|r| r.required_approvals = 1);
        let release = rule("release/*", &|r| r.required_approvals = 2);
        let release_v1 = rule("release/v1/*", &|r| r.required_approvals = 3);
        let other = rule("other", &|_| {});
        let rules = [main, release, release_v1, other];

        // Exact beats glob.
        let m = matching(&rules, "refs/heads/main").unwrap();
        assert_eq!(m.required_approvals, 1);

        // Longest glob prefix wins: a branch under release/v1/ matches
        // release/v1/*, not the shorter release/* glob.
        let v1 = matching(&rules, "refs/heads/release/v1/x").unwrap();
        assert_eq!(v1.required_approvals, 3);
        let generic = matching(&rules, "refs/heads/release/v2").unwrap();
        assert_eq!(generic.required_approvals, 2);

        // No match.
        assert!(matching(&rules, "refs/heads/unknown").is_none());
        assert!(matching(&rules, "refs/tags/v1").is_none());
        assert!(matching(&rules, "main").is_none());

        // Protection payload propagates.
        let force = rule("force/*", &|r| {
            r.allow_force_push = true;
            r.require_pull_request = true;
            r.required_approvals = 7;
            r.required_status_checks = vec!["ci".to_owned()];
            r.require_code_owner_review = true;
        });
        let rules2 = [force];
        let p = matching(&rules2, "refs/heads/force/x").unwrap();
        assert!(p.allow_force_push);
        assert!(p.require_pull_request);
        assert_eq!(p.required_approvals, 7);
        assert!(p.require_checks);
        assert!(p.require_code_owner_review);
        assert_eq!(
            required_status_checks(&rules2, "refs/heads/force/x"),
            vec!["ci".to_owned()]
        );
        assert!(required_status_checks(&rules2, "refs/heads/other").is_empty());
    }
}
