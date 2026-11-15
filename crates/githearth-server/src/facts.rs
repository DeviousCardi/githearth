//! Loading persisted authorization facts for one decision
//! (docs/decisions/0002-authorization.md).

use std::time::Duration;

use githearth_authz::{
    Account, Action, BuiltinRole, Confinement, Grant, Organization, Principal, Request, Resource,
    Role, Run, Scope, ServiceAccount, Subject, Team,
};
use sqlx::{Connection, PgConnection, Row};
use uuid::Uuid;

/// The principal a snapshot authorizes for. `Account` is the historical path;
/// `ServiceAccount` and `Run` are the non-account subjects from ADR 0014.
#[derive(Clone)]
pub enum PrincipalFacts {
    Account(Account),
    ServiceAccount(ServiceAccount),
    Run(Run),
}

#[derive(Clone)]
pub struct Facts {
    /// The account a decision acts for: the signed-in account, or (for a run)
    /// the account that triggered it. A service-account snapshot carries a
    /// placeholder so the historical field stays present; `principal` is what
    /// the engine uses.
    pub account: Account,
    pub principal: Option<PrincipalFacts>,
    pub teams: Vec<Team>,
    pub grants: Vec<Grant>,
    pub organization: Organization,
    /// True when the request presented a read-only scoped access token: the
    /// engine refuses every action outside the read group regardless of grants.
    pub read_only: bool,
}

impl Facts {
    pub fn principal<'a>(&'a self) -> Principal<'a> {
        match &self.principal {
            Some(PrincipalFacts::Account(account)) => Principal::Account(account),
            Some(PrincipalFacts::ServiceAccount(account)) => Principal::ServiceAccount(account),
            Some(PrincipalFacts::Run(run)) => Principal::Run(run),
            None => Principal::Account(&self.account),
        }
    }

    pub fn request<'a>(
        &'a self,
        action: Action,
        resource: Resource<'a>,
        auth_age: Option<Duration>,
    ) -> Request<'a> {
        Request {
            principal: self.principal(),
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

    /// One action against the organization itself. Invitation acceptance uses
    /// it so it asks the engine the same question the JSON handlers do
    /// (ADR 0002), with the same principal, teams and grants.
    pub fn on_organization<'s>(
        &'s self,
        action: Action,
        auth_age: Option<Duration>,
    ) -> Request<'s> {
        self.request(action, Resource::Organization, auth_age)
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

/// The same snapshot as [`load`], but marking the principal as read-only when
/// the request authenticated with a scoped access token lacking `repo:write`.
pub async fn load_scoped(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
    read_only: bool,
) -> Result<Option<Facts>, sqlx::Error> {
    load_scoped_confined(conn, account_id, organization_id, read_only, None).await
}

/// The same snapshot with the token's resource-set/scope/audience confinement
/// attached to the account principal.
pub async fn load_scoped_confined(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
    read_only: bool,
    confinement: Option<Confinement>,
) -> Result<Option<Facts>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let facts = snapshot(&mut tx, account_id, organization_id, read_only, confinement).await?;
    tx.commit().await?;
    Ok(facts)
}

/// Load facts for any token subject. The caller has already authenticated the
/// credential and checked its audience; this snapshot enforces the token's
/// resource set and scopes inside the engine.
pub async fn load_actor(
    conn: &mut PgConnection,
    actor: &crate::Actor,
    organization_id: Uuid,
) -> Result<Option<Facts>, sqlx::Error> {
    match actor {
        crate::Actor::Account {
            account,
            confinement,
            ..
        } => {
            load_scoped_confined(
                conn,
                account.id,
                organization_id,
                account.is_read_only(),
                confinement.clone(),
            )
            .await
        }
        crate::Actor::ServiceAccount {
            id, confinement, ..
        } => load_service_account(conn, *id, organization_id, confinement.clone()).await,
        crate::Actor::Run {
            id, confinement, ..
        } => load_run(conn, *id, organization_id, confinement.clone()).await,
    }
}

/// Load the same snapshot on a connection that is already inside a transaction.
///
/// Membership mutations take the organization row lock and then re-read the
/// caller's authorization in that transaction: the locked read must see a
/// concurrent demotion that committed while this request waited, so this uses
/// the transaction's own (READ COMMITTED) snapshot instead of opening the
/// read-only `REPEATABLE READ` transaction [`load`] uses.
pub async fn load_in_transaction(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
) -> Result<Option<Facts>, sqlx::Error> {
    snapshot(conn, account_id, organization_id, false, None).await
}

async fn organization(
    conn: &mut PgConnection,
    organization_id: Uuid,
) -> Result<Option<Organization>, sqlx::Error> {
    Ok(
        sqlx::query("SELECT suspended FROM organizations WHERE id = $1")
            .bind(organization_id)
            .fetch_optional(&mut *conn)
            .await?
            .map(|row| Organization {
                id: organization_id,
                suspended: row.get("suspended"),
            }),
    )
}

async fn account_row(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<(Account, bool)>, sqlx::Error> {
    let Some(row) =
        sqlx::query("SELECT suspended, is_admin, must_change_password FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&mut *conn)
            .await?
    else {
        return Ok(None);
    };
    Ok(Some((
        Account {
            id: account_id,
            suspended: row.get("suspended"),
            site_admin: row.get("is_admin"),
            organizations: Vec::new(),
            teams: Vec::new(),
            confinement: None,
        },
        row.get("must_change_password"),
    )))
}

async fn account_snapshot(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
    read_only: bool,
    confinement: Option<Confinement>,
) -> Result<Option<Facts>, sqlx::Error> {
    let Some(organization) = organization(conn, organization_id).await? else {
        return Ok(None);
    };
    let Some((mut account, _must_change)) = account_row(conn, account_id).await? else {
        return Ok(None);
    };
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE account_id = $1 AND organization_id = $2)",
    )
    .bind(account_id)
    .bind(organization_id)
    .fetch_one(&mut *conn)
    .await?;
    // The engine links Team grants through the Account's direct teams even
    // when the grant sits on an ancestor.
    let direct: Vec<Uuid> = sqlx::query(
        "SELECT t.id FROM teams t JOIN team_members m ON m.team_id = t.id \
 WHERE m.account_id = $1 AND t.organization_id = $2 ORDER BY t.id",
    )
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| row.get("id"))
    .collect();
    account.organizations = if member {
        vec![organization_id]
    } else {
        vec![]
    };
    account.teams = direct;
    account.confinement = confinement;
    let teams = teams(conn, account_id, organization_id).await?;
    let grants = account_grants(conn, account_id, organization_id).await?;
    Ok(Some(Facts {
        account,
        principal: None,
        teams,
        grants,
        organization,
        read_only,
    }))
}

async fn snapshot(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
    read_only: bool,
    confinement: Option<Confinement>,
) -> Result<Option<Facts>, sqlx::Error> {
    account_snapshot(conn, account_id, organization_id, read_only, confinement).await
}

async fn load_service_account(
    conn: &mut PgConnection,
    service_account_id: Uuid,
    organization_id: Uuid,
    confinement: Option<Confinement>,
) -> Result<Option<Facts>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let Some(organization) = organization(&mut tx, organization_id).await? else {
        tx.commit().await?;
        return Ok(None);
    };
    let Some(row) = sqlx::query(
        "SELECT id, organization_id, disabled_at IS NOT NULL AS disabled \
         FROM service_accounts WHERE id = $1 AND organization_id = $2",
    )
    .bind(service_account_id)
    .bind(organization_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(None);
    };
    let principal = ServiceAccount {
        id: service_account_id,
        organization: organization_id,
        suspended: row.get("disabled"),
        confinement,
    };
    let grants = service_account_grants(&mut tx, service_account_id, organization_id).await?;
    // The historical field is unused for a service account; a placeholder keeps
    // the struct shape without inventing an owning account.
    let account = Account {
        id: Uuid::nil(),
        suspended: principal.suspended,
        site_admin: false,
        organizations: vec![organization_id],
        teams: Vec::new(),
        confinement: None,
    };
    tx.commit().await?;
    Ok(Some(Facts {
        account,
        principal: Some(PrincipalFacts::ServiceAccount(principal)),
        teams: Vec::new(),
        grants,
        organization,
        read_only: false,
    }))
}

async fn load_run(
    conn: &mut PgConnection,
    run_id: Uuid,
    organization_id: Uuid,
    confinement: Option<Confinement>,
) -> Result<Option<Facts>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let Some(_organization) = organization(&mut tx, organization_id).await? else {
        tx.commit().await?;
        return Ok(None);
    };
    let Some(row) = sqlx::query(
        "SELECT triggered_by, status, (ended_at IS NOT NULL) AS ended FROM ci_runs \
         WHERE id = $1 AND organization_id = $2",
    )
    .bind(run_id)
    .bind(organization_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(None);
    };
    let account_id: Uuid = row.get("triggered_by");
    let ended = row.get::<bool, _>("ended") || row.get::<String, _>("status") != "running";
    // Reuse the account snapshot's facts for the triggering account.
    let mut facts = account_snapshot(
        &mut tx,
        account_id,
        organization_id,
        false,
        // The run, not the account, carries the token's confinement.
        None,
    )
    .await?
    .expect("run organization and account were just checked");
    let run = Run {
        id: run_id,
        organization: organization_id,
        account: facts.account.clone(),
        ended,
        confinement,
    };
    facts.principal = Some(PrincipalFacts::Run(run));
    tx.commit().await?;
    Ok(Some(facts))
}

async fn teams(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
) -> Result<Vec<Team>, sqlx::Error> {
    Ok(sqlx::query(&format!(
        "{TEAMS_CTE} SELECT id, organization_id, parent_id FROM tree ORDER BY id"
    ))
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| Team {
        id: row.get("id"),
        organization: row.get("organization_id"),
        parent: row.get("parent_id"),
    })
    .collect())
}

async fn account_grants(
    conn: &mut PgConnection,
    account_id: Uuid,
    organization_id: Uuid,
) -> Result<Vec<Grant>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "{TEAMS_CTE} SELECT role, subject_kind, subject_id, repository_id FROM grants \
 WHERE scope_id = $2 AND revoked_at IS NULL \
 AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
 AND ((subject_kind = 'account' AND subject_id = $1) \
 OR (subject_kind = 'team' AND subject_id IN (SELECT id FROM tree))) \
 ORDER BY id"
    ))
    .bind(account_id)
    .bind(organization_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(grants_from_rows(rows, organization_id))
}

async fn service_account_grants(
    conn: &mut PgConnection,
    service_account_id: Uuid,
    organization_id: Uuid,
) -> Result<Vec<Grant>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT role, subject_kind, subject_id, repository_id FROM grants \
 WHERE scope_id = $2 AND subject_kind = 'service_account' AND subject_id = $1 \
 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
 ORDER BY id",
    )
    .bind(service_account_id)
    .bind(organization_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(grants_from_rows(rows, organization_id))
}

fn grants_from_rows(rows: Vec<sqlx::postgres::PgRow>, organization_id: Uuid) -> Vec<Grant> {
    let mut grants = Vec::new();
    for row in rows {
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
            "service_account" => Subject::ServiceAccount(row.get("subject_id")),
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
    grants
}
