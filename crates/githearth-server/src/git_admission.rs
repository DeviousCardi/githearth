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
    let credentials = header.strip_prefix("Basic ").ok_or(AuthError(
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

pub fn decide_ref(
    state: &AppState,
    admitted: &Admitted,
    update: &Update,
    fast_forward: bool,
) -> Result<(), &'static str> {
    if !update.reference.starts_with("refs/heads/") && !update.reference.starts_with("refs/tags/") {
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
            dummy_hash: Arc::new(String::new()),
            authz: Arc::new(engine),
            storage: Arc::new(storage),
        }
    }

    #[tokio::test]
    async fn decide_ref_push_ok() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: true,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![Grant {
                role: Role::Builtin(BuiltinRole::Owner),
                subject: Subject::Account(account_id),
                scope: Scope::Organization(org_id),
            }],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: false,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "0000000000000000000000000000000000000000".into(),
            new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            reference: "refs/heads/main".into(),
        };
        assert!(decide_ref(&state, &admitted, &update, true).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_delete() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: true,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![Grant {
                role: Role::Builtin(BuiltinRole::Owner),
                subject: Subject::Account(account_id),
                scope: Scope::Organization(org_id),
            }],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: false,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            new: "0000000000000000000000000000000000000000".into(),
            reference: "refs/heads/main".into(),
        };
        // Site admin with Owner grant — RefDelete should be allowed
        assert!(decide_ref(&state, &admitted, &update, false).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_non_ff() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: true,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![Grant {
                role: Role::Builtin(BuiltinRole::Owner),
                subject: Subject::Account(account_id),
                scope: Scope::Organization(org_id),
            }],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: false,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            new: "cccccccccccccccccccccccccccccccccccccccc".into(),
            reference: "refs/heads/main".into(),
        };
        // Non-ff with Owner grant should be allowed
        assert!(decide_ref(&state, &admitted, &update, false).is_ok());
    }

    #[tokio::test]
    async fn decide_ref_rejects_refs_notes() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: false,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: false,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "0000000000000000000000000000000000000000".into(),
            new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            reference: "refs/notes/review".into(),
        };
        assert_eq!(
            decide_ref(&state, &admitted, &update, true).unwrap_err(),
            "ref namespace is read-only"
        );
    }

    #[tokio::test]
    async fn decide_ref_archived_denied() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: false,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: true,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "0000000000000000000000000000000000000000".into(),
            new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            reference: "refs/heads/main".into(),
        };
        let result = decide_ref(&state, &admitted, &update, true);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn decide_ref_read_only_role_denied() {
        let state = test_state();
        let org_id = uuid::Uuid::new_v4();
        let account_id = uuid::Uuid::new_v4();
        let org = Organization {
            id: org_id,
            suspended: false,
        };
        let account = githearth_authz::Account {
            id: account_id,
            suspended: false,
            site_admin: false,
            organizations: vec![org_id],
            teams: vec![],
        };
        let facts = crate::facts::Facts {
            account,
            teams: vec![],
            grants: vec![],
            organization: org,
        };
        let admitted = Admitted {
            account: account_id,
            organization: org_id,
            repository: RepositoryRow {
                id: uuid::Uuid::new_v4(),
                organization_id: org_id,
                name: "test".into(),
                description: "".into(),
                visibility: "private".into(),
                archived: false,
                status: RepositoryStatus::Active,
                operation_id: uuid::Uuid::new_v4(),
                created_by: uuid::Uuid::new_v4(),
            },
            facts,
        };

        let update = Update {
            old: "0000000000000000000000000000000000000000".into(),
            new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            reference: "refs/heads/main".into(),
        };
        let result = decide_ref(&state, &admitted, &update, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "permission denied: ref.push");
    }
}
