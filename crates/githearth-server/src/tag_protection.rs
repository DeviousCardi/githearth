//! Tag protection (M3): per-repository rules that restrict who may create,
//! update (move) and delete matching tags.
//!
//! Unlike branch rules, a tag rule carries no pull-request or approval fields:
//! it names the minimum role allowed to create a matching tag and two switches
//! that (by default) forbid moving or deleting one. Enforcement lives in the
//! shared ref admission path (`git_admission::decide_ref`), so Smart HTTP and
//! SSH pushes agree, and server-side merge/suggestion ref mutation can never
//! touch tags (it only ever writes `refs/heads/`).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use githearth_authz::{Action, BuiltinRole, Role, Scope};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::{
    ApiError, AppState, authenticate, event, facts::Facts, idempotency, invalid, repositories,
    repository_api,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/tag-protection",
            axum::routing::post(create).get(list),
        )
        .route(
            "/api/v1/organizations/{org}/repositories/{repo}/tag-protection/{id}",
            axum::routing::delete(delete),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "tag protection rule not found")
}
fn not_active() -> ApiError {
    ApiError(StatusCode::CONFLICT, "repository is not active")
}
fn duplicate() -> ApiError {
    ApiError(StatusCode::CONFLICT, "tag protection rule already exists")
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TagProtectionRule {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub pattern: String,
    pub allowed_to_create: String,
    pub allow_update: bool,
    pub allow_delete: bool,
    pub created_at: String,
    pub updated_at: String,
}

const RULE_COLUMNS: &str = "id, repository_id, pattern, allowed_to_create, allow_update, \
 allow_delete, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at";

/// The roles a tag rule may name as its `allowed_to_create` threshold, ordered
/// from least to most privileged. `owner` is deliberately absent: it is an
/// organization-level role and satisfies `admin` through the rank comparison.
pub(crate) const CREATE_ROLES: &[&str] =
    &["member", "read", "triage", "write", "maintain", "admin"];

fn valid_create_role(role: &str) -> bool {
    CREATE_ROLES.contains(&role)
}

/// A pattern is a concrete tag name, or a prefix wildcard with one trailing
/// `*`. Unlike branch rules the `*` needs no preceding `/`: `v*` protects every
/// `v…` tag. Returns the literal (`v1.0.0`) and glob prefix (`v`, `release/`)
/// parts, or `None` when the pattern is not valid.
pub(crate) fn parse_pattern(pattern: &str) -> Option<(Option<&str>, Option<&str>)> {
    if pattern.is_empty() {
        return None;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        if prefix.is_empty() || prefix.contains('*') {
            return None;
        }
        // `release/*` ends with `/`; `v*` does not. Both are prefixes over
        // valid segments.
        let segments = prefix.strip_suffix('/').unwrap_or(prefix);
        if !crate::protection::valid_segments(segments) {
            return None;
        }
        Some((None, Some(prefix)))
    } else if crate::protection::valid_segments(pattern) {
        Some((Some(pattern), None))
    } else {
        None
    }
}

/// The most specific rule matching `ref_name`: an exact tag name beats any
/// glob, and among globs the longest prefix wins (`refs/tags/v1.2` picks
/// `v1.*` over `v*`). A trailing `*` is a prefix match, so `release/*` also
/// protects `release/a/b`.
pub(crate) fn matching_rule<'a>(
    rules: &'a [TagProtectionRule],
    ref_name: &str,
) -> Option<&'a TagProtectionRule> {
    let tag = ref_name.strip_prefix("refs/tags/")?;
    let mut best_glob: Option<(&str, &TagProtectionRule)> = None;
    for rule in rules {
        let Some((exact, glob)) = parse_pattern(&rule.pattern) else {
            continue;
        };
        if let Some(name) = exact
            && name == tag
        {
            return Some(rule);
        }
        if let Some(prefix) = glob
            && tag.starts_with(prefix)
            && best_glob.is_none_or(|(existing, _)| prefix.len() > existing.len())
        {
            best_glob = Some((prefix, rule));
        }
    }
    best_glob.map(|(_, rule)| rule)
}

/// Load every tag rule for a repository, ordered for deterministic listing.
pub(crate) async fn load_rules(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<Vec<TagProtectionRule>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM tag_protection_rules \
         WHERE repository_id = $1 ORDER BY pattern"
    ))
    .bind(repository_id)
    .fetch_all(&mut *conn)
    .await
}

/// Loads the rules and the protection generation as one snapshot, mirroring
/// `protection::load_rules_with_generation` so a listing can pair rules with
/// the generation that produced them.
pub(crate) async fn load_rules_with_generation(
    conn: &mut PgConnection,
    repository_id: Uuid,
) -> Result<(Vec<TagProtectionRule>, i64), sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let rules = load_rules(&mut tx, repository_id).await?;
    let generation = crate::protection::current_generation(&mut tx, repository_id).await?;
    tx.commit().await?;
    Ok((rules, generation))
}

fn role_rank(role: &str) -> u8 {
    match role {
        "member" => 0,
        "read" => 1,
        "triage" => 2,
        "write" => 3,
        "maintain" => 4,
        "admin" => 5,
        "owner" => 6,
        _ => u8::MAX,
    }
}

fn grant_rank(role: &Role) -> u8 {
    match role {
        Role::Builtin(BuiltinRole::Member) => 0,
        Role::Builtin(BuiltinRole::Read) => 1,
        Role::Builtin(BuiltinRole::Triage) => 2,
        Role::Builtin(BuiltinRole::Write) => 3,
        Role::Builtin(BuiltinRole::Maintain) => 4,
        Role::Builtin(BuiltinRole::Admin) => 5,
        Role::Builtin(BuiltinRole::Owner) => 6,
        // A custom role has no rank in the built-in hierarchy and never
        // satisfies a built-in role threshold.
        Role::Custom(_) => u8::MAX,
    }
}

/// Whether the actor holds a grant of rank `>= required_role` on the repository
/// itself or its owning organization. Team grants already resolve through
/// [`Facts::grants`]; grants scoped to other repositories are ignored.
pub(crate) fn may_operate(
    facts: &Facts,
    repository_id: Uuid,
    organization_id: Uuid,
    required_role: &str,
) -> bool {
    let required = role_rank(required_role);
    facts.grants.iter().any(|grant| {
        let in_scope = match grant.scope {
            Scope::Organization(id) => id == organization_id,
            Scope::Repository(id) => id == repository_id,
        };
        in_scope && grant_rank(&grant.role) >= required
    })
}

/// The tag-specific admission decision for a matching rule. A protected tag may
/// be created only by the configured role (or higher); moving and deleting are
/// refused unless the rule explicitly allows them to that role.
pub(crate) fn decide(
    facts: &Facts,
    repository_id: Uuid,
    organization_id: Uuid,
    rule: &TagProtectionRule,
    is_create: bool,
    is_delete: bool,
) -> Result<(), &'static str> {
    let permitted = || {
        may_operate(
            facts,
            repository_id,
            organization_id,
            &rule.allowed_to_create,
        )
    };
    if is_delete {
        if rule.allow_delete && permitted() {
            return Ok(());
        }
        return Err("protected tag: ref.delete");
    }
    if is_create {
        if permitted() {
            return Ok(());
        }
        return Err("protected tag: ref.create");
    }
    // A move/update of an existing tag.
    if rule.allow_update && permitted() {
        return Ok(());
    }
    Err("protected tag: ref.update")
}

#[derive(Serialize)]
struct TagProtectionList {
    generation: i64,
    items: Vec<TagProtectionRule>,
}

/// Loads a repository's tag rules plus the current protection generation for
/// a read-capable actor. Shared by the JSON listing and the settings page.
pub(crate) async fn list_rules(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
) -> Result<(Vec<TagProtectionRule>, i64), ApiError> {
    let loaded = repository_api::load_repo(state, actor_id, auth_age, org_slug, repo_name).await?;
    repository_api::require(state, &loaded, Action::RepositoryRead)?;
    if loaded.repo.status != githearth_core::RepositoryStatus::Active {
        return Err(not_found());
    }
    let mut conn = state.pool.acquire().await?;
    load_rules_with_generation(&mut conn, loaded.repo.id)
        .await
        .map_err(ApiError::from)
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
) -> Result<Json<TagProtectionList>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let (items, generation) = list_rules(&state, actor.id, auth_age, &org_slug, &repo_name).await?;
    Ok(Json(TagProtectionList { generation, items }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTagRule {
    pattern: String,
    #[serde(default = "default_admin")]
    allowed_to_create: String,
    #[serde(default)]
    allow_update: bool,
    #[serde(default)]
    allow_delete: bool,
}

fn default_admin() -> String {
    "admin".to_owned()
}

fn validate_rule(input: &NewTagRule) -> Result<(), ApiError> {
    if parse_pattern(&input.pattern).is_none() || !valid_create_role(&input.allowed_to_create) {
        return Err(invalid());
    }
    Ok(())
}

/// Creates a tag protection rule as `actor_id`; shared by the JSON handler and
/// the settings page. Validation, authorization and audit happen once here.
pub(crate) async fn create_rule(
    state: &AppState,
    actor_id: Uuid,
    auth_age: u64,
    org_slug: &str,
    repo_name: &str,
    body: &[u8],
    key: Option<&str>,
) -> Result<(StatusCode, serde_json::Value), ApiError> {
    let path_str =
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/tag-protection");
    idempotency::run(
        &state.pool,
        actor_id,
        "POST",
        &path_str,
        body,
        key,
        |idem| async move {
            let input: NewTagRule = serde_json::from_slice(body).map_err(|_| invalid())?;
            validate_rule(&input)?;
            let loaded =
                repository_api::load_current(state, actor_id, auth_age, org_slug, repo_name)
                    .await?;
            repository_api::require(state, &loaded, Action::RepositoryManageProtection)?;
            if loaded.repo.status != githearth_core::RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let id = Uuid::new_v4();
            let inserted = sqlx::query(
                "INSERT INTO tag_protection_rules \
                 (id, repository_id, pattern, allowed_to_create, allow_update, allow_delete, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .bind(&input.pattern)
            .bind(&input.allowed_to_create)
            .bind(input.allow_update)
            .bind(input.allow_delete)
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
            crate::protection::bump_generation(&mut tx, loaded.repo.id).await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "tag_protection.created",
            )
            .await?;

            let rule: TagProtectionRule = sqlx::query_as(&format!(
                "SELECT {RULE_COLUMNS} FROM tag_protection_rules WHERE id = $1"
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

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, repo_name)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<TagProtectionRule>), ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let (status, value) = create_rule(
        &state, actor.id, auth_age, &org_slug, &repo_name, &body, key,
    )
    .await?;
    Ok((status, Json(serde_json::from_value(value).unwrap())))
}

/// Deletes a tag protection rule as `actor_id`; shared by the JSON handler and
/// the settings page.
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
        format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/tag-protection/{id}");
    idempotency::run(
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
            repository_api::require(state, &loaded, Action::RepositoryManageProtection)?;
            if loaded.repo.status != githearth_core::RepositoryStatus::Active {
                return Err(not_active());
            }

            let mut tx = state.pool.begin().await?;
            if !repositories::lock_active(&mut tx, loaded.repo.id, false).await? {
                return Err(not_active());
            }
            let deleted = sqlx::query(
                "DELETE FROM tag_protection_rules WHERE id = $1 AND repository_id = $2",
            )
            .bind(id)
            .bind(loaded.repo.id)
            .execute(&mut *tx)
            .await?;
            if deleted.rows_affected() == 0 {
                return Err(not_found());
            }
            crate::protection::bump_generation(&mut tx, loaded.repo.id).await?;
            event(
                &mut tx,
                Some(loaded.org_id),
                actor_id,
                "tag_protection.deleted",
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
    .await
    .map(|(status, _)| status)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, overrides: &dyn Fn(&mut TagProtectionRule)) -> TagProtectionRule {
        let mut r = TagProtectionRule {
            id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            pattern: pattern.to_owned(),
            allowed_to_create: "admin".to_owned(),
            allow_update: false,
            allow_delete: false,
            created_at: String::new(),
            updated_at: String::new(),
        };
        overrides(&mut r);
        r
    }

    #[test]
    fn pattern_parsing_accepts_and_rejects() {
        for (good, exact, glob) in [
            ("v1.0.0", Some("v1.0.0"), None),
            ("v*", None, Some("v")),
            ("release/*", None, Some("release/")),
            ("release*", None, Some("release")),
            ("v1.*", None, Some("v1.")),
            ("team/tag/name", Some("team/tag/name"), None),
        ] {
            assert_eq!(parse_pattern(good), Some((exact, glob)), "{good}");
        }
        for bad in [
            "",
            "*",
            "v**",
            "release**",
            "/*",
            "/v",
            "v/",
            "v//x",
            "a//b",
            "a/../b",
            "a/./b",
            "rel ease",
            "v *",
        ] {
            assert_eq!(parse_pattern(bad), None, "{bad}");
        }
    }

    #[test]
    fn matching_exact_beats_glob_and_longest_prefix_wins() {
        let v = rule("v*", &|r| r.allowed_to_create = "maintain".into());
        let v1 = rule("v1.*", &|r| r.allowed_to_create = "admin".into());
        let release = rule("release/*", &|_| {});
        let exact = rule("v1.2.3", &|_| {});
        let rules = [v, v1, release, exact];

        // Exact beats glob.
        let m = matching_rule(&rules, "refs/tags/v1.2.3").unwrap();
        assert_eq!(m.allowed_to_create, "admin");

        // Longest glob prefix wins.
        let m = matching_rule(&rules, "refs/tags/v1.4.0").unwrap();
        assert_eq!(m.allowed_to_create, "admin");
        let m = matching_rule(&rules, "refs/tags/v2.0.0").unwrap();
        assert_eq!(m.allowed_to_create, "maintain");
        let m = matching_rule(&rules, "refs/tags/release/1.0").unwrap();
        assert_eq!(m.pattern, "release/*");

        // No match, wrong namespace.
        assert!(matching_rule(&rules, "refs/tags/other").is_none());
        assert!(matching_rule(&rules, "refs/heads/v1.0.0").is_none());
        assert!(matching_rule(&rules, "v1.0.0").is_none());
        assert!(matching_rule(&[], "refs/tags/v1.0.0").is_none());
    }

    #[test]
    fn a_malformed_rule_does_not_disable_the_others() {
        let rules = [rule("bad/../x", &|_| {}), rule("v*", &|_| {})];
        assert!(matching_rule(&rules, "refs/tags/v1").is_some());
    }

    #[test]
    fn create_role_validation() {
        for good in CREATE_ROLES {
            assert!(valid_create_role(good), "{good}");
        }
        assert!(!valid_create_role("owner"));
        assert!(!valid_create_role("bogus"));
    }

    #[test]
    fn rank_comparison_orders_builtin_roles() {
        assert!(role_rank("member") < role_rank("read"));
        assert!(role_rank("read") < role_rank("triage"));
        assert!(role_rank("triage") < role_rank("write"));
        assert!(role_rank("write") < role_rank("maintain"));
        assert!(role_rank("maintain") < role_rank("admin"));
        assert!(role_rank("admin") < role_rank("owner"));
        assert_eq!(role_rank("nope"), u8::MAX);
    }

    use githearth_authz::{Account, Grant, Organization, Subject};

    fn facts_with_role(role: BuiltinRole, org: Uuid, repo: Uuid, scoped: bool) -> Facts {
        let account_id = Uuid::new_v4();
        let scope = if scoped {
            Scope::Repository(repo)
        } else {
            Scope::Organization(org)
        };
        Facts {
            account: Account {
                id: account_id,
                suspended: false,
                site_admin: false,
                organizations: vec![org],
                teams: vec![],
            },
            teams: vec![],
            grants: vec![Grant {
                role: Role::Builtin(role),
                subject: Subject::Account(account_id),
                scope,
            }],
            organization: Organization {
                id: org,
                suspended: false,
            },
            read_only: false,
        }
    }

    #[test]
    fn may_operate_respects_role_rank_and_scope() {
        let org = Uuid::new_v4();
        let repo = Uuid::new_v4();
        let other = Uuid::new_v4();

        // Org owner and org admin satisfy an 'admin' threshold.
        assert!(may_operate(
            &facts_with_role(BuiltinRole::Owner, org, repo, false),
            repo,
            org,
            "admin"
        ));
        assert!(may_operate(
            &facts_with_role(BuiltinRole::Admin, org, repo, false),
            repo,
            org,
            "admin"
        ));
        // A repo-scoped admin also satisfies it; a write role does not.
        assert!(may_operate(
            &facts_with_role(BuiltinRole::Admin, org, repo, true),
            repo,
            org,
            "admin"
        ));
        assert!(!may_operate(
            &facts_with_role(BuiltinRole::Write, org, repo, false),
            repo,
            org,
            "admin"
        ));
        // 'write' threshold is met by maintain/owner but not by triage.
        assert!(may_operate(
            &facts_with_role(BuiltinRole::Maintain, org, repo, false),
            repo,
            org,
            "write"
        ));
        assert!(!may_operate(
            &facts_with_role(BuiltinRole::Triage, org, repo, false),
            repo,
            org,
            "write"
        ));
        // A grant scoped to another repository never applies.
        assert!(!may_operate(
            &facts_with_role(BuiltinRole::Admin, org, other, true),
            repo,
            org,
            "admin"
        ));
    }

    #[test]
    fn decide_enforces_create_update_delete_defaults() {
        let org = Uuid::new_v4();
        let repo = Uuid::new_v4();
        let admin = facts_with_role(BuiltinRole::Admin, org, repo, false);
        let write = facts_with_role(BuiltinRole::Write, org, repo, false);

        // Default rule: admin may create, nobody may update or delete.
        let r = rule("v*", &|_| {});
        assert!(decide(&admin, repo, org, &r, true, false).is_ok());
        assert_eq!(
            decide(&write, repo, org, &r, true, false).unwrap_err(),
            "protected tag: ref.create"
        );
        assert_eq!(
            decide(&admin, repo, org, &r, false, false).unwrap_err(),
            "protected tag: ref.update"
        );
        assert_eq!(
            decide(&admin, repo, org, &r, false, true).unwrap_err(),
            "protected tag: ref.delete"
        );

        // allow_update / allow_delete gate the configured role, not just admins.
        let rw = rule("v*", &|r| {
            r.allowed_to_create = "write".into();
            r.allow_update = true;
            r.allow_delete = true;
        });
        assert!(decide(&write, repo, org, &rw, true, false).is_ok());
        assert!(decide(&write, repo, org, &rw, false, false).is_ok());
        assert!(decide(&write, repo, org, &rw, false, true).is_ok());
    }
}
