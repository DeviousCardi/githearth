//! Audit chain endpoints.
//!
//! # Limits
//! A database owner can disable the triggers and rewrite the whole chain, so
//! tamper evidence against them requires exporting/anchoring the head outside
//! the database (follow-up). Every audited write serializes on its
//! organization's chain row until commit (measure before M4).

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use futures_util::StreamExt;
use githearth_authz::{Action, Resource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Row};
use std::time::Duration;
use uuid::Uuid;

use crate::{ApiError, AppState, authenticate, facts, invalid};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/organizations/{org}/audit", get(list))
        .route(
            "/api/v1/organizations/{org}/audit/verify",
            get(verify_endpoint),
        )
}

const SITE_CHAIN: Uuid = Uuid::nil();
const ZERO_HASH: [u8; 32] = [0u8; 32];

fn does_not_exist() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found")
}

/// Authorize reading the audit log. There is no `audit` action in
/// `githearth-authz`; reading audit is an owner/admin concern, so gate on the
/// existing owner-only sensitive action `organization.update`. A member who
/// can read the organization but not administer it gets 403, anyone else 404.
async fn authorize(
    state: &AppState,
    account_id: Uuid,
    auth_age: u64,
    org_slug: &str,
) -> Result<(Uuid, facts::Facts), ApiError> {
    let mut conn = state.pool.acquire().await?;
    let org_id = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(org_slug)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(does_not_exist)?;
    let Some(facts) = facts::load(&mut conn, account_id, org_id).await? else {
        return Err(does_not_exist());
    };
    if !state
        .authz
        .authorize(&facts.request(
            Action::OrganizationUpdate,
            Resource::Organization,
            Some(Duration::from_secs(auth_age)),
        ))
        .is_allowed()
    {
        if state
            .authz
            .authorize(&facts.request(
                Action::OrganizationRead,
                Resource::Organization,
                Some(Duration::from_secs(auth_age)),
            ))
            .is_allowed()
        {
            return Err(ApiError(StatusCode::FORBIDDEN, "not permitted"));
        }
        return Err(does_not_exist());
    }
    Ok((org_id, facts))
}

#[derive(Serialize)]
pub struct Verification {
    pub rows: u64,
    pub ok: bool,
    pub first_bad_seq: Option<i64>,
}

/// Recompute every hash in seq order for one chain and compare it with the
/// stored `hash`/`prev_hash` and the `audit_chain` head. Streaming, so a long
/// chain is not loaded into memory.
pub async fn verify(
    conn: &mut PgConnection,
    chain: Option<Uuid>,
) -> Result<Verification, sqlx::Error> {
    let chain = chain.unwrap_or(SITE_CHAIN);
    // Read the head first and bound the stream by it: under READ COMMITTED the
    // rows and the head would otherwise come from different snapshots, so a
    // concurrent writer could commit new rows with an old head (false alarm).
    // A missing head is an empty chain; bound to 0 so a concurrently inserted
    // first row is not streamed against a `None` head (which would also alarm).
    let head: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT last_seq, last_hash FROM audit_chain WHERE chain = $1")
            .bind(chain)
            .fetch_optional(&mut *conn)
            .await?;
    let head_seq = head.as_ref().map_or(0, |(s, _)| *s);
    // Plain predicates keep the (organization_id, seq) index usable; the site chain is the NULL rows.
    let chain_filter = if chain == SITE_CHAIN {
        "organization_id IS NULL AND $1::uuid IS NOT NULL"
    } else {
        "organization_id = $1"
    };
    let sql = format!(
        "SELECT seq, id, organization_id, actor_id, kind, \
         (EXTRACT(EPOCH FROM created_at) * 1000000)::bigint AS micros, prev_hash, hash, \
         host(client_ip) AS client_ip, client_ip_key_id, client_ip_digest \
         FROM audit_events WHERE {chain_filter} AND seq IS NOT NULL AND seq <= $2 ORDER BY seq"
    );
    let mut rows = sqlx::query(&sql)
        .bind(chain)
        .bind(head_seq)
        .fetch(&mut *conn);

    let mut count = 0u64;
    let mut ok = true;
    let mut first_bad: Option<i64> = None;
    let mut expected_prev = ZERO_HASH;
    let mut last_seq = 0i64;
    let mut last_hash = None::<[u8; 32]>;

    while let Some(row) = rows.next().await {
        let row = row?;
        count += 1;
        let seq: i64 = row.get("seq");
        let prev_hash: Vec<u8> = row.get("prev_hash");
        let hash: Vec<u8> = row.get("hash");
        let recomputed = hash_row(&row, seq, &expected_prev);
        if ok && (prev_hash.as_slice() != expected_prev || hash.as_slice() != recomputed.as_slice())
        {
            ok = false;
            first_bad = Some(seq);
        }
        expected_prev.copy_from_slice(&recomputed);
        last_seq = seq;
        let mut h = [0u8; 32];
        h.copy_from_slice(&recomputed);
        last_hash = Some(h);
    }
    drop(rows);

    if ok {
        let head_matches = match (&head, last_hash) {
            (Some((s, h)), Some(lh)) => {
                count > 0 && *s == last_seq && h.as_slice() == lh.as_slice()
            }
            (None, None) => count == 0,
            _ => false,
        };
        if !head_matches {
            ok = false;
            first_bad = first_bad.or_else(|| (count > 0).then_some(last_seq));
        }
    }

    Ok(Verification {
        rows: count,
        ok,
        first_bad_seq: first_bad,
    })
}

/// The canonical encoding from the migration, mirrored byte-for-byte:
/// id || organization_id (nil when NULL) || actor_id || len(kind) || kind
/// || seq || created_at microseconds, all big-endian, followed by exactly one
/// trailing form:
///   V3 — key_id (16 bytes) || digest (32 bytes) when a digest is present
///   (the raw IP is NOT part of the hash, so it can be pruned later);
///   V2 — len(host(client_ip)) || host(client_ip) when the IP is present;
///   V1 — nothing extra when both are absent.
fn hash_row(row: &sqlx::postgres::PgRow, seq: i64, prev: &[u8; 32]) -> Vec<u8> {
    let id: Uuid = row.get("id");
    let org: Option<Uuid> = row.get("organization_id");
    let actor: Uuid = row.get("actor_id");
    let kind: String = row.get("kind");
    let micros: i64 = row.get("micros");
    let client_ip: Option<String> = row.get("client_ip");
    let key_id: Option<Uuid> = row.get("client_ip_key_id");
    let digest: Option<Vec<u8>> = row.get("client_ip_digest");
    let mut hasher = Sha256::new();
    hasher.update(prev);
    hasher.update(id.as_bytes());
    hasher.update(org.unwrap_or(SITE_CHAIN).as_bytes());
    hasher.update(actor.as_bytes());
    hasher.update((kind.len() as i64).to_be_bytes());
    hasher.update(kind.as_bytes());
    hasher.update(seq.to_be_bytes());
    hasher.update(micros.to_be_bytes());
    if let Some(digest) = digest {
        hasher.update(key_id.expect("a digest implies a key id").as_bytes());
        hasher.update(&digest);
    } else if let Some(ip) = client_ip {
        hasher.update((ip.len() as i64).to_be_bytes());
        hasher.update(ip.as_bytes());
    }
    hasher.finalize().to_vec()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    after: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct AuditItem {
    seq: i64,
    kind: String,
    actor_id: Uuid,
    created_at: String,
    hash: String,
    client_ip: Option<String>,
}

const AUDIT_ITEM_COLUMNS: &str = "seq, kind, actor_id, \
 to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
 encode(hash, 'hex') AS hash, \
 host(client_ip) AS client_ip";

#[derive(Serialize)]
struct AuditPage {
    items: Vec<AuditItem>,
    next: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<AuditPage>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let limit = query.limit.unwrap_or(30);
    if !(1..=200).contains(&limit) {
        return Err(invalid());
    }
    let (org_id, _facts) = authorize(&state, actor.id, auth_age, &org_slug).await?;

    let mut conn = state.pool.acquire().await?;
    let rows = sqlx::query_as::<_, AuditItem>(&format!(
        "SELECT {AUDIT_ITEM_COLUMNS} FROM audit_events \
             WHERE organization_id = $1 AND ($2::bigint IS NULL OR seq > $2) \
             ORDER BY seq LIMIT $3"
    ))
    .bind(org_id)
    .bind(query.after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;

    let has_more = rows.len() as i64 > limit;
    let mut items = rows;
    items.truncate(limit as usize);
    let next = if has_more {
        items.last().map(|i| i.seq)
    } else {
        None
    };
    Ok(Json(AuditPage { items, next }))
}

async fn verify_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(org_slug): Path<String>,
) -> Result<Json<Verification>, ApiError> {
    let (actor, auth_age) = authenticate(&state.pool, &headers).await?;
    let (org_id, _facts) = authorize(&state, actor.id, auth_age, &org_slug).await?;
    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let verification = verify(&mut tx, Some(org_id)).await?;
    tx.commit().await?;
    Ok(Json(verification))
}
