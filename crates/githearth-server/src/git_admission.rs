use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use githearth_authz::{Action, Ref, Resource};
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
pub struct Admitted {
    pub account: Uuid,
    pub organization: Uuid,
    pub repository: RepositoryRow,
    pub facts: crate::facts::Facts,
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

pub async fn admit(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
    repo_name: &str,
    want: Want,
) -> Result<Admitted, AuthError> {
    let (username, password) = parse_basic(headers)?;
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

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable"))?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable"))?
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;

    let Some(facts) = crate::facts::load(&mut conn, account_id, org_id)
        .await
        .map_err(|_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable"))?
    else {
        return Err(AuthError(StatusCode::NOT_FOUND, "not found"));
    };

    let repo = repositories::find_by_name(&mut conn, org_id, repo_name)
        .await
        .map_err(|_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable"))?
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;

    // Archived repositories stay readable; the engine refuses their ref updates.
    if repo.status != githearth_core::RepositoryStatus::Active {
        return Err(AuthError(StatusCode::NOT_FOUND, "not found"));
    }

    let authz_repo = repositories::authz_repository(&repo);
    let read_decision = state.authz.authorize(&facts.request(
        Action::RepositoryRead,
        Resource::Repository(&authz_repo),
        Some(Duration::from_secs(0)),
    ));
    if !read_decision.is_allowed() {
        return Err(AuthError(StatusCode::NOT_FOUND, "not found"));
    }

    if want == Want::Fetch {
        let clone_decision = state.authz.authorize(&facts.request(
            Action::RepositoryClone,
            Resource::Repository(&authz_repo),
            Some(Duration::from_secs(0)),
        ));
        if !clone_decision.is_allowed() {
            return Err(AuthError(StatusCode::FORBIDDEN, "not permitted"));
        }
    }

    Ok(Admitted {
        account: account_id,
        organization: org_id,
        repository: repo,
        facts,
    })
}

/// Re-reads the repository row and facts. The pre-receive hook runs after the whole pack has
/// uploaded, so call this before `decide_ref` to honour an archive or revoke made meanwhile.
pub async fn reload(state: &AppState, admitted: &Admitted) -> Result<Admitted, AuthError> {
    let unavailable = |_| AuthError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable");
    let mut conn = state.pool.acquire().await.map_err(unavailable)?;
    let facts = crate::facts::load(&mut conn, admitted.account, admitted.organization)
        .await
        .map_err(unavailable)?
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;
    let repository = repositories::get(&mut conn, admitted.repository.id)
        .await
        .map_err(unavailable)?
        .filter(|r| r.status == githearth_core::RepositoryStatus::Active)
        .ok_or(AuthError(StatusCode::NOT_FOUND, "not found"))?;
    Ok(Admitted {
        account: admitted.account,
        organization: admitted.organization,
        repository,
        facts,
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
        protection: None,
    };
    let decision = state.authz.authorize(&admitted.facts.request(
        action,
        Resource::Ref(&authz_repo, &reference),
        Some(Duration::from_secs(0)),
    ));

    if decision.is_allowed() {
        return Ok(());
    }

    let reason = match action {
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
            },
            facts: crate::facts::Facts {
                account,
                teams: vec![],
                grants,
                organization,
            },
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
