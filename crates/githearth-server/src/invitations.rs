//! Invitations with expiry (M4.1 acceptance case U06).
//!
//! An invitation names one organization, optionally one team inside it, the
//! invited email address, the organization role the accepted account receives
//! and — for a team invitation — the role the team carries on its repositories.
//! The token is shown once to the inviter and stored only as its SHA-256:
//! `invitations.token_hash` is the only place any trace of it survives, and no
//! row, idempotency record, log line or audit event carries the plaintext, which
//! leaves memory when the response is written. Creation does not run through the
//! idempotency store — that store keeps a handler's response body in
//! `operations.response_body` for 24 h, and this body carries the token — so a
//! repeated request makes a second, equally valid invitation (never a duplicate
//! membership or grant) and there is no stored response body in which the token
//! could survive.
//!
//! Managing invitations is the owner-only `invitation.*` group of ADR 0002;
//! acceptance is deliberately not a Cedar decision. The token is the
//! credential: a valid, unexpired, single-use token for the signed-in account's
//! verified email creates the membership and this invitation's team membership
//! in the same transaction as the account's owner or member grant, audits one
//! event and consumes the row.
//!
//! Refusal is uniform. "No such invitation", "wrong account" and "expired" (and
//! a revoked or already used one, and an account without a verified email) are
//! the same status, the same body and the same code path, so nothing about the
//! invitation — least of all the name, id, slug or member count of a private
//! organization — reaches an account holding a token that is not its own. The
//! assessment of the token reads only the invitation row.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    ApiError, AppState, Page, authenticate, event_with, idempotency, invalid, membership,
    valid_path_param,
};

/// An invitation expires after this many days unless revoked first. The
/// default is published here, next to the column that stores the computed
/// expiry, so a listing can show exactly what the row holds.
pub(crate) const DEFAULT_EXPIRY_DAYS: i64 = 7;
const MAX_EXPIRY_DAYS: i64 = 30;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/organizations/{org}/invitations",
            post(create_invitation).get(list_invitations),
        )
        .route(
            "/api/v1/organizations/{org}/invitations/{id}",
            axum::routing::delete(revoke_invitation),
        )
        .route("/api/v1/invitations/{token}", get(preview).post(accept))
}

/// The one refusal an acceptance can give, whatever the reason. It names no
/// organization and carries no field an inviter could have set.
fn refused() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "invitation not found")
}

fn no_such_invitation() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "invitation not found")
}

/// The invited address must be a plausible lower-case SMTP address. It is only
/// ever compared with the account's verified address; nothing is sent here.
/// The same rule is `accounts_email_format` in migration 0049, so a SQL insert
/// and this validator agree: exactly one `@`, a local part of 1–64 characters
/// that does not start with a dot, a domain with a dot, no leading or trailing
/// dot and no empty label, all printable ASCII.
pub(crate) fn valid_email(value: &str) -> bool {
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    value.len() <= 320
        && !value.is_empty()
        && !crate::has_nul(value)
        && !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && value.bytes().all(|b| (b' '..=b'~').contains(&b))
}

/// An invitation as the API lists it. It never carries the token or its hash.
#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct InvitationJson {
    pub(crate) id: Uuid,
    pub(crate) email: String,
    pub(crate) role: String,
    pub(crate) team: Option<String>,
    pub(crate) status: String,
    pub(crate) created_at: String,
    pub(crate) expires_at: String,
}

const INVITATION_COLUMNS: &str = "i.id, i.email, i.role, t.slug AS team, \
 CASE WHEN i.used_at IS NOT NULL THEN 'accepted' \
      WHEN i.revoked_at IS NOT NULL THEN 'revoked' \
      WHEN i.expires_at <= clock_timestamp() THEN 'expired' \
      ELSE 'pending' END AS status, \
 to_char(i.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 to_char(i.expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at";

/// The created invitation and its one-time token: the only response that ever
/// carries the plaintext.
#[derive(Serialize, Deserialize)]
pub(crate) struct CreatedInvitation {
    #[serde(flatten)]
    pub(crate) invitation: InvitationJson,
    pub(crate) token: String,
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A token is 64 lower-case hex characters, like a password-reset code.
fn valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Manage
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewInvitation {
    pub(crate) email: String,
    pub(crate) role: String,
    pub(crate) team: Option<String>,
    pub(crate) expires_in_days: Option<i64>,
}

/// Every invitation of the organization, newest first.
pub(crate) async fn list_in(
    state: &AppState,
    org_id: Uuid,
) -> Result<Vec<InvitationJson>, ApiError> {
    Ok(sqlx::query_as::<_, InvitationJson>(&format!(
        "SELECT {INVITATION_COLUMNS} FROM invitations i \
         LEFT JOIN teams t ON t.id = i.team_id \
         WHERE i.organization_id = $1 ORDER BY i.created_at DESC, i.id"
    ))
    .bind(org_id)
    .fetch_all(&state.pool)
    .await?)
}

async fn list_invitations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<InvitationJson>>, ApiError> {
    let (account, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = membership::load(&state, account, auth_age, &org_slug).await?;
    if !may_read(&state, &loaded) {
        return Err(loaded.refuse(&state));
    }
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    Ok(Json(
        sqlx::query_as::<_, InvitationJson>(&format!(
            "SELECT {INVITATION_COLUMNS} FROM invitations i \
             LEFT JOIN teams t ON t.id = i.team_id \
             WHERE i.organization_id = $1 AND ($2::uuid IS NULL OR i.id > $2) \
             ORDER BY i.id LIMIT $3"
        ))
        .bind(loaded.org_id)
        .bind(page.after)
        .bind(limit)
        .fetch_all(&state.pool)
        .await?,
    ))
}

fn may_read(state: &AppState, loaded: &membership::Loaded) -> bool {
    state
        .authz
        .authorize(&loaded.facts.on_organization(
            githearth_authz::Action::InvitationRead,
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed()
}

fn may_create(state: &AppState, loaded: &membership::Loaded) -> bool {
    state
        .authz
        .authorize(&loaded.facts.on_organization(
            githearth_authz::Action::InvitationCreate,
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed()
}

/// Create an invitation with `invitation.create` (owner, sensitive). The
/// response carries the token once; the row carries only its hash and the
/// audit event names the invitation, never the token.
///
/// This deliberately does not run through `idempotency::run`: the framework
/// stores the handler's response body in `operations.response_body` for 24 h,
/// and that body carries the token. An invitation is not a grant, so a retry
/// makes a second, equally valid invitation (listed next to the first) rather
/// than a duplicate membership, which is harmless; there is then no stored
/// response body in which the plaintext could survive a dump or the retention
/// window. The `invitation_created` audit event is written as on any create.
pub(crate) async fn create(
    state: &AppState,
    loaded: &membership::Loaded,
    body: &[u8],
) -> Result<CreatedInvitation, ApiError> {
    if !may_create(state, loaded) {
        return Err(loaded.refuse(state));
    }
    let input: NewInvitation = serde_json::from_slice(body).map_err(|_| invalid())?;
    let email = input.email.trim().to_ascii_lowercase();
    let days = input.expires_in_days.unwrap_or(DEFAULT_EXPIRY_DAYS);
    let valid = email.contains('@')
        && valid_email(&email)
        && membership::ROLES.contains(&input.role.as_str())
        && (1..=MAX_EXPIRY_DAYS).contains(&days);
    if !valid {
        return Err(invalid());
    }
    let mut tx = state.pool.begin().await?;
    membership::lock_organization(&mut tx, loaded.org_id).await?;
    let loaded = membership::reauthorize(&mut tx, loaded).await?;
    if !may_create(state, &loaded) {
        return Err(loaded.refuse(state));
    }
    let team_id: Option<Uuid> = match input.team.as_deref() {
        None => None,
        Some(slug) => {
            if !valid_path_param(slug) {
                return Err(invalid());
            }
            // An unknown team is a bad request, exactly as an unknown address
            // or role is: the inviter is an owner, so nothing is concealed
            // from them.
            sqlx::query_scalar("SELECT id FROM teams WHERE organization_id = $1 AND slug = $2")
                .bind(loaded.org_id)
                .bind(slug)
                .fetch_optional(&mut *tx)
                .await?
                .map(Some)
                .ok_or_else(invalid)?
        }
    };
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let token = hex::encode(bytes);
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO invitations (id, organization_id, team_id, email, role, token_hash, \
         invited_by, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, \
         clock_timestamp() + $8 * interval '1 day')",
    )
    .bind(id)
    .bind(loaded.org_id)
    .bind(team_id)
    .bind(&email)
    .bind(&input.role)
    .bind(token_hash(&token))
    .bind(loaded.account.id)
    .bind(days as f64)
    .execute(&mut *tx)
    .await?;
    event_with(
        &mut tx,
        Some(loaded.org_id),
        loaded.account.id,
        "organization.invitation_created",
        serde_json::json!({
            "invitation_id": id,
            "email": email,
            "role": input.role,
            "team_id": team_id,
            "expires_in_days": days,
        }),
    )
    .await?;
    let row = invitation_row(&mut tx, loaded.org_id, id)
        .await?
        .ok_or_else(no_such_invitation)?;
    tx.commit().await?;
    Ok(CreatedInvitation {
        invitation: row,
        token,
    })
}

async fn invitation_row(
    conn: &mut PgConnection,
    org_id: Uuid,
    id: Uuid,
) -> Result<Option<InvitationJson>, sqlx::Error> {
    sqlx::query_as::<_, InvitationJson>(&format!(
        "SELECT {INVITATION_COLUMNS} FROM invitations i \
         LEFT JOIN teams t ON t.id = i.team_id WHERE i.organization_id = $1 AND i.id = $2"
    ))
    .bind(org_id)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
}

async fn create_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<CreatedInvitation>), ApiError> {
    let (account, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = membership::load(&state, account, auth_age, &org_slug).await?;
    if !may_create(&state, &loaded) {
        return Err(loaded.refuse(&state));
    }
    // An `Idempotency-Key` is still validated when present, so a malformed one
    // is a 400 as on every other mutation, but creation is not replayed: see
    // `create`.
    idempotency::extract_key(&headers).transpose()?;
    let created = create(&state, &loaded, &body).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Revoke an invitation with `invitation.revoke` (owner, sensitive). Revocation
/// is idempotent: an already accepted invitation is refused, an already revoked
/// one is accepted as done.
pub(crate) async fn revoke(
    state: &AppState,
    loaded: &membership::Loaded,
    id: Uuid,
    key: Option<&str>,
) -> Result<(), ApiError> {
    if !may_revoke(state, loaded) {
        return Err(loaded.refuse(state));
    }
    let path = format!("/api/v1/organizations/{}/invitations/{id}", loaded.slug);
    let (_, _) = idempotency::run(
        &state.pool,
        loaded.account.id,
        "DELETE",
        &path,
        b"",
        key,
        |idem| async move {
            let mut tx = state.pool.begin().await?;
            membership::lock_organization(&mut tx, loaded.org_id).await?;
            let loaded = membership::reauthorize(&mut tx, loaded).await?;
            if !may_revoke(state, &loaded) {
                return Err(loaded.refuse(state));
            }
            let row: Option<(bool, bool)> = sqlx::query_as(
                "SELECT revoked_at IS NOT NULL, used_at IS NOT NULL FROM invitations \
                 WHERE id = $1 AND organization_id = $2 FOR UPDATE",
            )
            .bind(id)
            .bind(loaded.org_id)
            .fetch_optional(&mut *tx)
            .await?;
            match row {
                None => return Err(no_such_invitation()),
                Some((_, true)) => return Err(no_such_invitation()),
                Some((true, false)) => {}
                Some((false, false)) => {
                    sqlx::query(
                        "UPDATE invitations SET revoked_at = clock_timestamp(), revoked_by = $2 \
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(loaded.account.id)
                    .execute(&mut *tx)
                    .await?;
                    event_with(
                        &mut tx,
                        Some(loaded.org_id),
                        loaded.account.id,
                        "organization.invitation_revoked",
                        serde_json::json!({"invitation_id": id}),
                    )
                    .await?;
                }
            }
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

fn may_revoke(state: &AppState, loaded: &membership::Loaded) -> bool {
    state
        .authz
        .authorize(&loaded.facts.on_organization(
            githearth_authz::Action::InvitationRevoke,
            Some(Duration::from_secs(loaded.auth_age)),
        ))
        .is_allowed()
}

async fn revoke_invitation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((org_slug, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (account, auth_age) = authenticate(&state.pool, &headers).await?;
    let loaded = membership::load(&state, account, auth_age, &org_slug).await?;
    let id = Uuid::parse_str(&id).map_err(|_| invalid())?;
    let key = idempotency::extract_key(&headers).transpose()?;
    revoke(&state, &loaded, id, key).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Accept
// ---------------------------------------------------------------------------

/// What a holder of a live token is told: the invitations addressed to their
/// own verified address, and the organization each one joins. The preview
/// decides nothing; acceptance does, in one transaction.
#[derive(Serialize)]
pub(crate) struct Offer {
    pub(crate) invitations: Vec<OfferItem>,
}

#[derive(Serialize)]
pub(crate) struct OfferItem {
    pub(crate) organization: String,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) team: Option<String>,
    pub(crate) expires_at: String,
}

#[derive(Serialize)]
pub(crate) struct Accepted {
    pub(crate) organization_id: Uuid,
    pub(crate) role: String,
}

/// The account's verified email, or `None`: an invitation can only be accepted
/// by the identity it was addressed to, and an unverified address is not one.
/// A database error is not a refusal: it propagates as a 5xx rather than
/// masquerading as "no verified address".
async fn verified_email(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<String>, ApiError> {
    let email = sqlx::query_scalar::<_, String>(
        "SELECT email FROM accounts WHERE id = $1 AND email_verified_at IS NOT NULL",
    )
    .bind(account_id)
    .fetch_optional(conn)
    .await?;
    Ok(email.map(|email| email.trim().to_ascii_lowercase()))
}

/// The live invitations for the token's own address. A token that names another
/// address, or no invitation, yields nothing — the same empty answer.
async fn offers_for(
    conn: &mut PgConnection,
    email: &str,
    token: &str,
) -> Result<Vec<OfferItem>, sqlx::Error> {
    sqlx::query_as::<_, OfferRow>(
        "SELECT o.slug, o.name, i.role, t.slug AS team, \
         to_char(i.expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at \
         FROM invitations i JOIN organizations o ON o.id = i.organization_id \
         LEFT JOIN teams t ON t.id = i.team_id \
         WHERE i.token_hash = $1 AND i.email = $2 AND i.used_at IS NULL AND i.revoked_at IS NULL \
         AND i.expires_at > clock_timestamp() AND NOT o.suspended \
         ORDER BY i.created_at",
    )
    .bind(token_hash(token))
    .bind(email)
    .fetch_all(&mut *conn)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|row| OfferItem {
                organization: row.slug,
                name: row.name,
                role: row.role,
                team: row.team,
                expires_at: row.expires_at,
            })
            .collect()
    })
}

#[derive(sqlx::FromRow)]
struct OfferRow {
    slug: String,
    name: String,
    role: String,
    team: Option<String>,
    expires_at: String,
}

async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Json<Offer>, ApiError> {
    let (account, _) = authenticate(&state.pool, &headers).await?;
    Ok(Json(offer(&state, account.id, &token).await?))
}

/// The preview an accepting holder sees, shared by the JSON route and the
/// accept page. A malformed token is the same refusal as an unknown one: it
/// costs no index lookup and reveals nothing.
pub(crate) async fn offer(
    state: &AppState,
    account_id: Uuid,
    token: &str,
) -> Result<Offer, ApiError> {
    if !valid_token(token) {
        return Err(refused());
    }
    let mut conn = state.pool.acquire().await?;
    let Some(email) = verified_email(&mut conn, account_id).await? else {
        return Err(refused());
    };
    let invitations = offers_for(&mut conn, &email, token)
        .await
        .map_err(ApiError::from)?;
    if invitations.is_empty() {
        return Err(refused());
    }
    Ok(Offer { invitations })
}

/// Accept the invitation a token names, as the account whose verified email it
/// was addressed to. This is not a Cedar decision (ADR 0002): the token is the
/// credential. Everything the acceptance writes — membership row, organization
/// grant, team membership, team grants, audit event and the consumed invitation
/// — commits together or not at all; a refusal anywhere rolls back and answers
/// the one refusal. Accepting again is refused, as an expired one is.
pub(crate) async fn accept_invitation(
    state: &AppState,
    account_id: Uuid,
    token: &str,
) -> Result<Accepted, ApiError> {
    if !valid_token(token) {
        return Err(refused());
    }
    let hash = token_hash(token);
    let mut tx = state.pool.begin().await?;
    let result = accept_in(&mut tx, account_id, &hash).await;
    match result {
        Ok(accepted) => {
            tx.commit().await?;
            Ok(accepted)
        }
        Err(error) => {
            // No join may half-commit; rollback is best effort because the
            // connection is recycled either way.
            let _ = tx.rollback().await;
            Err(error)
        }
    }
}

async fn accept_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: Uuid,
    hash: &[u8],
) -> Result<Accepted, ApiError> {
    let Some(email) = verified_email(tx, account_id).await? else {
        return Err(refused());
    };
    // The organization lock first, in the same order membership mutations take
    // it, so an acceptance and a removal of the same member cannot interleave.
    let Some(org_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT organization_id FROM invitations \
         WHERE token_hash = $1 AND email = $2 AND used_at IS NULL AND revoked_at IS NULL \
         AND expires_at > clock_timestamp()",
    )
    .bind(hash)
    .bind(&email)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Err(refused());
    };
    membership::lock_organization(tx, org_id).await?;
    // Re-read the whole invitation under the lock: a concurrent acceptance of
    // the same token or a revocation that committed while this waited wins, and
    // this one refuses with the same body. The row is locked for update.
    let locked: Option<(Uuid, Option<Uuid>, String, Uuid)> = sqlx::query_as(
        "SELECT id, team_id, role, invited_by FROM invitations WHERE token_hash = $1 \
         AND email = $2 AND used_at IS NULL AND revoked_at IS NULL \
         AND expires_at > clock_timestamp() FOR UPDATE",
    )
    .bind(hash)
    .bind(&email)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, team_id, role, invited_by)) = locked else {
        return Err(refused());
    };
    let suspended: bool = sqlx::query_scalar("SELECT suspended FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&mut **tx)
        .await?;
    if suspended {
        return Err(refused());
    }

    // Membership and its grant, then the team and its grants: the same rows a
    // manual membership and a team grant write.
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT role FROM memberships WHERE organization_id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(org_id)
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await?;
    match existing.as_deref() {
        Some(current) if current == role => {}
        Some("member") if role == "owner" => {
            // Accepting an owner invitation promotes; accepting a member
            // invitation never demotes an existing owner.
            sqlx::query(
                "UPDATE memberships SET role = 'owner' WHERE organization_id = $1 AND account_id = $2",
            )
            .bind(org_id)
            .bind(account_id)
            .execute(&mut **tx)
            .await?;
            upsert_role_grant(tx, org_id, account_id, "owner", invited_by).await?;
        }
        Some(_) => return Err(refused()),
        None => {
            sqlx::query(
                "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, $3)",
            )
            .bind(org_id)
            .bind(account_id)
            .bind(&role)
            .execute(&mut **tx)
            .await?;
            upsert_role_grant(tx, org_id, account_id, &role, invited_by).await?;
        }
    }
    if let Some(team_id) = team_id {
        sqlx::query(
            "INSERT INTO team_members (team_id, account_id) VALUES ($1, $2) \
             ON CONFLICT (team_id, account_id) DO NOTHING",
        )
        .bind(team_id)
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
        grant_team_repositories(tx, org_id, team_id, invited_by).await?;
    }
    let consumed = sqlx::query(
        "UPDATE invitations SET used_at = clock_timestamp(), used_by = $2 WHERE id = $1 \
         AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    if consumed.rows_affected() != 1 {
        return Err(refused());
    }
    event_with(
        tx,
        Some(org_id),
        account_id,
        "organization.invitation_accepted",
        serde_json::json!({
            "invitation_id": id,
            "role": role,
            "team_id": team_id,
        }),
    )
    .await?;
    Ok(Accepted {
        organization_id: org_id,
        role,
    })
}

/// The membership role's grant, written exactly as the membership slice writes
/// it: grants cannot be edited, so a superseded row is revoked and a new one
/// inserted, and an existing grant of the same role is reused.
async fn upsert_role_grant(
    conn: &mut PgConnection,
    org_id: Uuid,
    account_id: Uuid,
    role: &str,
    granted_by: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE scope_kind = 'organization' AND scope_id = $2 AND subject_kind = 'account' \
         AND subject_id = $3 AND repository_id IS NULL AND revoked_at IS NULL \
         AND role IN ('owner', 'member') AND role <> $4",
    )
    .bind(granted_by)
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
           AND repository_id IS NULL AND role = $2 AND revoked_at IS NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(role)
    .bind(account_id)
    .bind(org_id)
    .bind(granted_by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Give the invited team `write` (or the inviter's chosen role) on every
/// active repository of the organization, which is what "the team carries
/// repository grants" means when a new member joins it.
async fn grant_team_repositories(
    conn: &mut PgConnection,
    org_id: Uuid,
    team_id: Uuid,
    granted_by: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, \
         repository_id, granted_by) \
         SELECT gen_random_uuid(), 'write', 'team', $1, 'repository', $2, r.id, $3 \
         FROM repositories r WHERE r.organization_id = $2 AND r.status = 'active' \
         AND NOT EXISTS (SELECT 1 FROM grants g WHERE g.subject_kind = 'team' AND g.subject_id = $1 \
           AND g.repository_id = r.id AND g.role = 'write' AND g.revoked_at IS NULL)",
    )
    .bind(team_id)
    .bind(org_id)
    .bind(granted_by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Json<Accepted>, ApiError> {
    let (account, _) = authenticate(&state.pool, &headers).await?;
    Ok(Json(accept_invitation(&state, account.id, &token).await?))
}

#[cfg(test)]
mod tests {
    use super::valid_email;

    #[test]
    fn the_address_rule_is_the_one_the_0049_check_applies() {
        // The same table runs against `accounts_email_format` in
        // `tests/invitations.rs`, so the validator and the CHECK cannot drift.
        for email in ["a@b.c", "x.y+z@sub.example.co", "a@b.cd"] {
            assert!(valid_email(email), "{email}");
        }
        for email in [
            "x@@y",
            "x@a",
            ".x@y.z",
            "@x.y",
            "x@",
            "x@.y",
            "x@y.",
            "x@y..z",
            "x\u{7f}@y.z",
            "x@y.z\n",
            "x@y.zé",
            "",
        ] {
            assert!(!valid_email(email), "{email:?}");
        }
        // A space is inside ASCII 32–126 and the rule does not ban it (the
        // domain rules keep it out of a real address); the CHECK agrees.
        assert!(valid_email("x y@y.z"));
    }
}
