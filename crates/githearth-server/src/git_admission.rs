use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use githearth_authz::{Action, Organization, Principal, Ref, Request, Resource};
use githearth_git::journal::Update;
use std::time::Duration;
use uuid::Uuid;

use crate::{AppState, repositories, repositories::RepositoryRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Fetch,
    Push,
}

#[derive(Clone)]
pub enum CredentialRef {
    Password,
    Token(Uuid),
    SshKey(Uuid),
}

#[derive(Clone)]
pub struct Admitted {
    pub account: Uuid,
    pub organization: Uuid,
    pub repository: RepositoryRow,
    pub facts: crate::facts::Facts,
    pub rules: Vec<crate::protection::ProtectionRule>,
    pub tag_rules: Vec<crate::tag_protection::TagProtectionRule>,
    /// True when the credential was a read-only scoped access token.
    pub read_only: bool,
    pub credential: CredentialRef,
}

impl std::fmt::Debug for Admitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admitted")
            .field("account", &self.account)
            .field("organization", &self.organization)
            .field("repository", &self.repository)
            .finish()
    }
}

#[derive(Debug)]
pub struct AuthError(pub StatusCode, pub &'static str);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let AuthError(status, msg) = self;
        let mut response = (status, Json(serde_json::json!({"error": msg}))).into_response();
        // Git prompts for credentials only on a 401 challenge.
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                "www-authenticate",
                "Basic realm=\"githearth\"".parse().unwrap(),
            );
        }
        response
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|_| ())
}

fn parse_basic(headers: &HeaderMap) -> Result<(String, String), AuthError> {
    let header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired credentials",
        ))?;
    // RFC 7235: the scheme name is case-insensitive.
    let credentials = header
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Basic"))
        .map(|(_, rest)| rest)
        .ok_or(AuthError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired credentials",
        ))?;
    let decoded = base64_decode(credentials.trim())
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "invalid or expired credentials"))?;
    let text = std::str::from_utf8(&decoded)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, "invalid or expired credentials"))?;
    let (user, pass) = text.split_once(':').ok_or(AuthError(
        StatusCode::UNAUTHORIZED,
        "invalid or expired credentials",
    ))?;
    Ok((user.to_owned(), pass.to_owned()))
}

/// Git URLs are decoded before this point, so a `%00` in a path segment arrives
/// as NUL. PostgreSQL cannot store it, and querying with it must never happen.
fn reject_nul_path(org_slug: &str, repo_name: &str) -> Result<(), AuthError> {
    if crate::valid_path_param(org_slug) && crate::valid_path_param(repo_name) {
        Ok(())
    } else {
        Err(AuthError(StatusCode::BAD_REQUEST, "invalid input"))
    }
}

pub async fn admit(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
    repo_name: &str,
    want: Want,
) -> Result<Admitted, AuthError> {
    reject_nul_path(org_slug, repo_name)?;
    let (username, password) = parse_basic(headers)?;
    // A personal access token may be supplied as the Basic password (or the
    // username when only the token is embedded in the URL). Detect `gth_` and
    // short-circuit to the access-token path before password verification.
    let (account_id, read_only, credential) =
        if password.starts_with("gth_") || username.starts_with("gth_") {
            let token = if password.starts_with("gth_") {
                &password
            } else {
                &username
            };
            match crate::authenticate_access_token_plaintext(&state.pool, token).await {
                Ok(((account, _), token_id)) => (
                    account.id,
                    account.is_read_only(),
                    CredentialRef::Token(token_id),
                ),
                Err(e) if e.status_code() == StatusCode::UNAUTHORIZED => {
                    return Err(AuthError(
                        StatusCode::UNAUTHORIZED,
                        "invalid or expired credentials",
                    ));
                }
                Err(_) => {
                    return Err(AuthError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "authentication unavailable",
                    ));
                }
            }
        } else {
            let Some(account_id) = crate::verify_password(state, &username, &password)
                .await
                // Busy or unavailable is not a bad password: the client must not re-prompt.
                .map_err(|e| match e.0 {
                    StatusCode::TOO_MANY_REQUESTS => AuthError(e.0, "authentication busy"),
                    _ => AuthError(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "authentication unavailable",
                    ),
                })?
            else {
                return Err(AuthError(
                    StatusCode::UNAUTHORIZED,
                    "invalid or expired credentials",
                ));
            };
            (account_id, false, CredentialRef::Password)
        };

    admit_account(
        state, account_id, read_only, credential, org_slug, repo_name, want,
    )
    .await
}

/// Admit a fetch. A request without an Authorization header may read a public
/// repository anonymously; anything else must authenticate.
pub async fn admit_fetch(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
    repo_name: &str,
) -> Result<RepositoryRow, AuthError> {
    if headers.contains_key("authorization") {
        admit(state, headers, org_slug, repo_name, Want::Fetch)
            .await
            .map(|a| a.repository)
    } else {
        admit_anonymous(state, org_slug, repo_name).await
    }
}

/// The requester of a fetch: a signed-in account (whose facts were already
/// loaded) or an anonymous visitor.
#[derive(Clone, Copy)]
enum Requester<'a> {
    Account(&'a crate::facts::Facts),
    Anonymous,
}

/// Outcome of the shared fetch authorization, neutral about the caller's error
/// shape so the authenticated and anonymous paths map refusals their own way.
enum FetchAuth {
    Admitted(RepositoryRow),
    /// Missing, inactive, or not readable.
    Denied,
    /// Readable but not clonable (only produced when a clone is requested).
    NotClonable,
}

/// Loads the organization and repository for a fetch and runs the read (and,
/// when `clone` is set, the clone) authorization checks. Shared by the
/// authenticated ([`admit_account`]) and anonymous ([`admit_anonymous`]) paths
/// so the lookup and permission checks cannot drift apart. A database failure
/// surfaces as `SERVICE_UNAVAILABLE`; every refusal is reported through the
/// returned enum so each caller maps it to its own status code.
async fn authorize_fetch(
    state: &AppState,
    requester: Requester<'_>,
    org_slug: &str,
    repo_name: &str,
    clone: bool,
) -> Result<FetchAuth, AuthError> {
    reject_nul_path(org_slug, repo_name)?;
    let unavailable = |_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable");
    let mut conn = state.pool.acquire().await.map_err(unavailable)?;

    // The account path already loaded its facts, and therefore the organization;
    // the anonymous path looks the organization up by slug.
    let organization = match requester {
        Requester::Account(facts) => facts.organization.clone(),
        Requester::Anonymous => {
            let row: Option<(Uuid, bool)> =
                sqlx::query_as("SELECT id, suspended FROM organizations WHERE slug = $1")
                    .bind(org_slug)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(unavailable)?;
            match row {
                Some((id, suspended)) => Organization { id, suspended },
                None => return Ok(FetchAuth::Denied),
            }
        }
    };

    let Some(repo) = repositories::find_by_name(&mut conn, organization.id, repo_name)
        .await
        .map_err(unavailable)?
    else {
        return Ok(FetchAuth::Denied);
    };
    if repo.status != githearth_core::RepositoryStatus::Active {
        return Ok(FetchAuth::Denied);
    }

    let authz_repo = repositories::authz_repository(&repo);
    let read = match requester {
        Requester::Account(facts) => facts.request(
            Action::RepositoryRead,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(0)),
        ),
        Requester::Anonymous => Request {
            principal: Principal::Anonymous,
            teams: &[],
            grants: &[],
            action: Action::RepositoryRead,
            organization: &organization,
            resource: Resource::Repository(&authz_repo),
            merge: None,
            auth_age: None,
            read_only: false,
        },
    };
    if !state.authz.authorize(&read).is_allowed() {
        return Ok(FetchAuth::Denied);
    }

    if clone {
        let clone_req = match requester {
            Requester::Account(facts) => facts.request(
                Action::RepositoryClone,
                Resource::Repository(&authz_repo),
                Some(Duration::from_secs(0)),
            ),
            Requester::Anonymous => Request {
                principal: Principal::Anonymous,
                teams: &[],
                grants: &[],
                action: Action::RepositoryClone,
                organization: &organization,
                resource: Resource::Repository(&authz_repo),
                merge: None,
                auth_age: None,
                read_only: false,
            },
        };
        if !state.authz.authorize(&clone_req).is_allowed() {
            return Ok(FetchAuth::NotClonable);
        }
    }

    Ok(FetchAuth::Admitted(repo))
}

/// An anonymous visitor may fetch a public repository only; push is never
/// anonymous.
///
/// Every denial fails with 401 rather than 404: an unauthenticated client must
/// be challenged for credentials (and a missing repository must not be
/// distinguishable from a private one).
async fn admit_anonymous(
    state: &AppState,
    org_slug: &str,
    repo_name: &str,
) -> Result<RepositoryRow, AuthError> {
    reject_nul_path(org_slug, repo_name)?;
    let unauthorized = || AuthError(StatusCode::UNAUTHORIZED, "invalid or expired credentials");
    match authorize_fetch(state, Requester::Anonymous, org_slug, repo_name, true).await? {
        FetchAuth::Admitted(repo) => Ok(repo),
        FetchAuth::Denied | FetchAuth::NotClonable => Err(unauthorized()),
    }
}

/// Loads a repository an anonymous visitor may read, returning `None` for
/// anything not publicly readable (or when the database is unavailable) so the
/// web UI can fall back to a sign-in prompt without revealing the repository's
/// existence. This read authorization is the single source of the anonymous
/// public-read rule, shared with the git fetch path ([`admit_anonymous`]),
/// which additionally requires the clone action.
pub(crate) async fn anonymous_readable_repository(
    state: &AppState,
    org_slug: &str,
    repo_name: &str,
) -> Option<RepositoryRow> {
    match authorize_fetch(state, Requester::Anonymous, org_slug, repo_name, false).await {
        Ok(FetchAuth::Admitted(repo)) => Some(repo),
        _ => None,
    }
}

/// Authorization after authentication, shared by HTTP and SSH transports (ADR 0005).
pub async fn admit_account(
    state: &AppState,
    account_id: Uuid,
    read_only: bool,
    credential: CredentialRef,
    org_slug: &str,
    repo_name: &str,
    want: Want,
) -> Result<Admitted, AuthError> {
    reject_nul_path(org_slug, repo_name)?;
    let unavailable = |_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable");
    let mut conn = state.pool.acquire().await.map_err(unavailable)?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;

    let Some(facts) = crate::facts::load_scoped(&mut conn, account_id, org_id, read_only)
        .await
        .map_err(unavailable)?
    else {
        return Err(AuthError(StatusCode::NOT_FOUND, "not found"));
    };
    drop(conn);

    // Archived repositories stay readable; the engine refuses their ref updates.
    let repo = match authorize_fetch(
        state,
        Requester::Account(&facts),
        org_slug,
        repo_name,
        want == Want::Fetch,
    )
    .await?
    {
        FetchAuth::Admitted(repo) => repo,
        FetchAuth::Denied => return Err(AuthError(StatusCode::NOT_FOUND, "not found")),
        FetchAuth::NotClonable => return Err(AuthError(StatusCode::FORBIDDEN, "not permitted")),
    };

    let mut conn = state.pool.acquire().await.map_err(unavailable)?;
    let rules = crate::protection::load_rules(&mut conn, repo.id)
        .await
        .map_err(unavailable)?;
    let tag_rules = crate::tag_protection::load_rules(&mut conn, repo.id)
        .await
        .map_err(unavailable)?;

    // Refuse a push at admission, before the client uploads a pack. The
    // synthetic unprotected ref asks the same policy engine whether this
    // principal may push at all; per-branch protection is still decided by the
    // pre-receive hook once the actual updates are known.
    if want == Want::Push {
        let authz_repo = repositories::authz_repository(&repo);
        let reference = Ref {
            repository: repo.id,
            name: "refs/heads/githearth-admission".to_owned(),
            protection: None,
            exists: true,
        };
        let decision = state.authz.authorize(&facts.request(
            Action::RefPush,
            Resource::Ref(&authz_repo, &reference),
            Some(Duration::from_secs(0)),
        ));
        if !decision.is_allowed() {
            return Err(AuthError(StatusCode::FORBIDDEN, "not permitted"));
        }
    }

    Ok(Admitted {
        account: account_id,
        organization: org_id,
        repository: repo,
        facts,
        rules,
        tag_rules,
        read_only,
        credential,
    })
}

/// Re-reads the repository row and facts. The pre-receive hook runs after the whole pack has
/// uploaded, so call this before `decide_ref` to honour an archive or revoke made meanwhile.
pub async fn reload(state: &AppState, admitted: &Admitted) -> Result<Admitted, AuthError> {
    let unavailable = |_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable");
    let mut conn = state.pool.acquire().await.map_err(unavailable)?;
    let facts = crate::facts::load_scoped(
        &mut conn,
        admitted.account,
        admitted.organization,
        admitted.read_only,
    )
    .await
    .map_err(unavailable)?
    .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;
    let repository = repositories::get(&mut conn, admitted.repository.id)
        .await
        .map_err(unavailable)?
        .filter(|r| r.status == githearth_core::RepositoryStatus::Active)
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;
    let rules = crate::protection::load_rules(&mut conn, admitted.repository.id)
        .await
        .map_err(unavailable)?;
    let tag_rules = crate::tag_protection::load_rules(&mut conn, admitted.repository.id)
        .await
        .map_err(unavailable)?;
    Ok(Admitted {
        account: admitted.account,
        organization: admitted.organization,
        repository,
        facts,
        rules,
        tag_rules,
        read_only: admitted.read_only,
        credential: admitted.credential.clone(),
    })
}

pub fn decide_ref(
    state: &AppState,
    admitted: &Admitted,
    update: &Update,
    fast_forward: bool,
) -> Result<(), &'static str> {
    if !crate::pushes::valid_ref_name(&update.reference) {
        return Err("ref namespace is read-only");
    }

    let is_all_zero_new = update.new.bytes().all(|b| b == b'0');
    let all_zero_old = update.old.bytes().all(|b| b == b'0');

    // Tag protection (M3): a matching tag rule restricts create/update/delete.
    // This runs before the Cedar decision so both transports (and the shared
    // admission socket) agree on the reason.
    if let Some(rule) = crate::tag_protection::matching_rule(&admitted.tag_rules, &update.reference)
    {
        crate::tag_protection::decide(
            &admitted.facts,
            admitted.repository.id,
            admitted.organization,
            rule,
            !is_all_zero_new && all_zero_old,
            is_all_zero_new,
        )?;
    }

    let action = if is_all_zero_new {
        Action::RefDelete
    } else if !all_zero_old && !fast_forward {
        Action::RefForcePush
    } else {
        Action::RefPush
    };

    let authz_repo = repositories::authz_repository(&admitted.repository);
    let reference = Ref {
        repository: admitted.repository.id,
        name: update.reference.clone(),
        protection: crate::protection::matching(&admitted.rules, &update.reference),
        // A create (old oid all zeros) may target a protected glob; the rule
        // then applies to every later push.
        exists: !all_zero_old,
    };
    let decision = state.authz.authorize(&admitted.facts.request(
        action,
        Resource::Ref(&authz_repo, &reference),
        Some(Duration::from_secs(0)),
    ));

    if decision.is_allowed() {
        return Ok(());
    }

    // The deny reason names the policy id that refused the update. Branch
    // protection forbids report their own ids, which map to clearer reasons;
    // everything else keeps the existing `permission denied:` strings.
    let denied_by = |id: &str| match &decision {
        githearth_authz::Decision::Deny(reasons) => reasons.iter().any(|r| r == id),
        _ => false,
    };
    let reason = match action {
        _ if denied_by("protected-no-force-push")
            || denied_by("protected-requires-pull-request") =>
        {
            if action == Action::RefForcePush {
                "protected branch: ref.force_push"
            } else {
                "protected branch: ref.push"
            }
        }
        _ if denied_by("protected-no-deletion") => "protected branch: ref.delete",
        Action::RefDelete => "permission denied: ref.delete",
        Action::RefForcePush => "permission denied: ref.force_push",
        _ => "permission denied: ref.push",
    };
    Err(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use githearth_authz::{BuiltinRole, Engine, Grant, Organization, Role, Scope, Subject};
    use githearth_core::RepositoryStatus;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::Arc;

    const ZERO: &str = "0000000000000000000000000000000000000000";
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn test_state() -> AppState {
        let engine = Engine::new().unwrap();
        let root = std::env::temp_dir().join(format!("githearth-gad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let storage = githearth_git::layout::Storage::open(&root).unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/unused")
            .unwrap();
        AppState {
            pool,
            config: crate::config::Config {
                storage_root: root,
                database_url: "postgres://localhost/unused".into(),
                ..crate::config::Config::default()
            },
            auth_slots: Arc::new(tokio::sync::Semaphore::new(10)),
            git_slots: Arc::new(tokio::sync::Semaphore::new(10)),
            dummy_hash: Arc::new(String::new()),
            authz: Arc::new(engine),
            storage: Arc::new(storage),
            hook_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            hook_socket_path: std::path::PathBuf::from("/nonexistent/hook.sock"),
            trusted_proxies: Vec::new(),
        }
    }

    /// An admitted account; `owner` gives an organization Owner grant, otherwise no grants.
    fn admitted(owner: bool, archived: bool) -> Admitted {
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let organization = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: owner,
            organizations: vec![org_id],
            teams: vec![],
        };
        let grants = if owner {
            vec![Grant {
                role: Role::Builtin(BuiltinRole::Owner),
                subject: Subject::Account(account_id),
                scope: Scope::Organization(org_id),
            }]
        } else {
            vec![]
        };
        Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
                object_format: "sha1".into(),
                parent_repository_id: None,
            },
            facts: crate::facts::Facts {
                account,
                teams: vec![],
                grants,
                organization,
                read_only: false,
            },
            rules: vec![],
            tag_rules: vec![],
            read_only: false,
            credential: CredentialRef::Password,
        }
    }

    fn update(old: &str, new: &str, reference: &str) -> Update {
        Update {
            old: old.into(),
            new: new.into(),
            reference: reference.into(),
        }
    }

    #[tokio::test]
    async fn decide_ref_push_ok() {
        let u = update(ZERO, A, "refs/heads/main");
        assert!(decide_ref(&test_state(), &admitted(true, false), &u, true).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_delete() {
        let u = update(A, ZERO, "refs/heads/main");
        assert!(decide_ref(&test_state(), &admitted(true, false), &u, false).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_non_ff() {
        let u = update(
            A,
            "cccccccccccccccccccccccccccccccccccccccc",
            "refs/heads/main",
        );
        assert!(decide_ref(&test_state(), &admitted(true, false), &u, false).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_rejects_foreign_and_empty_refs() {
        for reference in ["refs/notes/review", "refs/heads/", "refs/tags/"] {
            let u = update(ZERO, A, reference);
            assert_eq!(
                decide_ref(&test_state(), &admitted(true, false), &u, true).unwrap_err(),
                "ref namespace is read-only",
                "{reference}"
            );
        }
    }

    #[tokio::test]
    async fn decide_ref_archived_denied() {
        let u = update(ZERO, A, "refs/heads/main");
        assert!(decide_ref(&test_state(), &admitted(true, true), &u, true).is_err());
    }

    #[tokio::test]
    async fn decide_ref_without_grant_denied() {
        let u = update(ZERO, A, "refs/heads/main");
        assert_eq!(
            decide_ref(&test_state(), &admitted(false, false), &u, true).unwrap_err(),
            "permission denied: ref.push"
        );
    }

    fn protected_rule(
        pattern: &str,
        require_pull_request: bool,
    ) -> crate::protection::ProtectionRule {
        crate::protection::ProtectionRule {
            id: uuid::Uuid::new_v4(),
            repository_id: uuid::Uuid::new_v4(),
            pattern: pattern.to_owned(),
            allow_force_push: false,
            allow_deletion: false,
            require_pull_request,
            required_approvals: 0,
            required_status_checks: Vec::new(),
            dismiss_stale_approvals: true,
            require_signed_commits: false,
            require_merge_queue: false,
            require_code_owner_review: false,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn decide_ref_creation_of_a_protected_branch_is_allowed_but_updates_are_refused() {
        let mut a = admitted(true, false);
        a.rules = vec![protected_rule("release/*", true)];

        // Creating release/2.0 (old all zeros) is allowed; the rule applies
        // from the next push onward.
        let create = update(ZERO, A, "refs/heads/release/2.0");
        assert!(decide_ref(&test_state(), &a, &create, false).is_ok());

        // A fast-forward push to the now-existing branch is refused.
        let push = update(
            A,
            "cccccccccccccccccccccccccccccccccccccccc",
            "refs/heads/release/2.0",
        );
        assert_eq!(
            decide_ref(&test_state(), &a, &push, true).unwrap_err(),
            "protected branch: ref.push"
        );
    }

    fn tag_rule(
        pattern: &str,
        allow_update: bool,
        allow_delete: bool,
    ) -> crate::tag_protection::TagProtectionRule {
        crate::tag_protection::TagProtectionRule {
            id: uuid::Uuid::new_v4(),
            repository_id: uuid::Uuid::new_v4(),
            pattern: pattern.to_owned(),
            allowed_to_create: "admin".to_owned(),
            allow_update,
            allow_delete,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn protected_tag_create_requires_the_configured_role() {
        // An owner (admin) may create a protected tag; a member without a
        // write-or-higher grant may not.
        let mut a = admitted(true, false);
        a.tag_rules = vec![tag_rule("v*", false, false)];
        let create = update(ZERO, A, "refs/tags/v1.0.0");
        assert!(decide_ref(&test_state(), &a, &create, false).is_ok());

        let mut nobody = admitted(false, false);
        nobody.tag_rules = vec![tag_rule("v*", false, false)];
        assert_eq!(
            decide_ref(&test_state(), &nobody, &create, false).unwrap_err(),
            "protected tag: ref.create"
        );
    }

    #[tokio::test]
    async fn protected_tag_update_and_delete_are_refused_by_default() {
        let mut a = admitted(true, false);
        a.tag_rules = vec![tag_rule("v*", false, false)];

        // Move (update): both oids non-zero.
        let mv = update(
            A,
            "cccccccccccccccccccccccccccccccccccccccc",
            "refs/tags/v1.0.0",
        );
        assert_eq!(
            decide_ref(&test_state(), &a, &mv, false).unwrap_err(),
            "protected tag: ref.update"
        );

        // Delete: new oid all zeros.
        let del = update(A, ZERO, "refs/tags/v1.0.0");
        assert_eq!(
            decide_ref(&test_state(), &a, &del, false).unwrap_err(),
            "protected tag: ref.delete"
        );
    }

    #[tokio::test]
    async fn unprotected_tags_are_unaffected() {
        // No matching tag rule: the owner may create, move and delete a tag.
        let a = admitted(true, false);
        let create = update(ZERO, A, "refs/tags/v2.0.0");
        assert!(decide_ref(&test_state(), &a, &create, false).is_ok());
        let mv = update(
            A,
            "cccccccccccccccccccccccccccccccccccccccc",
            "refs/tags/v2.0.0",
        );
        assert!(decide_ref(&test_state(), &a, &mv, false).is_ok());
        let del = update(A, ZERO, "refs/tags/v2.0.0");
        assert!(decide_ref(&test_state(), &a, &del, false).is_ok());
    }

    #[test]
    fn basic_scheme_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        // "u:p" in base64
        headers.insert("authorization", "bAsIc dTpw".parse().unwrap());
        assert_eq!(parse_basic(&headers).unwrap(), ("u".into(), "p".into()));
        headers.insert("authorization", "Bearer dTpw".parse().unwrap());
        assert!(parse_basic(&headers).is_err());
    }
}
