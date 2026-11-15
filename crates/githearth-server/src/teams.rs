//! Teams: create, delete, list members, add and remove members, and carry
//! repository grants (M4.1 acceptance case U05).
//!
//! A team belongs to one organization and its members must be organization
//! members; the `team_members` trigger has enforced the second since migration
//! 0002 and the organization slug rules govern a team slug. A team carries
//! grants exactly as an account does, and authorization reads them through the
//! same facts loader as the membership slice ([`crate::facts`]): a decision for
//! a team member is the union of the member's direct grants and the grants of
//! every team they belong to, on the API, the browser and both Git transports,
//! because every one of them loads those facts.
//!
//! Authority is the organization-level, owner-only `team.*` actions (ADR 0002):
//! `team.create` for a new team, `team.update` for team membership and for
//! adding a grant, and `team.delete` for deleting a team or revoking one of its
//! grants. Adding a grant is an update to the team's access, not a deletion, so
//! it does not ride on `team.delete`: a role that may `team.update` but not
//! `team.delete` can grant, and cannot delete the team or revoke a grant.
//! Nothing here deletes a grant: `grants` rows are revoked, never
//! deleted, and the 0002 trigger refuses deletion outside organization removal.
//! Removing a team member, deleting a team and revoking a team grant each deny
//! new admission on the next request, including for an already-issued token or
//! SSH key, because every admission re-reads grants.
//!
//! Whoever may administer a team can hand it an organization `owner` grant, so
//! that grant counts toward the last-owner rule exactly as a member's owner row
//! does — but only while a live member can exercise it. Every path that can
//! remove the last reachable owner takes the organization row lock first and
//! re-counts under it with `membership::reachable_owner_count`, so two
//! administrators cannot each remove a different last owner at once, and the
//! team-member, membership-removal and demotion paths share that one rule.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
};
use githearth_authz::Action;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Page, authenticate, event_with, idempotency, invalid, membership,
    valid_path_param, valid_username,
};

/// The roles a team may carry on a repository: every built-in role except the
/// two organization membership roles, which describe the organization itself
/// and that the grant trigger refuses on a repository.
pub(crate) const REPOSITORY_ROLES: [&str; 5] = ["read", "triage", "write", "maintain", "admin"];

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/teams",
            post(create_team).get(list_teams),
        )
        .route(
            "/api/v1/organizations/{org}/teams/{team}",
            delete(delete_team),
        )
        .route(
            "/api/v1/organizations/{org}/teams/{team}/members",
            post(add_team_member).get(list_team_members),
        )
        .route(
            "/api/v1/organizations/{org}/teams/{team}/members/{username}",
            delete(remove_team_member),
        )
        .route(
            "/api/v1/organizations/{org}/teams/{team}/grants",
            post(grant_team).get(list_team_grants),
        )
        .route(
            "/api/v1/organizations/{org}/teams/{team}/grants/{grant}",
            delete(revoke_team_grant),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}

fn no_such_team() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "team not found")
}

fn no_such_member() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "member not found")
}

fn no_such_grant() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "grant not found")
}

/// A team as the API returns it.
#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct TeamJson {
    pub(crate) id: Uuid,
    pub(crate) slug: String,
    pub(crate) name: String,
    pub(crate) created_at: String,
}

const TEAM_COLUMNS: &str = "t.id, t.slug, t.name, \
 to_char(t.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

/// A team's active grant: on a repository (`repository` names it) or, when
/// `repository` is null, on the organization itself.
#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct GrantJson {
    pub(crate) id: Uuid,
    pub(crate) role: String,
    pub(crate) repository: Option<String>,
    pub(crate) created_at: String,
}

const GRANT_COLUMNS: &str = "g.id, g.role, r.name AS repository, \
 to_char(g.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

/// A team slug follows the organization slug rules, which the path segment of
/// every team route already assumes.
pub(crate) fn valid_team_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A team's display name: short, printable, without the NUL PostgreSQL text
/// cannot hold.
pub(crate) fn valid_team_name(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().count() <= 200 && !crate::has_nul(value)
}

/// The organization a team request addresses, plus the team once resolved.
pub(crate) struct Loaded {
    pub(crate) org: membership::Loaded,
    pub(crate) team_id: Uuid,
    pub(crate) team_slug: String,
}

impl Loaded {
    /// Ask the engine one organization action with the caller's loaded facts:
    /// the same principal, teams and grants every other handler uses. A
    /// freshness denial keeps its re-authentication envelope.
    pub(crate) fn require(&self, state: &AppState, action: Action) -> Result<(), ApiError> {
        self.org.require(state, action)
    }
}

/// Load organization and team, then require `action` on the organization.
async fn load_with(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
    team_slug: &str,
    action: Action,
) -> Result<Loaded, ApiError> {
    let (account, auth_age) = authenticate(&state.pool, headers).await?;
    let org = membership::load(state, account, auth_age, org_slug).await?;
    // Authorization before the team lookup: a caller without the action learns
    // only what the organization decision says, never whether the team exists.
    org.require(state, action)?;
    load_team(state, org, team_slug).await
}

/// Resolve the team inside an already-authorized organization. There is no
/// separate team-level gate: every handler asks `require` for the exact action
/// its operation needs, so page and API share one rule.
pub(crate) async fn load_team(
    state: &AppState,
    org: membership::Loaded,
    team_slug: &str,
) -> Result<Loaded, ApiError> {
    if !valid_path_param(team_slug) {
        return Err(membership::invalid_param());
    }
    let team_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM teams WHERE organization_id = $1 AND slug = $2")
            .bind(org.org_id)
            .bind(team_slug)
            .fetch_optional(&state.pool)
            .await?;
    Ok(Loaded {
        team_id: team_id.ok_or_else(no_such_team)?,
        team_slug: team_slug.to_owned(),
        org,
    })
}

/// Load the organization alone (routes that have no team yet).
async fn load_org(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
    action: Action,
) -> Result<membership::Loaded, ApiError> {
    let (account, auth_age) = authenticate(&state.pool, headers).await?;
    let loaded = membership::load(state, account, auth_age, org_slug).await?;
    loaded.require(state, action)?;
    Ok(loaded)
}

async fn lock_team(conn: &mut PgConnection, team_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM teams WHERE id = $1 FOR NO KEY UPDATE")
        .bind(team_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Whether this active grant is the organization's last owner grant. Counts
/// exactly what the membership slice counts: active owner grants on the
/// organization, whether their subject is a member or a team.
async fn last_owner_grant(
    conn: &mut PgConnection,
    org_id: Uuid,
    grant_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants \
         WHERE scope_kind = 'organization' AND scope_id = $1 AND role = 'owner' \
         AND repository_id IS NULL AND revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > clock_timestamp())",
    )
    .bind(org_id)
    .fetch_one(&mut *conn)
    .await?;
    let owner: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM grants WHERE id = $1 AND role = 'owner' \
         AND scope_kind = 'organization' AND repository_id IS NULL AND revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > clock_timestamp()))",
    )
    .bind(grant_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(owner && owners <= 1)
}

/// Whether `team_id` holds an active organization owner grant.
async fn team_holds_owner(conn: &mut PgConnection, team_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM grants WHERE subject_kind = 'team' AND subject_id = $1 \
         AND role = 'owner' AND scope_kind = 'organization' AND repository_id IS NULL \
         AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > clock_timestamp()))",
    )
    .bind(team_id)
    .fetch_one(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every team of the organization, in the same slug order the page lists.
pub(crate) async fn all_teams(state: &AppState, org_id: Uuid) -> Result<Vec<TeamJson>, ApiError> {
    Ok(sqlx::query_as::<_, TeamJson>(&format!(
        "SELECT {TEAM_COLUMNS} FROM teams t WHERE t.organization_id = $1 ORDER BY t.slug"
    ))
    .bind(org_id)
    .fetch_all(&state.pool)
    .await?)
}

/// One team by slug inside its organization.
pub(crate) async fn get(
    state: &AppState,
    org_id: Uuid,
    team_slug: &str,
) -> Result<Option<TeamJson>, ApiError> {
    Ok(sqlx::query_as::<_, TeamJson>(&format!(
        "SELECT {TEAM_COLUMNS} FROM teams t WHERE t.organization_id = $1 AND t.slug = $2"
    ))
    .bind(org_id)
    .bind(team_slug)
    .fetch_optional(&state.pool)
    .await?)
}

/// The team's members, by username.
pub(crate) async fn members(
    state: &AppState,
    team_id: Uuid,
) -> Result<Vec<membership::MemberJson>, ApiError> {
    Ok(sqlx::query_as::<_, membership::MemberJson>(
        "SELECT a.id AS account_id, a.username, m.role, \
         to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
         FROM team_members tm JOIN accounts a ON a.id = tm.account_id \
         JOIN memberships m ON m.organization_id = (SELECT organization_id FROM teams WHERE id = $1) \
           AND m.account_id = tm.account_id \
         WHERE tm.team_id = $1 ORDER BY a.username",
    )
    .bind(team_id)
    .fetch_all(&state.pool)
    .await?)
}

/// The team's active grants.
pub(crate) async fn grants(state: &AppState, team_id: Uuid) -> Result<Vec<GrantJson>, ApiError> {
    Ok(sqlx::query_as::<_, GrantJson>(&format!(
        "SELECT {GRANT_COLUMNS} FROM grants g LEFT JOIN repositories r ON r.id = g.repository_id \
         WHERE g.subject_kind = 'team' AND g.subject_id = $1 AND g.revoked_at IS NULL \
         AND (g.expires_at IS NULL OR g.expires_at > clock_timestamp()) \
         ORDER BY g.created_at, g.id"
    ))
    .bind(team_id)
    .fetch_all(&state.pool)
    .await?)
}

async fn list_teams(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<TeamJson>>, ApiError> {
    // The team list is organization administration data; owners alone read it.
    // It pages by slug, the order `all_teams` and therefore the page use, so a
    // page of the API and the page it feeds list teams the same way. `Page`'s
    // `after` is a uuid everywhere, so the last slug travels as its text form.
    let org = load_org(&state, &headers, &org_slug, Action::TeamCreate).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(
        sqlx::query_as::<_, TeamJson>(&format!(
            "SELECT {TEAM_COLUMNS} FROM teams t WHERE t.organization_id = $1 \
             AND ($2::text IS NULL OR t.slug > $2) ORDER BY t.slug LIMIT $3"
        ))
        .bind(org.org_id)
        .bind(page.after.map(|id| id.to_string()))
        .bind(limit)
        .fetch_all(&state.pool)
        .await?,
    ))
}

async fn list_team_members(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug)): Path<(String, String)>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<membership::MemberJson>>, ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamUpdate).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(
        sqlx::query_as::<_, membership::MemberJson>(
            "SELECT a.id AS account_id, a.username, m.role, \
             to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
             FROM team_members tm JOIN accounts a ON a.id = tm.account_id \
             JOIN memberships m ON m.organization_id = $2 AND m.account_id = tm.account_id \
             WHERE tm.team_id = $1 AND ($3::uuid IS NULL OR a.id > $3) \
             ORDER BY a.id LIMIT $4",
        )
        .bind(loaded.team_id)
        .bind(loaded.org.org_id)
        .bind(page.after)
        .bind(limit)
        .fetch_all(&state.pool)
        .await?,
    ))
}

async fn list_team_grants(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug)): Path<(String, String)>,
) -> Result<Json<Vec<GrantJson>>, ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamUpdate).await?;
    Ok(Json(grants(&state, loaded.team_id).await?))
}

// ---------------------------------------------------------------------------
// Team lifecycle
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTeam {
    pub(crate) slug: String,
    pub(crate) name: String,
}

/// Create a team with `team.create`. A second team with the same slug in the
/// organization is a conflict (the unique index answers, through
/// `ApiError`'s unique-violation mapping).
pub(crate) async fn create(
    state: &AppState,
    org: &membership::Loaded,
    body: &[u8],
    key: Option<&str>,
) -> Result<TeamJson, ApiError> {
    org.require(state, Action::TeamCreate)?;
    let input: NewTeam = serde_json::from_slice(body).map_err(|_| invalid())?;
    // Validate only after authorization, so an unauthorized caller cannot turn
    // a bad request into a 400 that reveals the organization exists.
    if !valid_team_slug(&input.slug) || !valid_team_name(&input.name) {
        return Err(invalid());
    }
    let name = input.name.trim().to_owned();
    let path = format!("/api/v1/organizations/{}/teams", org.slug);
    let (_, value) = idempotency::run(
        &state.pool,
        org.account.id,
        "POST",
        &path,
        body,
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamCreate)?;
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO teams (id, organization_id, slug, name) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(org.org_id)
            .bind(&input.slug)
            .bind(&name)
            .execute(&mut *tx)
            .await?;
            event_with(
                &mut tx,
                Some(org.org_id),
                org.account.id,
                "organization.team_created",
                serde_json::json!({"team_id": id, "slug": input.slug, "name": name}),
            )
            .await?;
            let created = sqlx::query_as::<_, TeamJson>(&format!(
                "SELECT {TEAM_COLUMNS} FROM teams t WHERE t.id = $1"
            ))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&created).unwrap();
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

async fn create_team(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<TeamJson>), ApiError> {
    let org = load_org(&state, &headers, &org_slug, Action::TeamCreate).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let team = create(&state, &org, &body, key).await?;
    Ok((StatusCode::CREATED, Json(team)))
}

/// Delete a team with `team.delete`. Its grants are revoked (never deleted) and
/// its team memberships are removed in the same transaction, so nobody admits
/// through it on the next request. A team whose only active grant is the
/// organization's last owner grant cannot be deleted: the organization would be
/// left with no owner, and every team member of it holds the owner actions the
/// rule protects.
pub(crate) async fn delete_team_inner(
    state: &AppState,
    loaded: &Loaded,
    key: Option<&str>,
) -> Result<(), ApiError> {
    loaded.require(state, Action::TeamDelete)?;
    let org = &loaded.org;
    let path = format!(
        "/api/v1/organizations/{}/teams/{}",
        org.slug, loaded.team_slug
    );
    let (_, _) = idempotency::run(
        &state.pool,
        org.account.id,
        "DELETE",
        &path,
        b"",
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamDelete)?;
            lock_team(&mut tx, loaded.team_id).await?;
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams WHERE id = $1)")
                    .bind(loaded.team_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !exists {
                return Err(no_such_team());
            }
            if team_holds_owner(&mut tx, loaded.team_id).await? {
                for grant in sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM grants WHERE subject_kind = 'team' AND subject_id = $1 \
                     AND role = 'owner' AND scope_kind = 'organization' AND repository_id IS NULL \
                     AND revoked_at IS NULL FOR UPDATE",
                )
                .bind(loaded.team_id)
                .fetch_all(&mut *tx)
                .await?
                {
                    if last_owner_grant(&mut tx, org.org_id, grant).await? {
                        return Err(membership::last_owner());
                    }
                }
            }
            revoke_team_grants(&mut tx, loaded.team_id, org.account.id).await?;
            // Team memberships cascade with the team row; the delete is
            // explicit so the trigger records them before the team is gone.
            sqlx::query("DELETE FROM team_members WHERE team_id = $1")
                .bind(loaded.team_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM teams WHERE id = $1")
                .bind(loaded.team_id)
                .execute(&mut *tx)
                .await?;
            event_with(
                &mut tx,
                Some(org.org_id),
                org.account.id,
                "organization.team_deleted",
                serde_json::json!({"team_id": loaded.team_id, "slug": loaded.team_slug}),
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
    Ok(())
}

async fn delete_team(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamDelete).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    delete_team_inner(&state, &loaded, key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke every active grant the team holds, never deleting a row.
async fn revoke_team_grants(
    conn: &mut PgConnection,
    team_id: Uuid,
    actor: Uuid,
) -> Result<u64, sqlx::Error> {
    let revoked = sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE subject_kind = 'team' AND subject_id = $2 AND revoked_at IS NULL",
    )
    .bind(actor)
    .bind(team_id)
    .execute(&mut *conn)
    .await?;
    Ok(revoked.rows_affected())
}

// ---------------------------------------------------------------------------
// Team membership
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewMember {
    pub(crate) username: String,
}

/// Adding an organization member to a team needs `team.update`, whoever is
/// added: a team membership lets the account exercise every grant the team
/// holds, so adding the caller themselves is an update to the team like any
/// other.
async fn add_team_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<membership::MemberJson>), ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamUpdate).await?;
    let input: NewMember = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let member = add_member(&state, &loaded, &input.username, &body, key).await?;
    Ok((StatusCode::CREATED, Json(member)))
}

/// Add an organization member to a team with `team.update`, the same action for
/// every caller including one adding themselves. The team membership trigger
/// refuses an account that is not a member of the organization; this checks it
/// first so the refusal is a 404, not a 503.
pub(crate) async fn add_member(
    state: &AppState,
    loaded: &Loaded,
    username: &str,
    body: &[u8],
    key: Option<&str>,
) -> Result<membership::MemberJson, ApiError> {
    loaded.require(state, Action::TeamUpdate)?;
    if !valid_username(username) {
        return Err(invalid());
    }
    let org = &loaded.org;
    let path = format!(
        "/api/v1/organizations/{}/teams/{}/members",
        org.slug, loaded.team_slug
    );
    let (_, value) = idempotency::run(
        &state.pool,
        org.account.id,
        "POST",
        &path,
        body,
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamUpdate)?;
            lock_team(&mut tx, loaded.team_id).await?;
            let account_id: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
                    .bind(username)
                    .fetch_optional(&mut *tx)
                    .await?;
            let account_id = account_id.ok_or_else(no_such_member)?;
            let is_member: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
            )
            .bind(org.org_id)
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
            if !is_member {
                return Err(no_such_member());
            }
            let inserted = sqlx::query(
                "INSERT INTO team_members (team_id, account_id) VALUES ($1, $2) \
                 ON CONFLICT (team_id, account_id) DO NOTHING",
            )
            .bind(loaded.team_id)
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
            // Adding a member who is already one changes nothing and is
            // audited as nothing: the retry reads the current row.
            if inserted.rows_affected() == 1 {
                event_with(
                    &mut tx,
                    Some(org.org_id),
                    org.account.id,
                    "organization.team_member_added",
                    serde_json::json!({
                        "team_id": loaded.team_id,
                        "slug": loaded.team_slug,
                        "account_id": account_id,
                        "username": username,
                    }),
                )
                .await?;
            }
            let member = sqlx::query_as::<_, membership::MemberJson>(
                "SELECT a.id AS account_id, a.username, m.role, \
                 to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
                 FROM team_members tm JOIN accounts a ON a.id = tm.account_id \
                 JOIN memberships m ON m.organization_id = $2 AND m.account_id = tm.account_id \
                 WHERE tm.team_id = $1 AND tm.account_id = $3",
            )
            .bind(loaded.team_id)
            .bind(org.org_id)
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(no_such_member)?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&member).unwrap();
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

async fn remove_team_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug, username)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamUpdate).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    remove_member(&state, &loaded, &username, key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove one account from a team with `team.update`. It applies the one
/// reachable-owner rule the membership slice applies (`remove`, `set_role`):
/// refuse when the removal would leave no owner grant a live member can still
/// exercise. Earlier this handler asked only whether the team held an owner
/// grant, which missed the case where the *last member* of a team that is *not*
/// the only owner grant is still the last reachable route to an owner. The count
/// runs inside the transaction that holds `lock_organization`, so two
/// concurrent removals cannot both pass it.
pub(crate) async fn remove_member(
    state: &AppState,
    loaded: &Loaded,
    username: &str,
    key: Option<&str>,
) -> Result<(), ApiError> {
    loaded.require(state, Action::TeamUpdate)?;
    if !valid_username(username) {
        return Err(invalid());
    }
    let org = &loaded.org;
    let path = format!(
        "/api/v1/organizations/{}/teams/{}/members/{username}",
        org.slug, loaded.team_slug
    );
    let (_, _) = idempotency::run(
        &state.pool,
        org.account.id,
        "DELETE",
        &path,
        b"",
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamUpdate)?;
            lock_team(&mut tx, loaded.team_id).await?;
            let account_id: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
                    .bind(username)
                    .fetch_optional(&mut *tx)
                    .await?;
            let account_id = account_id.ok_or_else(no_such_member)?;
            let deleted =
                sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND account_id = $2")
                    .bind(loaded.team_id)
                    .bind(account_id)
                    .execute(&mut *tx)
                    .await?;
            if deleted.rows_affected() == 0 {
                return Err(no_such_member());
            }
            // The same reachable-owner rule the membership slice uses: a
            // team grant is an owner only while some member can exercise
            // it, so this refuses exactly when the removal would leave
            // zero reachable owners (the removed account is excluded).
            if membership::reachable_owner_count(&mut tx, org.org_id, account_id).await? == 0 {
                return Err(membership::last_owner());
            }
            event_with(
                &mut tx,
                Some(org.org_id),
                org.account.id,
                "organization.team_member_removed",
                serde_json::json!({
                    "team_id": loaded.team_id,
                    "slug": loaded.team_slug,
                    "account_id": account_id,
                    "username": username,
                }),
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
    Ok(())
}

// ---------------------------------------------------------------------------
// Team grants
// ---------------------------------------------------------------------------

/// A team grant request: a repository by name and a repository role, or
/// `on_organization` for an organization-scope grant (the only place an
/// organization `owner` grant to a team is created).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewGrant {
    pub(crate) repository: Option<String>,
    pub(crate) role: String,
    /// An organization-scope grant: `role` is a membership role.
    #[serde(default)]
    pub(crate) on_organization: bool,
}

async fn grant_team(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<GrantJson>), ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamUpdate).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let grant = add_grant(&state, &loaded, &body, key).await?;
    Ok((StatusCode::CREATED, Json(grant)))
}

/// Grant the team a repository role (or an organization role with
/// `on_organization`) with `team.update`: adding a grant is a change to the
/// team's access, not a deletion. `team.delete` stays on revoking a grant and
/// deleting the team. The grant trigger checks that the repository and the team
/// both belong to the organization, and a grant is never edited: the same grant
/// again is a no-op that returns the active row.
pub(crate) async fn add_grant(
    state: &AppState,
    loaded: &Loaded,
    body: &[u8],
    key: Option<&str>,
) -> Result<GrantJson, ApiError> {
    loaded.require(state, Action::TeamUpdate)?;
    let input: NewGrant = serde_json::from_slice(body).map_err(|_| invalid())?;
    let valid_role = if input.on_organization {
        input.repository.is_none() && membership::ROLES.contains(&input.role.as_str())
    } else {
        input.repository.is_some() && REPOSITORY_ROLES.contains(&input.role.as_str())
    };
    if !valid_role {
        return Err(invalid());
    }
    let org = &loaded.org;
    let path = format!(
        "/api/v1/organizations/{}/teams/{}/grants",
        org.slug, loaded.team_slug
    );
    let (_, value) = idempotency::run(
        &state.pool,
        org.account.id,
        "POST",
        &path,
        body,
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamUpdate)?;
            lock_team(&mut tx, loaded.team_id).await?;
            let repository_id: Option<Uuid> = match input.repository.as_deref() {
                None => None,
                Some(name) => {
                    if !valid_path_param(name) {
                        return Err(invalid());
                    }
                    sqlx::query_scalar(
                        "SELECT id FROM repositories WHERE organization_id = $1 AND name = $2 \
                         AND status <> 'deleted'",
                    )
                    .bind(org.org_id)
                    .bind(name)
                    .fetch_optional(&mut *tx)
                    .await?
                    .map(Some)
                    .ok_or_else(not_found)?
                }
            };
            let existing: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM grants WHERE subject_kind = 'team' AND subject_id = $1 \
                 AND scope_id = $2 AND repository_id IS NOT DISTINCT FROM $3 AND role = $4 \
                 AND revoked_at IS NULL",
            )
            .bind(loaded.team_id)
            .bind(org.org_id)
            .bind(repository_id)
            .bind(&input.role)
            .fetch_optional(&mut *tx)
            .await?;
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, \
                         repository_id, granted_by) VALUES ($1, $2, 'team', $3, \
                         CASE WHEN $4::uuid IS NULL THEN 'organization' ELSE 'repository' END, \
                         $5, $4, $6)",
                    )
                    .bind(id)
                    .bind(&input.role)
                    .bind(loaded.team_id)
                    .bind(repository_id)
                    .bind(org.org_id)
                    .bind(org.account.id)
                    .execute(&mut *tx)
                    .await?;
                    event_with(
                        &mut tx,
                        Some(org.org_id),
                        org.account.id,
                        "organization.team_grant_added",
                        serde_json::json!({
                            "team_id": loaded.team_id,
                            "slug": loaded.team_slug,
                            "grant_id": id,
                            "role": input.role,
                            "repository_id": repository_id,
                        }),
                    )
                    .await?;
                    id
                }
            };
            let grant = sqlx::query_as::<_, GrantJson>(&format!(
                "SELECT {GRANT_COLUMNS} FROM grants g LEFT JOIN repositories r ON r.id = g.repository_id \
                 WHERE g.id = $1"
            ))
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let status = StatusCode::CREATED;
            let body = serde_json::to_value(&grant).unwrap();
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

async fn revoke_team_grant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, team_slug, grant)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let loaded = load_with(&state, &headers, &org_slug, &team_slug, Action::TeamDelete).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let grant_id = Uuid::parse_str(&grant).map_err(|_| invalid())?;
    revoke_grant(&state, &loaded, grant_id, key).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke one of the team's grants with `team.delete`. The row is stamped, never
/// deleted. Revoking the organization's last owner grant, even a team's, is
/// refused exactly as demoting or removing the last owner is.
pub(crate) async fn revoke_grant(
    state: &AppState,
    loaded: &Loaded,
    grant_id: Uuid,
    key: Option<&str>,
) -> Result<(), ApiError> {
    loaded.require(state, Action::TeamDelete)?;
    let org = &loaded.org;
    let path = format!(
        "/api/v1/organizations/{}/teams/{}/grants/{grant_id}",
        org.slug, loaded.team_slug
    );
    let (_, _) = idempotency::run(
        &state.pool,
        org.account.id,
        "DELETE",
        &path,
        b"",
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, org.org_id).await?;
            let org = membership::reauthorize(&mut tx, org).await?;
            org.require(state, Action::TeamDelete)?;
            lock_team(&mut tx, loaded.team_id).await?;
            let active: Option<(String, Option<Uuid>)> = sqlx::query_as(
                "SELECT role, repository_id FROM grants WHERE id = $1 AND subject_kind = 'team' \
                 AND subject_id = $2 AND revoked_at IS NULL FOR UPDATE",
            )
            .bind(grant_id)
            .bind(loaded.team_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((role, repository_id)) = active else {
                return Err(no_such_grant());
            };
            if repository_id.is_none()
                && role == "owner"
                && last_owner_grant(&mut tx, org.org_id, grant_id).await?
            {
                return Err(membership::last_owner());
            }
            sqlx::query(
                "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $2 WHERE id = $1",
            )
            .bind(grant_id)
            .bind(org.account.id)
            .execute(&mut *tx)
            .await?;
            event_with(
                &mut tx,
                Some(org.org_id),
                org.account.id,
                "organization.team_grant_revoked",
                serde_json::json!({
                    "team_id": loaded.team_id,
                    "slug": loaded.team_slug,
                    "grant_id": grant_id,
                    "role": role,
                    "repository_id": repository_id,
                }),
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
    Ok(())
}
