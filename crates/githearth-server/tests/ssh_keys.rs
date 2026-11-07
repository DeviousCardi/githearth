//! SSH keys: API lifecycle, parsing rules, and authentication (ADR 0005,
//! Person A).
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router, ssh_keys};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::Path;
use std::process::Command;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture helpers (mirror tests/access_tokens.rs)
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login(app: &Router, username: &str, password: &str) -> String {
    let (status, value) = request(
        app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    value["token"].as_str().unwrap().to_owned()
}

async fn fixture_account(pool: &PgPool, username: &str, password: &str) -> Uuid {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(hash)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn create_token(app: &Router, session: &str, scopes: &[&str]) -> String {
    let (status, value) = request(
        app,
        "POST",
        "/api/v1/user/tokens",
        Some(session),
        json!({"name": tag("token"), "scopes": scopes}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    value["token"].as_str().unwrap().to_owned()
}

async fn api_pool() -> (PgPool, Router) {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-sshk-{}", Uuid::new_v4())),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    (pool, app)
}

// ---------------------------------------------------------------------------
// Key generation via ssh-keygen
// ---------------------------------------------------------------------------

fn temp_key(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("githearth-sshk-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(label)
}

fn generate_key(key_type: &str, path: &Path) {
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            key_type,
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "githearth-test",
            "-q",
        ])
        .output()
        .expect("ssh-keygen failed");
    assert!(
        output.status.success(),
        "ssh-keygen -t {key_type}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn public_key_line(path: &Path) -> String {
    std::fs::read_to_string(path.with_extension("pub")).unwrap()
}

fn sha256_fingerprint(path: &Path) -> String {
    let output = Command::new("ssh-keygen")
        .args(["-lf", &path.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// API tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_returns_metadata_with_fingerprint() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    let expected_fp = sha256_fingerprint(&key_path);

    let (status, value) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&session),
        json!({"name": "laptop", "public_key": public_key}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    assert_eq!(value["fingerprint"], expected_fp);
    assert_eq!(value["name"], "laptop");
    assert_eq!(value["algorithm"], "ed25519");
    assert!(
        value["public_key"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .nth(2)
            .is_none()
    );
    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn list_revoke_and_list_shows_revocation() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);

    let (status, value) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&session),
        json!({"name": "laptop", "public_key": public_key}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let key_id = value["id"].as_str().unwrap().to_owned();

    let (status, listed) = request(
        &app,
        "GET",
        "/api/v1/user/ssh-keys",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = listed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], key_id);
    assert!(list[0].get("revoked_at").unwrap().is_null());

    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/user/ssh-keys/{key_id}"),
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, listed) = request(
        &app,
        "GET",
        "/api/v1/user/ssh-keys",
        Some(&session),
        Value::Null,
    )
    .await;
    let list = listed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].get("revoked_at").unwrap().is_string());

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unsupported_and_bad_keys_are_rejected() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    // RSA is not supported (server built without the rsa feature).
    let rsa_path = temp_key("id_rsa");
    generate_key("rsa", &rsa_path);
    let rsa_line = public_key_line(&rsa_path);
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&session),
        json!({"name": "rsa", "public_key": rsa_line}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Garbage is a 400.
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&session),
            json!({"name": "bad", "public_key": "not a key"}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // Empty name is a 400.
    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&session),
            json!({"name": "", "public_key": public_key}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // An over-long input (beyond 16 KiB) is a 400.
    let mut overlong = "ssh-ed25519 ".to_owned();
    overlong.push_str(&"A".repeat(17 * 1024));
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&session),
            json!({"name": "long", "public_key": overlong}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let _ = std::fs::remove_dir_all(rsa_path.parent().unwrap());
    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn duplicate_fingerprint_is_409_across_accounts() {
    let (pool, app) = api_pool().await;
    let alice = tag("alice");
    let bob = tag("bob");
    fixture_account(&pool, &alice, "correct horse battery staple").await;
    fixture_account(&pool, &bob, "correct horse battery staple").await;
    let alice_session = login(&app, &alice, "correct horse battery staple").await;
    let bob_session = login(&app, &bob, "correct horse battery staple").await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);

    let (status, created) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&alice_session),
        json!({"name": "alice key", "public_key": public_key.clone()}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&bob_session),
        json!({"name": "bob key", "public_key": public_key.clone()}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Once revoked, the same key can be registered again.
    let id = created["id"].as_str().unwrap();
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/user/ssh-keys/{id}"),
        Some(&alice_session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&bob_session),
        json!({"name": "bob key", "public_key": public_key}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn access_token_caller_gets_403() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let token = create_token(&app, &session, &["repo:write"]).await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);

    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&token),
        json!({"name": "nested", "public_key": public_key}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/user/ssh-keys",
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/v1/user/ssh-keys/{}", Uuid::new_v4()),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn delete_another_accounts_key_is_404() {
    let (pool, app) = api_pool().await;
    let alice = tag("alice");
    let bob = tag("bob");
    fixture_account(&pool, &alice, "correct horse battery staple").await;
    fixture_account(&pool, &bob, "correct horse battery staple").await;
    let alice_session = login(&app, &alice, "correct horse battery staple").await;
    let bob_session = login(&app, &bob, "correct horse battery staple").await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);

    let (status, value) = request(
        &app,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&alice_session),
        json!({"name": "alice key", "public_key": public_key}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let key_id = value["id"].as_str().unwrap().to_owned();

    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/api/v1/user/ssh-keys/{key_id}"),
        Some(&bob_session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Alice's key still exists.
    let (_, listed) = request(
        &app,
        "GET",
        "/api/v1/user/ssh-keys",
        Some(&alice_session),
        Value::Null,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    pool.close().await;
}

// ---------------------------------------------------------------------------
// authenticate() tests
// ---------------------------------------------------------------------------

async fn insert_key(pool: &PgPool, account_id: Uuid, fingerprint: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ssh_keys (id, account_id, name, algorithm, public_key, fingerprint) \
         VALUES ($1, $2, 'test', 'ed25519', 'ssh-ed25519 stub', $3)",
    )
    .bind(id)
    .bind(account_id)
    .bind(fingerprint)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn authenticate_returns_identity_or_none() {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-sshk-a-{}", Uuid::new_v4())),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let account = fixture_account(&pool, &tag("alice"), "correct horse battery staple").await;
    let fp_valid = format!("SHA256:valid-{}", Uuid::new_v4().simple());
    let fp_revoked = format!("SHA256:revoked-{}", Uuid::new_v4().simple());
    let fp_expired = format!("SHA256:expired-{}", Uuid::new_v4().simple());
    let fp_suspended = format!("SHA256:suspended-{}", Uuid::new_v4().simple());
    let valid_id = insert_key(&pool, account, &fp_valid).await;
    let _unknown = insert_key(&pool, account, &fp_revoked).await;
    sqlx::query("UPDATE ssh_keys SET revoked_at = now() WHERE fingerprint = $1")
        .bind(&fp_revoked)
        .execute(&pool)
        .await
        .unwrap();
    let _unknown = insert_key(&pool, account, &fp_expired).await;
    sqlx::query(
        "UPDATE ssh_keys SET expires_at = now() - interval '1 second' WHERE fingerprint = $1",
    )
    .bind(&fp_expired)
    .execute(&pool)
    .await
    .unwrap();
    let suspended = fixture_account(&pool, &tag("sus"), "correct horse battery staple").await;
    let _suspended_key = insert_key(&pool, suspended, &fp_suspended).await;
    sqlx::query("UPDATE accounts SET suspended = true WHERE id = $1")
        .bind(suspended)
        .execute(&pool)
        .await
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();

    // Valid key resolves to the right account and key_id.
    let identity = ssh_keys::authenticate(&mut conn, &fp_valid)
        .await
        .unwrap()
        .expect("valid key must authenticate");
    assert_eq!(identity.account, account);
    assert_eq!(identity.key_id, valid_id);
    assert!(!identity.read_only);

    // last_used_at was refreshed.
    let last_used: Option<String> = sqlx::query_scalar(
        "SELECT to_char(last_used_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM ssh_keys WHERE id = $1",
    )
    .bind(valid_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(last_used.is_some());

    assert!(
        ssh_keys::authenticate(
            &mut conn,
            &format!("SHA256:unknown-{}", Uuid::new_v4().simple())
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        ssh_keys::authenticate(&mut conn, &fp_revoked)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ssh_keys::authenticate(&mut conn, &fp_expired)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ssh_keys::authenticate(&mut conn, &fp_suspended)
            .await
            .unwrap()
            .is_none()
    );

    pool.close().await;
}
