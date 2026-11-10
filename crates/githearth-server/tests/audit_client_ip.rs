//! Client IP recording on audit events, inside the tamper-evident chain.
//! Opt in with --ignored, like the other database-backed tests.
use std::net::SocketAddr;

use axum::{
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::json;
use sha2::Sha256;
use sqlx::{Acquire, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

async fn setup() -> (axum::Router, PgPool, std::path::PathBuf, String, Uuid) {
    let storage_root = std::env::temp_dir().join(format!("githearth-audit-ip-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 32,
        database_timeout_seconds: 5,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    // A single account with a known password, so login can be exercised.
    let username = tag("user");
    let id = Uuid::new_v4();
    let hash = hash_password("a sufficiently long password").unwrap();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,$3,false)",
    )
    .bind(id)
    .bind(&username)
    .bind(hash)
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, storage_root, username, id)
}

async fn login_request(
    app: &axum::Router,
    username: &str,
    connect_info: Option<SocketAddr>,
) -> StatusCode {
    let body = json!({ "username": username, "password": "a sufficiently long password" });
    let mut builder = Request::post("/api/v1/login")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    if let Some(addr) = connect_info {
        builder.extensions_mut().insert(ConnectInfo(addr));
    }
    let response = app.clone().oneshot(builder).await.unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap().to_bytes();
    status
}

/// The client_ip of this fixture's own audit rows (login emits a
/// session.created site-chain event with actor_id = this account).
async fn client_ips_for_actor(pool: &PgPool, actor_id: Uuid) -> Vec<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT host(client_ip) FROM audit_events \
         WHERE actor_id = $1 AND seq IS NOT NULL ORDER BY seq",
    )
    .bind(actor_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// (client_ip, client_ip_key_id, client_ip_digest) for this actor's own
/// session.created row.
async fn ip_row_for_actor(
    pool: &PgPool,
    actor_id: Uuid,
) -> (Option<String>, Option<Uuid>, Option<Vec<u8>>) {
    sqlx::query_as(
        "SELECT host(client_ip), client_ip_key_id, client_ip_digest \
         FROM audit_events WHERE actor_id = $1 AND kind = 'session.created' AND seq IS NOT NULL \
         ORDER BY seq",
    )
    .bind(actor_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn http_action_records_client_ip_on_audit_row() {
    let (app, pool, storage_root, username, account) = setup().await;
    let status = login_request(
        &app,
        &username,
        Some(SocketAddr::from(([203, 0, 113, 7], 4242))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (ip, key_id, digest) = ip_row_for_actor(&pool, account).await;
    assert_eq!(ip, Some("203.0.113.7".to_string()));
    let key_id = key_id.expect("a ConnectInfo request records a key id");
    let digest = digest.expect("a ConnectInfo request records a digest");
    assert_eq!(digest.len(), 32);

    let key: Vec<u8> = sqlx::query_scalar("SELECT key FROM audit_ip_keys WHERE id = $1")
        .bind(key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(digest, hmac(&key, b"203.0.113.7"));

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn http_action_without_connect_info_records_null() {
    let (app, pool, storage_root, username, account) = setup().await;
    let status = login_request(&app, &username, None).await;
    assert_eq!(status, StatusCode::OK);

    let ips = client_ips_for_actor(&pool, account).await;
    assert_eq!(ips, vec![None]);

    let (ip, key_id, digest) = ip_row_for_actor(&pool, account).await;
    assert_eq!((ip, key_id, digest), (None, None, None));

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn verify_accepts_mixed_null_and_set_client_ips() {
    // Build a chain for a fresh organization with a NULL-IP row followed by a
    // set-IP row, then confirm verify() still reports ok: the Rust hash must
    // equal the trigger's stored hash byte-for-byte for both encodings.
    let (pool, storage_root) = plain_pool().await;
    let owner = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
    )
    .bind(owner)
    .bind(tag("owner"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(&pool)
        .await
        .unwrap();

    insert_event(&pool, Some(org_id), owner, "mixed.null", None).await;
    insert_event(&pool, Some(org_id), owner, "mixed.ip", Some("203.0.113.7")).await;

    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    let v = githearth_server::audit::verify(&mut tx, Some(org_id))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(v.rows, 2);
    assert!(v.ok, "chain with mixed client_ip must verify");
    assert_eq!(v.first_bad_seq, None);

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pruning_client_ip_keeps_chain_verifiable() {
    let (pool, storage_root) = plain_pool().await;
    let owner = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
    )
    .bind(owner)
    .bind(tag("owner"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(&pool)
        .await
        .unwrap();

    let key_id = Uuid::new_v4();
    let key = [0x5au8; 32];
    sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
        .bind(key_id)
        .bind(key.to_vec())
        .execute(&pool)
        .await
        .unwrap();
    let digest = hmac(&key, b"203.0.113.7");
    let id = insert_digest_event(
        &pool,
        Some(org_id),
        owner,
        "prune.me",
        "203.0.113.7",
        key_id,
        &digest,
    )
    .await;

    let (rows, ok, _bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows, ok), (1, true));

    // Pruning the raw IP is the one permitted UPDATE; the chain still verifies
    // because the hash only ever contained key_id || digest.
    sqlx::query("UPDATE audit_events SET client_ip = NULL WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (rows, ok, bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows, ok, bad), (1, true, None));

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleting_key_keeps_chain_verifiable() {
    let (app, pool, storage_root, username, account) = setup().await;

    // This test's own key, newest by created_at so event_with picks it.
    let key_id = Uuid::new_v4();
    let key = [0x33u8; 32];
    sqlx::query(
        "INSERT INTO audit_ip_keys (id, key, created_at) \
         VALUES ($1, $2, clock_timestamp() + interval '1000 years')",
    )
    .bind(key_id)
    .bind(key.to_vec())
    .execute(&pool)
    .await
    .unwrap();

    let status = login_request(
        &app,
        &username,
        Some(SocketAddr::from(([203, 0, 113, 7], 4242))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (ip, row_key_id, digest) = ip_row_for_actor(&pool, account).await;
    assert_eq!(ip, Some("203.0.113.7".to_string()));
    assert_eq!(row_key_id, Some(key_id));
    assert_eq!(digest, Some(hmac(&key, b"203.0.113.7")));

    let (_rows, ok, _bad) = verify_chain(&pool, None).await;
    assert!(ok, "site chain must verify before key deletion");

    sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
        .bind(key_id)
        .execute(&pool)
        .await
        .unwrap();

    let (_rows, ok, bad) = verify_chain(&pool, None).await;
    assert!(ok, "site chain must still verify after key deletion");
    assert_eq!(bad, None);

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn only_ip_pruning_updates_are_allowed() {
    let (pool, storage_root) = plain_pool().await;
    let owner = Uuid::new_v4();
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
    )
    .bind(owner)
    .bind(tag("owner"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(&pool)
        .await
        .unwrap();

    let key_id = Uuid::new_v4();
    let key = [0x5au8; 32];
    sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
        .bind(key_id)
        .bind(key.to_vec())
        .execute(&pool)
        .await
        .unwrap();
    let digest = hmac(&key, b"203.0.113.7");
    let id = insert_digest_event(
        &pool,
        Some(org_id),
        owner,
        "prune.me",
        "203.0.113.7",
        key_id,
        &digest,
    )
    .await;

    // A legacy v2 row with a raw IP but no digest.
    let plain_id = insert_event(&pool, Some(org_id), owner, "legacy", Some("203.0.113.7")).await;

    // Changing the digest.
    assert_append_only(
        sqlx::query("UPDATE audit_events SET client_ip_digest = decode(repeat('11',32),'hex') WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap_err(),
    );
    // Changing the key id.
    assert_append_only(
        sqlx::query("UPDATE audit_events SET client_ip_key_id = $2 WHERE id = $1")
            .bind(id)
            .bind(Uuid::new_v4())
            .execute(&pool)
            .await
            .unwrap_err(),
    );
    // Changing the IP to another value.
    assert_append_only(
        sqlx::query("UPDATE audit_events SET client_ip = '198.51.100.9'::inet WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap_err(),
    );
    // Pruning the IP while also changing another column.
    assert_append_only(
        sqlx::query("UPDATE audit_events SET client_ip = NULL, kind = 'edited' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap_err(),
    );
    // Pruning the IP on a row that has no digest.
    assert_append_only(
        sqlx::query("UPDATE audit_events SET client_ip = NULL WHERE id = $1")
            .bind(plain_id)
            .execute(&pool)
            .await
            .unwrap_err(),
    );

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

fn assert_append_only(err: sqlx::Error) {
    let message = err
        .as_database_error()
        .map(|e| e.message())
        .unwrap_or_default();
    assert!(
        message.contains("audit log is append-only"),
        "unexpected error: {err:?}"
    );
}

async fn plain_pool() -> (PgPool, std::path::PathBuf) {
    let storage_root = std::env::temp_dir().join(format!("githearth-audit-ip-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 32,
        database_timeout_seconds: 5,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    (pool, storage_root)
}

async fn verify_chain(pool: &PgPool, chain: Option<Uuid>) -> (u64, bool, Option<i64>) {
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    let v = githearth_server::audit::verify(&mut tx, chain)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (v.rows, v.ok, v.first_bad_seq)
}

/// Insert an audit row directly so the BEFORE INSERT trigger computes
/// seq/prev_hash/hash via the v2 encoding, mirroring how the fixture org's
/// chain is populated. `client_ip` may be a dotted-quad string or NULL.
async fn insert_event(
    pool: &PgPool,
    org: Option<Uuid>,
    actor: Uuid,
    kind: &str,
    ip: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip) VALUES ($1,$2,$3,$4,$5::inet)",
    )
    .bind(id)
    .bind(org)
    .bind(actor)
    .bind(kind)
    .bind(ip)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Insert an audit row with a client IP digest, so the trigger hashes via v3.
async fn insert_digest_event(
    pool: &PgPool,
    org: Option<Uuid>,
    actor: Uuid,
    kind: &str,
    ip: &str,
    key_id: Uuid,
    digest: &[u8],
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip,client_ip_key_id,client_ip_digest) \
         VALUES ($1,$2,$3,$4,$5::inet,$6,$7)",
    )
    .bind(id)
    .bind(org)
    .bind(actor)
    .bind(kind)
    .bind(ip)
    .bind(key_id)
    .bind(digest)
    .execute(pool)
    .await
    .unwrap();
    id
}
