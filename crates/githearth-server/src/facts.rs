//! Loading persisted authorization facts for one decision
//! (docs/decisions/0002-authorization.md).

use std::time::Duration;

use githearth_authz::{
    Account, Action, BuiltinRole, Grant, Organization, Principal, Request, Resource, Role, Scope,
    Subject, Team,
};
use sqlx::{Connection, PgConnection, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct Facts {
    pub account: Account,
    pub teams: Vec<Team>,
    pub grants: Vec<Grant>,
    pub organization: Organization,
    /// True when the request presented a read-only scoped access token: the
    /// engine refuses every action outside the read group regardless of grants.
    pub read_only: bool,
}

impl Facts {
    pub fn request<'a>(
        &'a self,
        action: Action,
        resource: Resource<'a>,
        auth_age: Option<Duration>,
    ) -> Request<'a> {
        Request {
            principal: Principal::Account(&self.account),
            teams: &self.teams,
            grants: &self.grants,
            action,
            organization: &self.organization,
            resource,
            merge: None,
            auth_age,
            read_only: self.read_only,
        }
    }
}

/// The account's direct teams plus all of their ancestors.
const TEAMS_CTE: &str = "WITH RECURSIVE tree AS (\
 SELECT t.id, t.organization_id, t.parent_id FROM teams t \
 JOIN team_members m ON m.team_id = t.id \
 WHERE m.account_id = $1 AND t.organization_id = $2 \
 UNION \
 SELECT p.id, p.organization_id, p.parent_id FROM teams p \
 JOIN tree c ON p.id = c.parent_id WHERE p.organization_id = $2) ";

/// One consistent snapshot per decision: the facts a request needs must not
/// straddle a committed change.
pub async fn load(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
) -> Result<Option<Facts>, sqlx::Error> {
    // Read-only enforcement for JSON API writes is applied at the router
    // boundary (a middleware layer), which has the credential and therefore
    // knows the token's scopes; this ABI only sees an account id and loads a
    // fully-privileged snapshot.
    load_scoped(conn, account_id, organization_id, false).await
}

/// Facts for instance (site-level) administration, used with
/// [`Resource::Instance`](githearth_authz::Resource::Instance): the acting
/// account and nothing else. The synthetic organization is never read by an
/// instance action; it exists only because a request always names one. Returns
/// `None` for an unknown account.
pub async fn load_site(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<Facts>, sqlx::Error> {
    let Some(account) = sqlx::query(
        "SELECT (suspended OR disabled_at IS NOT NULL) AS suspended, is_admin \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(Facts {
        account: Account {
            id: account_id,
            suspended: account.get("suspended"),
            site_admin: account.get("is_admin"),
            organizations: vec![],
            teams: vec![],
        },
        teams: vec![],
        grants: vec![],
        organization: Organization {
            id: Uuid::nil(),
            suspended: false,
        },
        read_only: false,
    }))
}

/// The same snapshot as [`load`], but marking the principal as read-only when
/// the request authenticated with a scoped access token lacking `repo:write`.
pub async fn load_scoped(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
    read_only: bool,
) -> Result<Option<Facts>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let organization = match sqlx::query("SELECT suspended FROM organizations WHERE id = $1")
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?
    {
        Some(row) => Organization {
            id: organization_id,
            suspended: row.get("suspended"),
        },
        None => return Ok(None),
    };
    // A disabled account has no active principal: `disabled_at` and `suspended`
    // are folded into the one fact the engine's `suspended-account` forbid reads.
    let Some(account) = sqlx::query(
        "SELECT (suspended OR disabled_at IS NOT NULL) AS suspended, is_admin \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE account_id = $1 AND organization_id = $2)",
    )
    .bind(account_id)
    .bind(organization_id)
    .fetch_one(&mut *tx)
    .await?;
    // The engine links Team grants through the Account's direct teams even
    // when the grant sits on an ancestor.
    let direct: Vec<Uuid> = sqlx::query(
        "SELECT t.id FROM teams t JOIN team_members m ON m.team_id = t.id \
 WHERE m.account_id = $1 AND t.organization_id = $2 ORDER BY t.id",
    )
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| row.get("id"))
    .collect();
    let account = Account {
        id: account_id,
        suspended: account.get("suspended"),
        site_admin: account.get("is_admin"),
        organizations: if member {
            vec![organization_id]
        } else {
            vec![]
        },
        teams: direct,
    };
    let teams: Vec<Team> = sqlx::query(&format!(
        "{TEAMS_CTE} SELECT id, organization_id, parent_id FROM tree ORDER BY id"
    ))
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| Team {
        id: row.get("id"),
        organization: row.get("organization_id"),
        parent: row.get("parent_id"),
    })
    .collect();
    let mut grants = Vec::new();
    for row in sqlx::query(&format!(
        "{TEAMS_CTE} SELECT role, subject_kind, subject_id, repository_id FROM grants \
 WHERE scope_id = $2 AND revoked_at IS NULL \
 AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
 AND ((subject_kind = 'account' AND subject_id = $1) \
 OR (subject_kind = 'team' AND subject_id IN (SELECT id FROM tree))) \
 ORDER BY id"
    ))
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let role = match row.get::<String, _>("role").as_str() {
            "member" => Some(BuiltinRole::Member),
            "read" => Some(BuiltinRole::Read),
            "triage" => Some(BuiltinRole::Triage),
            "write" => Some(BuiltinRole::Write),
            "maintain" => Some(BuiltinRole::Maintain),
            "admin" => Some(BuiltinRole::Admin),
            "owner" => Some(BuiltinRole::Owner),
            role => {
                tracing::warn!(role, %organization_id, "unknown grant role ignored");
                None
            }
        };
        let Some(role) = role else { continue };
        let subject = match row.get::<String, _>("subject_kind").as_str() {
            "account" => Subject::Account(row.get("subject_id")),
            "team" => Subject::Team(row.get("subject_id")),
            kind => {
                tracing::warn!(kind, %organization_id, "unknown grant subject ignored");
                continue;
            }
        };
        let scope = if let Some(repo_id) = row.get::<Option<Uuid>, _>("repository_id") {
            Scope::Repository(repo_id)
        } else {
            Scope::Organization(organization_id)
        };
        grants.push(Grant {
            role: Role::Builtin(role),
            subject,
            scope,
        });
    }
    tx.commit().await?;
    Ok(Some(Facts {
        account,
        teams,
        grants,
        organization,
        read_only,
    }))
}
