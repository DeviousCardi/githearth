//! Organization membership administration: list members, add a member by
//! username, change a member's role, and remove a member (or leave).
//!
//! Every decision goes through the policy engine's existing
//! `organization.manage_members` action, which the `owner` role template
//! permits and no other built-in role does (ADR 0002). A caller who may read
//! the organization but not administer it gets 403; anyone else gets the same
//! 404 the other organization endpoints return, so a private organization is
//! not revealed.
//!
//! A membership holds exactly one organization role (`owner` or `member`, the
//! two values the schema, the role templates and the membership table
//! understand), mirrored by one account-scoped organization grant, which is
//! what authorization actually reads. Both rows change together, in the same
//! transaction as the audit and outbox event, like every other mutation.
//!
//! The last-owner rule holds under concurrency: every membership mutation locks
//! the organization row first, so two requests that would each remove one of the
//! last two owners serialize, and the second sees the committed count.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{patch, post},
};
use githearth_authz::{Action, Resource};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use uuid::Uuid;

use crate::{
    Account, ApiError, AppState, Page, authenticate, event_with, facts, idempotency, invalid,
    valid_path_param, valid_username,
};

/// The organization roles a membership may hold: the values the membership
/// table accepts and the engine's role templates understand.
pub(crate) const ROLES: [&str; 2] = ["owner", "member"];

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/members",
            post(add_member).get(list_members),
        )
        .route(
            "/api/v1/organizations/{org}/members/{username}",
            patch(change_role).delete(remove_member),
        )
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}

fn not_permitted() -> ApiError {
    ApiError(StatusCode::FORBIDDEN, "not permitted")
}

/// The last owner cannot be removed, leave or be demoted.
fn last_owner() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "the organization must keep at least one owner",
    )
}

fn already_member() -> ApiError {
    ApiError(StatusCode::CONFLICT, "already a member")
}

fn no_such_member() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "member not found")
}

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct MemberJson {
    pub(crate) account_id: Uuid,
    pub(crate) username: String,
    pub(crate) role: String,
    pub(crate) created_at: String,
}

const MEMBER_COLUMNS: &str = "a.id AS account_id, a.username, m.role, \
 to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at";

/// The organization a request addresses, with the caller's authorization facts
/// loaded for it. Pages use the same value to tell a masked 404 from a 403.
pub(crate) struct Loaded {
    pub(crate) org_id: Uuid,
    pub(crate) slug: String,
    pub(crate) account: Account,
    pub(crate) auth_age: u64,
    pub(crate) facts: facts::Facts,
}

impl Loaded {
    /// Whether the caller may administer membership: the owner-only, sensitive
    /// `organization.manage_members` action.
    pub(crate) fn may_manage(&self, state: &AppState) -> bool {
        state
            .authz
            .authorize(&self.facts.request(
                Action::OrganizationManageMembers,
                Resource::Organization,
                Some(Duration::from_secs(self.auth_age)),
            ))
            .is_allowed()
    }

    /// Whether the caller may read the organization at all.
    fn may_read(&self, state: &AppState) -> bool {
        state
            .authz
            .authorize(&self.facts.request(
                Action::OrganizationRead,
                Resource::Organization,
                Some(Duration::from_secs(self.auth_age)),
            ))
            .is_allowed()
    }

    /// Whether the caller may act on `username`: a membership administrator,
    /// or themself where leaving is allowed. This is the authorization decision
    /// `require_target` makes, without the account lookup.
    pub(crate) fn may_change(&self, state: &AppState, username: &str, allow_self: bool) -> bool {
        self.may_manage(state) || (allow_self && username == self.account.username)
    }

    /// The masked refusal: a reader of the organization learns it is forbidden;
    /// anyone else sees the same 404 a missing organization gives.
    pub(crate) fn refuse(&self, state: &AppState) -> ApiError {
        if self.may_read(state) {
            not_permitted()
        } else {
            not_found()
        }
    }
}

/// Resolve the organization by slug and load the caller's authorization facts.
pub(crate) async fn load(
    state: &AppState,
    account: Account,
    auth_age: u64,
    org_slug: &str,
) -> Result<Loaded, ApiError> {
    if !valid_path_param(org_slug) {
        return Err(invalid());
    }
    let mut conn = state.pool.acquire().await?;
    let org: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await?;
    let org_id = org.ok_or_else(not_found)?.0;
    let Some(facts) = facts::load(&mut conn, account.id, org_id).await? else {
        return Err(not_found());
    };
    Ok(Loaded {
        org_id,
        slug: org_slug.to_owned(),
        account,
        auth_age,
        facts,
    })
}

/// Load the organization and require `organization.manage_members`.
async fn load_manage(
    state: &AppState,
    headers: &HeaderMap,
    org_slug: &str,
) -> Result<Loaded, ApiError> {
    let (account, auth_age) = authenticate(&state.pool, headers).await?;
    let loaded = load(state, account, auth_age, org_slug).await?;
    if !loaded.may_manage(state) {
        return Err(loaded.refuse(state));
    }
    Ok(loaded)
}

/// Lock the organization for the rest of the transaction. Every membership
/// mutation takes this lock first, which serializes role changes, removals and
/// leaves within one organization so the last-owner check cannot race.
async fn lock_organization(conn: &mut PgConnection, org_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM organizations WHERE id = $1 FOR NO KEY UPDATE")
        .bind(org_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Re-read the caller's authorization facts inside the mutation transaction,
/// after the organization lock. The facts `load` captured before the lock may
/// be stale: an owner demoted or removed while this request waited must lose
/// the ability to finish the mutation it already passed the outer check for.
async fn reauthorize(conn: &mut PgConnection, loaded: &Loaded) -> Result<Loaded, ApiError> {
    let Some(facts) = facts::load_in_transaction(conn, loaded.account.id, loaded.org_id).await?
    else {
        return Err(not_found());
    };
    Ok(Loaded {
        org_id: loaded.org_id,
        slug: loaded.slug.clone(),
        account: loaded.account.clone(),
        auth_age: loaded.auth_age,
        facts,
    })
}

// Test-only mutation barrier. The concurrency tests must know that both racing
// requests are inside the mutation before either commits, which a barrier on
// request start cannot show. `pause_mutation` runs in the transaction after the
// caller has been authorized and before the organization row is locked or
// anything is written; it counts the request and, when a pause is configured,
// sleeps so both requests are provably in flight together.
//
// Production configures nothing: the per-organization pause map is empty and
// the environment variable is unset, so the only cost is one map lookup and one
// `var` read per mutation. Integration tests cannot set an environment variable
// themselves (this workspace forbids `unsafe`, and `set_var` is unsafe in the
// 2024 edition), so they install the pause through the safe hook below; the
// environment variable exists for running the same check from outside.
static TEST_PAUSES: LazyLock<Mutex<HashMap<Uuid, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TEST_ENTRIES: LazyLock<Mutex<HashMap<Uuid, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Test hook: pause every membership mutation in `org_id` for `millis`, so two
/// racing requests overlap inside the mutation. Not called in production.
#[doc(hidden)]
pub fn test_pause_mutations(org_id: Uuid, millis: u64) {
    TEST_PAUSES.lock().unwrap().insert(org_id, millis);
}

/// Test hook: how many mutations have entered the pause for `org_id`.
#[doc(hidden)]
pub fn test_mutation_entries(org_id: Uuid) -> u64 {
    TEST_ENTRIES
        .lock()
        .unwrap()
        .get(&org_id)
        .copied()
        .unwrap_or(0)
}

async fn pause_mutation(org_id: Uuid) {
    let configured = TEST_PAUSES
        .lock()
        .unwrap()
        .get(&org_id)
        .copied()
        .unwrap_or(0);
    let millis = if configured > 0 {
        configured
    } else {
        // Read once per request: a production process pays one lookup with the
        // variable unset and never pauses. The variable is test-only.
        std::env::var("GITHEARTH_TEST_MEMBERSHIP_MUTATION_PAUSE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    if millis == 0 {
        return;
    }
    *TEST_ENTRIES.lock().unwrap().entry(org_id).or_insert(0) += 1;
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

async fn owner_count(conn: &mut PgConnection, org_id: Uuid) -> Result<i64, sqlx::Error> {
    // The authoritative owner set is the grants, not the membership cache:
    // owner grants on a team count, because members of the team hold the owner
    // actions the rule protects.
    sqlx::query_scalar(
        "SELECT count(*) FROM grants \
         WHERE scope_kind = 'organization' AND scope_id = $1 AND role = 'owner' \
         AND repository_id IS NULL AND revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > clock_timestamp())",
    )
    .bind(org_id)
    .fetch_one(conn)
    .await
}

async fn member_row(
    conn: &mut PgConnection,
    org_id: Uuid,
    account_id: Uuid,
) -> Result<Option<MemberJson>, sqlx::Error> {
    sqlx::query_as::<_, MemberJson>(&format!(
        "SELECT {MEMBER_COLUMNS} FROM memberships m JOIN accounts a ON a.id = m.account_id \
         WHERE m.organization_id = $1 AND m.account_id = $2"
    ))
    .bind(org_id)
    .bind(account_id)
    .fetch_optional(conn)
    .await
}

/// The current membership role, locked for the rest of the transaction.
async fn locked_role(
    conn: &mut PgConnection,
    org_id: Uuid,
    account_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT role FROM memberships WHERE organization_id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(org_id)
    .bind(account_id)
    .fetch_optional(conn)
    .await
}

/// Make `role` the account's only active organization grant in `org_id`.
async fn upsert_grant(
    conn: &mut PgConnection,
    org_id: Uuid,
    account_id: Uuid,
    role: &str,
    actor: Uuid,
) -> Result<(), sqlx::Error> {
    // Grants are never edited: a superseded organization grant is revoked and a
    // new one inserted, exactly as repository grants are. Only the membership
    // role grant is replaced: an independent organization-scope grant (for
    // example `write`) is not the membership row's to revoke and survives a
    // role change.
    sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE scope_kind = 'organization' AND scope_id = $2 AND subject_kind = 'account' \
         AND subject_id = $3 AND repository_id IS NULL AND revoked_at IS NULL \
         AND role IN ('owner', 'member') AND role <> $4",
    )
    .bind(actor)
    .bind(org_id)
    .bind(account_id)
    .bind(role)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by) \
         SELECT $1, $2, 'account', $3, 'organization', $4, $5 \
         WHERE NOT EXISTS (SELECT 1 FROM grants WHERE scope_kind = 'organization' \
           AND scope_id = $4 AND subject_kind = 'account' AND subject_id = $3 \
           AND repository_id IS NULL AND role = $2 \
           AND revoked_at IS NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(role)
    .bind(account_id)
    .bind(org_id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Revoke every active grant the account holds anywhere in the organization:
/// its organization role and any direct repository grant, so removal denies
/// admission on every path at once.
async fn revoke_grants(
    conn: &mut PgConnection,
    org_id: Uuid,
    account_id: Uuid,
    actor: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE scope_id = $2 AND subject_kind = 'account' AND subject_id = $3 \
         AND revoked_at IS NULL",
    )
    .bind(actor)
    .bind(org_id)
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn list_members(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<MemberJson>>, ApiError> {
    let loaded = load_manage(&state, &headers, &org_slug).await?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(list(&state, loaded.org_id, page.after, limit).await?))
}

/// One page of members in account-id order; `after` is the last id of the
/// previous page.
pub(crate) async fn list(
    state: &AppState,
    org_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<MemberJson>, ApiError> {
    Ok(sqlx::query_as::<_, MemberJson>(&format!(
        "SELECT {MEMBER_COLUMNS} FROM memberships m JOIN accounts a ON a.id = m.account_id \
         WHERE m.organization_id = $1 AND ($2::uuid IS NULL OR m.account_id > $2) \
         ORDER BY m.account_id LIMIT $3"
    ))
    .bind(org_id)
    .bind(after)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?)
}

/// Every member of the organization, for the members page: owners first, then
/// by username so the list is stable.
pub(crate) async fn all_members(
    state: &AppState,
    org_id: Uuid,
) -> Result<Vec<MemberJson>, ApiError> {
    Ok(sqlx::query_as::<_, MemberJson>(&format!(
        "SELECT {MEMBER_COLUMNS} FROM memberships m JOIN accounts a ON a.id = m.account_id \
         WHERE m.organization_id = $1 ORDER BY (m.role = 'member'), a.username"
    ))
    .bind(org_id)
    .fetch_all(&state.pool)
    .await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddMember {
    pub(crate) username: String,
    pub(crate) role: String,
}

/// Add a member, honoring the idempotency mechanism of every other mutation.
pub(crate) async fn add(
    state: &AppState,
    loaded: &Loaded,
    body: &[u8],
    key: Option<&str>,
) -> Result<MemberJson, ApiError> {
    if !loaded.may_manage(state) {
        return Err(loaded.refuse(state));
    }
    let input: AddMember = serde_json::from_slice(body).map_err(|_| invalid())?;
    if !valid_username(&input.username) || !ROLES.contains(&input.role.as_str()) {
        return Err(invalid());
    }
    let path = format!("/api/v1/organizations/{}/members", loaded.slug);
    let (_, value) = idempotency::run(
        &state.pool,
        loaded.account.id,
        "POST",
        &path,
        body,
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            pause_mutation(loaded.org_id).await;
            lock_organization(&mut tx, loaded.org_id).await?;
            // Re-authorize after the lock exactly as `set_role` and `remove`
            // do: an owner demoted or removed while this add waited must not
            // finish it with the authority it held before the lock.
            let loaded = reauthorize(&mut tx, loaded).await?;
            if !loaded.may_manage(state) {
                return Err(loaded.refuse(state));
            }
            let account_id: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
                    .bind(&input.username)
                    .fetch_optional(&mut *tx)
                    .await?;
            // The caller already holds `organization.manage_members`, so this
            // lookup does not conceal account existence: an owner gets 201 for
            // an existing account, 409 if it is already a member, and 404 for
            // an unknown username. Callers who may not administer membership
            // are refused before any account lookup (see `require_target`).
            let account_id = account_id.ok_or_else(no_such_member)?;
            let inserted = sqlx::query(
                "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, $3) \
                 ON CONFLICT (organization_id, account_id) DO NOTHING",
            )
            .bind(loaded.org_id)
            .bind(account_id)
            .bind(&input.role)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                return Err(already_member());
            }
            upsert_grant(
                &mut tx,
                loaded.org_id,
                account_id,
                &input.role,
                loaded.account.id,
            )
            .await?;
            event_with(
                &mut tx,
                Some(loaded.org_id),
                loaded.account.id,
                "organization.member_added",
                serde_json::json!({
                    "account_id": account_id,
                    "username": input.username,
                    "role": input.role,
                }),
            )
            .await?;
            let member = member_row(&mut tx, loaded.org_id, account_id)
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

async fn add_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<MemberJson>), ApiError> {
    let loaded = load_manage(&state, &headers, &org_slug).await?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let member = add(&state, &loaded, &body, key).await?;
    Ok((StatusCode::CREATED, Json(member)))
}

/// Resolve a target account for a membership change.
///
/// Removing one's own membership is leaving, which any member may do; every
/// other change needs `organization.manage_members`. Authorization is decided
/// before the account lookup: a caller who may neither administer the
/// organization nor act on themselves is refused without querying `accounts`,
/// so a plain member cannot use a 403/404 difference to enumerate accounts.
/// Only an administrator, who may see the member list anyway, can distinguish
/// an unknown username from an existing one.
async fn require_target(
    state: &AppState,
    conn: &mut PgConnection,
    loaded: &Loaded,
    username: &str,
    allow_self: bool,
) -> Result<Uuid, ApiError> {
    if !loaded.may_change(state, username, allow_self) {
        return Err(loaded.refuse(state));
    }
    // A NUL byte cannot be bound as PostgreSQL text and is not a valid path
    // parameter. Reject it before any SQL runs, but only after the
    // authorization decision above, so a refusal always wins over a 400.
    if !valid_path_param(username) {
        return Err(invalid());
    }
    let account_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
            .bind(username)
            .fetch_optional(&mut *conn)
            .await?;
    let account_id = account_id.ok_or_else(no_such_member)?;
    Ok(account_id)
}

/// Change a membership's role in one transaction with its grant, its audit
/// event and its outbox event.
pub(crate) async fn set_role(
    state: &AppState,
    loaded: &Loaded,
    username: &str,
    role: &str,
    key: Option<&str>,
) -> Result<MemberJson, ApiError> {
    let path = format!("/api/v1/organizations/{}/members/{username}", loaded.slug);
    // The request body is the new role, so a retry of the same flip reuses the
    // key while a different flip is a different request.
    let (_, value) = idempotency::run(
        &state.pool,
        loaded.account.id,
        "PATCH",
        &path,
        role.as_bytes(),
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            pause_mutation(loaded.org_id).await;
            lock_organization(&mut tx, loaded.org_id).await?;
            // Re-authorize the caller against the locked snapshot: a concurrent
            // demotion or removal that committed while this request waited must
            // not let it finish with the authority it had before the lock.
            let loaded = reauthorize(&mut tx, loaded).await?;
            let account_id = require_target(state, &mut tx, &loaded, username, false).await?;
            // Validate the role only after authorization, so an unauthorized
            // caller cannot turn a bad role into a 400 that renders the page.
            if !ROLES.contains(&role) {
                return Err(invalid());
            }
            let current = locked_role(&mut tx, loaded.org_id, account_id)
                .await?
                .ok_or_else(no_such_member)?;
            if current == role {
                // Nothing changes: no audit event and no grant rewrite.
                let member = member_row(&mut tx, loaded.org_id, account_id)
                    .await?
                    .ok_or_else(no_such_member)?;
                let (status, body) = (StatusCode::OK, serde_json::to_value(&member).unwrap());
                if !idem.store(&mut tx, status, &body).await? {
                    return Err(idempotency::claim_lost());
                }
                tx.commit().await?;
                return Ok((status, body));
            }
            let event_kind = if current == "owner" {
                if owner_count(&mut tx, loaded.org_id).await? <= 1 {
                    return Err(last_owner());
                }
                "organization.member_demoted"
            } else {
                "organization.member_promoted"
            };
            sqlx::query(
                "UPDATE memberships SET role = $1 WHERE organization_id = $2 AND account_id = $3",
            )
            .bind(role)
            .bind(loaded.org_id)
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
            upsert_grant(&mut tx, loaded.org_id, account_id, role, loaded.account.id).await?;
            event_with(
                &mut tx,
                Some(loaded.org_id),
                loaded.account.id,
                event_kind,
                serde_json::json!({
                    "account_id": account_id,
                    "username": username,
                    "role": role,
                    "previous_role": current,
                }),
            )
            .await?;
            let member = member_row(&mut tx, loaded.org_id, account_id)
                .await?
                .ok_or_else(no_such_member)?;
            let (status, body) = (StatusCode::OK, serde_json::to_value(&member).unwrap());
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeRole {
    pub(crate) role: String,
}

async fn change_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, username)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<MemberJson>, ApiError> {
    let loaded = load_manage(&state, &headers, &org_slug).await?;
    let input: ChangeRole = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let key = idempotency::extract_key(&headers).transpose()?;
    let member = set_role(&state, &loaded, &username, &input.role, key).await?;
    Ok(Json(member))
}

/// Remove a membership in one transaction with its grants, its team
/// memberships, its audit event and its outbox event. `allow_self` admits
/// leaving; the event kind records whether the member left or was removed.
pub(crate) async fn remove(
    state: &AppState,
    loaded: &Loaded,
    username: &str,
    key: Option<&str>,
    allow_self: bool,
) -> Result<(), ApiError> {
    let path = format!("/api/v1/organizations/{}/members/{username}", loaded.slug);
    let (_, _) = idempotency::run(
        &state.pool,
        loaded.account.id,
        "DELETE",
        &path,
        b"",
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            pause_mutation(loaded.org_id).await;
            lock_organization(&mut tx, loaded.org_id).await?;
            // Re-authorize after the lock: a caller demoted or removed by a
            // request that committed while this one waited is refused here
            // instead of completing the removal with stale authority.
            let loaded = reauthorize(&mut tx, loaded).await?;
            let account_id = require_target(state, &mut tx, &loaded, username, allow_self).await?;
            let current = locked_role(&mut tx, loaded.org_id, account_id)
                .await?
                .ok_or_else(no_such_member)?;
            if current == "owner" && owner_count(&mut tx, loaded.org_id).await? <= 1 {
                return Err(last_owner());
            }
            // Every path that admits the account reads the membership or a
            // grant, so revoking both takes effect as soon as this commits.
            revoke_grants(&mut tx, loaded.org_id, account_id, loaded.account.id).await?;
            sqlx::query(
                "DELETE FROM team_members WHERE account_id = $1 \
                 AND team_id IN (SELECT id FROM teams WHERE organization_id = $2)",
            )
            .bind(account_id)
            .bind(loaded.org_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM memberships WHERE organization_id = $1 AND account_id = $2")
                .bind(loaded.org_id)
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
            let event_kind = if account_id == loaded.account.id {
                "organization.member_left"
            } else {
                "organization.member_removed"
            };
            event_with(
                &mut tx,
                Some(loaded.org_id),
                loaded.account.id,
                event_kind,
                serde_json::json!({
                    "account_id": account_id,
                    "username": username,
                    "role": current,
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

async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, username)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    // Not `load_manage`: a plain member may remove their own membership (leave)
    // without holding the administrate permission. `remove` authorizes the
    // target: an administrator, or the caller themselves. Decide that before
    // the idempotency key is extracted, so a malformed or reused key cannot
    // distinguish an existing private organization from a missing one.
    let (account, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = load(&state, account, auth_age, &org_slug).await?;
    if !loaded.may_change(&state, &username, true) {
        return Err(loaded.refuse(&state));
    }
    let key = idempotency::extract_key(&headers).transpose()?;
    remove(&state, &loaded, &username, key, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a member from the members page: the form's fields become the same JSON
/// body the API takes, so validation, authorization, idempotency and events are
/// identical.
pub(crate) async fn add_from_page(
    state: &AppState,
    loaded: &Loaded,
    username: &str,
    role: &str,
    key: Option<&str>,
) -> Result<MemberJson, ApiError> {
    let body = serde_json::json!({"username": username, "role": role}).to_string();
    add(state, loaded, body.as_bytes(), key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_the_membership_roles_the_schema_accepts() {
        assert_eq!(ROLES, ["owner", "member"]);
        for role in githearth_authz::BuiltinRole::ALL {
            if role.organization_only() {
                assert!(ROLES.contains(&role.name()), "{}", role.name());
            }
        }
    }
}
