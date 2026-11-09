//! Client IP recording on audit events, inside the tamper-evident chain.
//! Opt in with --ignored, like the other database-backed tests.
use std::net::SocketAddr;

use axum::{
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::{Acquire, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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

    let ips = client_ips_for_actor(&pool, account).await;
    assert_eq!(ips, vec![Some("203.0.113.7".to_string())]);

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

/// Insert an audit row directly so the BEFORE INSERT trigger computes
/// seq/prev_hash/hash via the v2 encoding, mirroring how the fixture org's
/// chain is populated. `client_ip` may be a dotted-quad string or NULL.
async fn insert_event(pool: &PgPool, org: Option<Uuid>, actor: Uuid, kind: &str, ip: Option<&str>) {
    sqlx::query(
        "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip) VALUES ($1,$2,$3,$4,$5::inet)",
    )
    .bind(Uuid::new_v4())
    .bind(org)
    .bind(actor)
    .bind(kind)
    .bind(ip)
    .execute(pool)
    .await
    .unwrap();
}
