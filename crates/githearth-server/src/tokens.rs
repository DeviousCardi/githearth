//! Access-token subjects (ADR 0014 section 2). Session-authenticated issuance
//! only: a token authenticates its subject but can never create, list or
//! revoke tokens. The plaintext is shown exactly once; only its SHA-256 is
//! stored.
//!
//! Three subjects exist: an account (the historical personal token), an
//! organization-owned service account, and one CI run. A scoped token carries
//! a resource set, scopes over it, an audience and a short expiry. The legacy
//! two-scope token is still issued when the request omits the resource set.

use std::path::{Path, PathBuf};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    ApiError, AppState, AuditActor, Credential, authenticate_token_hash, bearer_credential,
    event_with_actor, has_nul, invalid,
};

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/user/tokens", post(create).get(list))
        .route("/api/v1/user/tokens/{id}", delete(revoke))
}

/// The scopes a token may carry. The two legacy scopes keep their old meaning;
/// package scopes exist for the M7 registry credential.
const SCOPES: [&str; 4] = ["repo:read", "repo:write", "package:read", "package:write"];
const LEGACY_SCOPES: [&str; 2] = ["repo:read", "repo:write"];
const AUDIENCES: [&str; 3] = ["api", "git", "registry"];
const DEFAULT_AUDIENCE: [&str; 2] = ["api", "git"];
const DEFAULT_EXPIRY_SECONDS: i64 = 3_600;
const MAX_EXPIRY_SECONDS: i64 = 86_400;

/// Authenticates a session only: an access token must not manage tokens.
async fn session_auth(
    pool: &sqlx::PgPool,
    headers: &HeaderMap,
) -> ApiResult<(crate::Account, u64)> {
    match bearer_credential(headers)? {
        Credential::Session { token_hash } => authenticate_token_hash(pool, token_hash).await,
        Credential::AccessToken { .. } => Err(ApiError(
            StatusCode::FORBIDDEN,
            "access tokens cannot manage access tokens",
        )),
    }
}

fn scope_ok(scopes: &[String], allowed: &[&str]) -> bool {
    !scopes.is_empty()
        && scopes.len() <= allowed.len()
        && scopes.iter().all(|s| allowed.contains(&s.as_str()))
}

fn name_ok(name: &str) -> bool {
    (1..=100).contains(&name.chars().count()) && !name.trim().is_empty() && !has_nul(name)
}

fn audience_ok(audience: &[String]) -> bool {
    !audience.is_empty()
        && audience.len() <= AUDIENCES.len()
        && audience.iter().all(|a| AUDIENCES.contains(&a.as_str()))
}

#[derive(Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SubjectInput {
    Account,
    ServiceAccount { id: Uuid },
    Run { id: Uuid },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    name: String,
    scopes: Vec<String>,
    #[serde(default)]
    expires_in_days: Option<i64>,
    #[serde(default)]
    subject: Option<SubjectInput>,
    /// Canonical resource-set selectors (ADR 0014 section 2). Absent means a
    /// legacy two-scope token.
    #[serde(default)]
    resource_set: Option<serde_json::Value>,
    #[serde(default)]
    audience: Option<Vec<String>>,
    #[serde(default)]
    expires_in_seconds: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub(crate) struct TokenMeta {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) subject_kind: String,
    pub(crate) resource_set: Option<serde_json::Value>,
    pub(crate) audience: Vec<String>,
    pub(crate) legacy: bool,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
    pub(crate) last_used_at: Option<String>,
    pub(crate) revoked_at: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CreatedToken {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) subject_kind: String,
    pub(crate) resource_set: Option<serde_json::Value>,
    pub(crate) audience: Vec<String>,
    pub(crate) legacy: bool,
    pub(crate) created_at: String,
    pub(crate) expires_at: Option<String>,
    /// Shown once: the plaintext token string.
    pub(crate) token: String,
}

const TOKEN_COLUMNS: &str = "id, name, scopes, subject_kind, resource_set, audience, legacy, \
  to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
  to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at, \
  to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS last_used_at, \
  to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS revoked_at";

/// Where a subject's token is anchored, plus the organization for a non-account
/// subject (used for its audit chain).
struct Subject {
    kind: &'static str,
    organization: Option<Uuid>,
}

async fn resolve_subject(
    pool: &sqlx::PgPool,
    caller: Uuid,
    subject: Option<SubjectInput>,
) -> ApiResult<Subject> {
    match subject.unwrap_or(SubjectInput::Account) {
        SubjectInput::Account => Ok(Subject {
            kind: "account",
            organization: None,
        }),
        SubjectInput::ServiceAccount { id } => {
            let row: Option<(Uuid, bool)> = sqlx::query_as(
                "SELECT sa.organization_id, (sa.disabled_at IS NOT NULL) \
                 FROM service_accounts sa JOIN memberships m ON m.organization_id = sa.organization_id \
                 WHERE sa.id = $1 AND m.account_id = $2 AND m.role = 'owner'",
            )
            .bind(id)
            .bind(caller)
            .fetch_optional(pool)
            .await?;
            let Some((organization, disabled)) = row else {
                return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
            };
            if disabled {
                return Err(ApiError(
                    StatusCode::FORBIDDEN,
                    "service account is disabled",
                ));
            }
            Ok(Subject {
                kind: "service_account",
                organization: Some(organization),
            })
        }
        SubjectInput::Run { id } => {
            let row: Option<(Uuid, String, bool)> = sqlx::query_as(
                "SELECT organization_id, status, (ended_at IS NOT NULL) \
                 FROM ci_runs WHERE id = $1 AND triggered_by = $2",
            )
            .bind(id)
            .bind(caller)
            .fetch_optional(pool)
            .await?;
            let Some((organization, status, ended)) = row else {
                return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
            };
            if ended || status != "running" {
                return Err(ApiError(StatusCode::FORBIDDEN, "run has ended"));
            }
            Ok(Subject {
                kind: "run",
                organization: Some(organization),
            })
        }
    }
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    let created = create_request(&state, account.id, input).await?;
    let created = created.ok_or(ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal error",
    ))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Shared by the JSON handler and the settings page's legacy "Create token"
/// form. Validation stays here so a bad name, scope set or expiry fails both
/// callers identically. Returns the plaintext exactly once.
pub(crate) async fn create_token(
    state: &AppState,
    account_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_in_days: Option<i64>,
    idempotency_key: Option<&str>,
) -> ApiResult<Option<CreatedToken>> {
    create_request_with_key(
        state,
        account_id,
        NewToken {
            name: name.to_owned(),
            scopes: scopes.to_vec(),
            expires_in_days,
            subject: None,
            resource_set: None,
            audience: None,
            expires_in_seconds: None,
        },
        idempotency_key,
    )
    .await
}

async fn create_request(
    state: &AppState,
    account_id: Uuid,
    input: NewToken,
) -> ApiResult<Option<CreatedToken>> {
    create_request_with_key(state, account_id, input, None).await
}

async fn create_request_with_key(
    state: &AppState,
    account_id: Uuid,
    input: NewToken,
    idempotency_key: Option<&str>,
) -> ApiResult<Option<CreatedToken>> {
    if !name_ok(&input.name) {
        return Err(invalid());
    }
    let subject = resolve_subject(&state.pool, account_id, input.subject.clone()).await?;
    let service_account_id = match &input.subject {
        Some(SubjectInput::ServiceAccount { id }) => Some(*id),
        _ => None,
    };
    let run_id = match &input.subject {
        Some(SubjectInput::Run { id }) => Some(*id),
        _ => None,
    };

    // New scoped token: resource set, scopes, audience and a short expiry.
    let (resource_set, audience, expires_at_sql, expires_in_seconds, legacy, scopes) = if let Some(
        raw_set,
    ) =
        input.resource_set
    {
        let resource_set = resolve_resource_set(&state.pool, &raw_set).await?;
        if !scope_ok(&input.scopes, &SCOPES) || input.expires_in_days.is_some() {
            return Err(invalid());
        }
        let audience = input
            .audience
            .unwrap_or_else(|| DEFAULT_AUDIENCE.iter().map(|s| (*s).to_owned()).collect());
        if !audience_ok(&audience) {
            return Err(invalid());
        }
        let seconds = input.expires_in_seconds.unwrap_or(DEFAULT_EXPIRY_SECONDS);
        if !(60..=MAX_EXPIRY_SECONDS).contains(&seconds) {
            return Err(invalid());
        }
        (
            Some(resource_set),
            audience,
            "clock_timestamp() + $14 * interval '1 second'",
            Some(seconds),
            false,
            input.scopes,
        )
    } else {
        // Legacy two-scope token, account subject only.
        if !matches!(subject.kind, "account")
            || !scope_ok(&input.scopes, &LEGACY_SCOPES)
            || input.expires_in_seconds.is_some()
        {
            return Err(invalid());
        }
        // After the window a legacy token would be refused on first use; do not
        // mint one. `legacy_token_deprecation_date` is bounded by config
        // validation, and the deadline is its last UTC instant.
        let legacy_open: bool = sqlx::query_scalar(
            "SELECT ($1::date + 1)::timestamp AT TIME ZONE 'UTC' > clock_timestamp()",
        )
        .bind(&state.config.legacy_token_deprecation_date)
        .fetch_one(&state.pool)
        .await?;
        if !legacy_open {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "legacy tokens are no longer issued; create a scoped token",
            ));
        }
        if let Some(days) = input.expires_in_days
            && !(1..=3650).contains(&days)
        {
            return Err(invalid());
        }
        (
            None,
            DEFAULT_AUDIENCE.iter().map(|s| (*s).to_owned()).collect(),
            "CASE WHEN $13::bigint IS NULL THEN NULL ELSE clock_timestamp() + $13 * interval '1 day' END",
            None,
            true,
            input.scopes,
        )
    };

    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let plaintext = format!("gth_{}", hex::encode(bytes));
    let token_hash = Sha256::digest(plaintext.as_bytes()).to_vec();
    let id = Uuid::new_v4();

    let mut tx = state.pool.begin().await?;
    let inserted = sqlx::query(&format!(
        "INSERT INTO access_tokens \
         (id, account_id, service_account_id, run_id, subject_kind, name, token_hash, scopes, \
          expires_at, idempotency_key, resource_set, audience, legacy, legacy_deadline) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, {expires_at_sql}, $9, $10, $11, $12, \
          CASE WHEN $12 THEN ($15::date + 1)::timestamp AT TIME ZONE 'UTC' \
            ELSE NULL END) \
         ON CONFLICT (account_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING"
    ))
    .bind(id)
    .bind((subject.kind == "account").then_some(account_id))
    .bind(service_account_id)
    .bind(run_id)
    .bind(subject.kind)
    .bind(&input.name)
    .bind(token_hash)
    .bind(&scopes)
    .bind(idempotency_key)
    .bind(&resource_set)
    .bind(&audience)
    .bind(legacy)
    .bind(input.expires_in_days)
    .bind(expires_in_seconds)
    .bind(&state.config.legacy_token_deprecation_date)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        tx.commit().await?;
        return Ok(None);
    }
    let audit_actor = match subject.kind {
        "service_account" => AuditActor::service_account(
            service_account_id.expect("service account kind implies a subject id"),
        ),
        "run" => AuditActor::run(run_id.expect("run kind implies a subject id"), account_id),
        _ => AuditActor::account(account_id),
    };
    event_with_actor(
        &mut tx,
        subject.organization,
        audit_actor,
        "token.created",
        serde_json::json!({"token_id": id, "subject_kind": subject.kind}),
    )
    .await?;
    let row = sqlx::query(&format!(
        "SELECT {TOKEN_COLUMNS} FROM access_tokens WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Some(CreatedToken {
        id,
        name: input.name,
        scopes,
        subject_kind: row.get("subject_kind"),
        resource_set: row.get("resource_set"),
        audience: row.get("audience"),
        legacy: row.get("legacy"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        token: plaintext,
    }))
}

/// Resolve the issuance grammar's selectors to the canonical stored form.
/// Names are resolved inside their organization; an unknown name is a 422, not
/// a silently empty set.
async fn resolve_resource_set(
    pool: &sqlx::PgPool,
    raw: &serde_json::Value,
) -> ApiResult<serde_json::Value> {
    let items = raw.as_array().ok_or_else(invalid)?;
    if items.is_empty() {
        return Err(invalid());
    }
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        let kind = item
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(invalid)?;
        match kind {
            "repository" => {
                let id = match item.get("id").and_then(|v| v.as_str()) {
                    Some(id) => id.parse::<Uuid>().map_err(|_| invalid())?,
                    None => {
                        let organization = item
                            .get("organization")
                            .and_then(|v| v.as_str())
                            .ok_or_else(invalid)?;
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or_else(invalid)?;
                        sqlx::query_scalar(
                            "SELECT r.id FROM repositories r JOIN organizations o ON o.id = r.organization_id \
                             WHERE o.slug = $1 AND r.name = $2 AND r.status <> 'deleted'",
                        )
                        .bind(organization)
                        .bind(name)
                        .fetch_optional(pool)
                        .await?
                        .ok_or_else(invalid)?
                    }
                };
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM repositories WHERE id = $1 AND status <> 'deleted')",
                )
                .bind(id)
                .fetch_one(pool)
                .await?;
                if !exists {
                    return Err(invalid());
                }
                resolved.push(serde_json::json!({"type": "repository", "id": id}));
            }
            "organization" => {
                let id = match item.get("id").and_then(|v| v.as_str()) {
                    Some(id) => id.parse::<Uuid>().map_err(|_| invalid())?,
                    None => {
                        let slug = item
                            .get("slug")
                            .and_then(|v| v.as_str())
                            .ok_or_else(invalid)?;
                        sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
                            .bind(slug)
                            .fetch_optional(pool)
                            .await?
                            .ok_or_else(invalid)?
                    }
                };
                resolved.push(serde_json::json!({"type": "organization", "id": id}));
            }
            "package" => {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| (1..=200).contains(&n.chars().count()))
                    .ok_or_else(invalid)?;
                let organization = match item.get("organization").and_then(|v| v.as_str()) {
                    Some(value) if value.len() == 36 => {
                        value.parse::<Uuid>().map_err(|_| invalid())?
                    }
                    Some(slug) => {
                        sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
                            .bind(slug)
                            .fetch_optional(pool)
                            .await?
                            .ok_or_else(invalid)?
                    }
                    None => return Err(invalid()),
                };
                resolved.push(
                    serde_json::json!({"type": "package", "organization": organization, "name": name}),
                );
            }
            _ => return Err(invalid()),
        }
    }
    Ok(serde_json::Value::Array(resolved))
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<TokenMeta>>> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    Ok(Json(list_tokens(&state, account.id).await?))
}

/// The account's own tokens, its service accounts' tokens (where it is an
/// organization owner) and the tokens of runs it triggered.
pub(crate) async fn list_tokens(state: &AppState, account_id: Uuid) -> ApiResult<Vec<TokenMeta>> {
    let rows = sqlx::query_as::<_, TokenMeta>(&format!(
        "SELECT {TOKEN_COLUMNS} FROM access_tokens t \
         WHERE t.account_id = $1 \
            OR t.run_id IN (SELECT id FROM ci_runs WHERE triggered_by = $1) \
            OR t.service_account_id IN ( \
                 SELECT sa.id FROM service_accounts sa JOIN memberships m \
                  ON m.organization_id = sa.organization_id \
                 WHERE m.account_id = $1 AND m.role = 'owner') \
         ORDER BY created_at, id"
    ))
    .bind(account_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows)
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<Uuid>,
) -> ApiResult<StatusCode> {
    let (account, _) = session_auth(&state.pool, &headers).await?;
    revoke_token(&state, account.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Shared by the JSON handler and the settings page's "Revoke" button. A
/// missing id or a token the caller may not see is a 404 for both callers.
pub(crate) async fn revoke_token(state: &AppState, account_id: Uuid, id: Uuid) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT t.subject_kind, t.account_id, t.service_account_id, t.run_id, r.triggered_by, \
                COALESCE(sa.organization_id, r.organization_id) AS organization \
         FROM access_tokens t \
         LEFT JOIN ci_runs r ON r.id = t.run_id \
         LEFT JOIN service_accounts sa ON sa.id = t.service_account_id \
         WHERE t.id = $1 AND t.revoked_at IS NULL AND ( \
           t.account_id = $2 \
           OR t.run_id IN (SELECT id FROM ci_runs WHERE triggered_by = $2) \
           OR t.service_account_id IN ( \
                SELECT sa.id FROM service_accounts sa JOIN memberships m \
                 ON m.organization_id = sa.organization_id \
                WHERE m.account_id = $2 AND m.role = 'owner'))",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(ApiError(StatusCode::NOT_FOUND, "token not found"));
    };
    sqlx::query("UPDATE access_tokens SET revoked_at = clock_timestamp() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let actor = match row.get::<String, _>("subject_kind").as_str() {
        "service_account" => AuditActor::service_account(row.get("service_account_id")),
        "run" => AuditActor::run(row.get("run_id"), row.get("triggered_by")),
        _ => AuditActor::account(row.get("account_id")),
    };
    event_with_actor(
        &mut tx,
        row.get("organization"),
        actor,
        "token.revoked",
        serde_json::json!({"token_id": id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// A token delivered to a job: the plaintext lives in a mode-0600 file whose
/// path is exported, and a masked variable carries it for tools that need it
/// inline. The caller owns the directory and is responsible for deletion when
/// the run ends. M6's job runner is the first caller; the contract is here so
/// it is tested now.
#[allow(dead_code)]
pub struct Delivery {
    pub file: PathBuf,
    pub env: Vec<(&'static str, String)>,
    pub masked: &'static str,
}

#[allow(dead_code)]
pub const TOKEN_FILE_VAR: &str = "GITHEARTH_TOKEN_FILE";
#[allow(dead_code)]
pub const TOKEN_VAR: &str = "GITHEARTH_TOKEN";
#[allow(dead_code)]
pub const MASKED_TOKEN: &str = "***";

#[allow(dead_code)]
pub fn deliver_token(directory: &Path, token: &str) -> std::io::Result<Delivery> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let file = directory.join("token");
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)?;
    handle.write_all(token.as_bytes())?;
    handle.write_all(b"\n")?;
    Ok(Delivery {
        file: file.clone(),
        env: vec![
            (TOKEN_FILE_VAR, file.display().to_string()),
            (TOKEN_VAR, token.to_owned()),
        ],
        masked: MASKED_TOKEN,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_writes_a_private_file_and_a_masked_variable() {
        let dir = std::env::temp_dir().join(format!("githearth-delivery-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let delivery = deliver_token(&dir, "gth_secret").unwrap();
        assert_eq!(
            std::fs::read_to_string(&delivery.file).unwrap(),
            "gth_secret\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&delivery.file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let inline = delivery
            .env
            .iter()
            .find(|(name, _)| *name == TOKEN_VAR)
            .unwrap();
        assert_eq!(inline.1, "gth_secret");
        assert_eq!(delivery.masked, "***");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resource_set_grammar_rejects_an_empty_list() {
        assert!(crate::parse_resource_set(&serde_json::json!([])).is_none());
        assert!(crate::parse_resource_set(&serde_json::json!([{"type": "unknown"}])).is_none());
    }
}
