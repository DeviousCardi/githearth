//! M4.1 U03: bounded, time-window failed-login throttling per account and per
//! client address. Requires a disposable PostgreSQL database.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn login(
    app: &Router,
    username: &str,
    password: &str,
    client: Option<SocketAddr>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"username": username, "password": password}).to_string(),
        ))
        .unwrap();
    if let Some(addr) = client {
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn fixture(overrides: impl FnOnce(&mut Config)) -> (PgPool, Router) {
    let mut config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-throttle-{}", Uuid::new_v4())),
        ..Config::default()
    };
    overrides(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    (pool, app)
}

async fn account(pool: &PgPool, password: &str, is_admin: bool) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let username = tag("user");
    sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(&username)
        .bind(githearth_server::hash_password(password).unwrap())
        .bind(is_admin)
        .execute(pool)
        .await
        .unwrap();
    (id, username)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_logins_are_throttled_per_account_and_recover() {
    let (pool, app) = fixture(|c| {
        c.login_throttle_account_limit = 3;
        c.login_throttle_client_limit = 1000;
        c.login_throttle_window_seconds = 300;
    })
    .await;
    let (id, name) = account(&pool, "correct-horse-battery", false).await;

    // Three failures are accepted as bad credentials.
    for _ in 0..3 {
        assert_eq!(
            login(&app, &name, "wrong-password", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    // The next attempt is refused before the password is considered.
    assert_eq!(
        login(&app, &name, "wrong-password", None).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Even the correct password is refused while the window is active.
    assert_eq!(
        login(&app, &name, "correct-horse-battery", None).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // The lockout expires: age the recorded failures past the window.
    sqlx::query(
        "UPDATE login_failures SET attempted_at = attempted_at - interval '1 hour' \
         WHERE account_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        login(&app, &name, "correct-horse-battery", None).await.0,
        StatusCode::OK
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn administrator_is_not_locked_out_permanently() {
    let (pool, app) = fixture(|c| {
        c.login_throttle_account_limit = 2;
        c.login_throttle_client_limit = 1000;
        c.login_throttle_window_seconds = 300;
    })
    .await;
    let (id, name) = account(&pool, "administrator-secret", true).await;

    for _ in 0..2 {
        assert_eq!(
            login(&app, &name, "wrong-password", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&app, &name, "administrator-secret", None).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Refused attempts are not recorded, so repeated guessing cannot extend the
    // window: once it passes, the administrator can sign in again.
    for _ in 0..5 {
        assert_eq!(
            login(&app, &name, "wrong-password", None).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    sqlx::query(
        "UPDATE login_failures SET attempted_at = attempted_at - interval '1 hour' \
         WHERE account_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        login(&app, &name, "administrator-secret", None).await.0,
        StatusCode::OK
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_logins_are_throttled_per_client_address() {
    let (pool, app) = fixture(|c| {
        c.login_throttle_account_limit = 2;
        c.login_throttle_client_limit = 2;
        c.login_throttle_window_seconds = 300;
    })
    .await;
    let (_id, name) = account(&pool, "victim-secret-pass", false).await;
    // Unique addresses per run: the test database is shared, so a fixed
    // address would inherit failures left by an earlier run.
    let salt = Uuid::new_v4();
    let attacker: SocketAddr =
        format!("198.18.{}.{}:40000", salt.as_bytes()[0], salt.as_bytes()[1])
            .parse()
            .unwrap();
    let other: SocketAddr = format!("198.18.{}.{}:40000", salt.as_bytes()[2], salt.as_bytes()[3])
        .parse()
        .unwrap();

    // Two failures from one address, spread across unknown usernames.
    for i in 0..2 {
        assert_eq!(
            login(
                &app,
                &tag(&format!("nobody{i}")),
                "guessing-guesses",
                Some(attacker)
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // The same address is refused a third time, whatever the username.
    assert_eq!(
        login(&app, &name, "victim-secret-pass", Some(attacker))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A different address is still evaluated normally (bad password → 401).
    assert_eq!(
        login(&app, &name, "wrong-password", Some(other)).await.0,
        StatusCode::UNAUTHORIZED
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_successful_login_clears_the_accounts_failures() {
    let (pool, app) = fixture(|c| {
        c.login_throttle_account_limit = 3;
        c.login_throttle_client_limit = 1000;
        c.login_throttle_window_seconds = 300;
    })
    .await;
    let (id, name) = account(&pool, "correct-horse-battery", false).await;
    for _ in 0..2 {
        assert_eq!(
            login(&app, &name, "wrong-password", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&app, &name, "correct-horse-battery", None).await.0,
        StatusCode::OK
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM login_failures WHERE account_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    pool.close().await;
}

#[test]
fn throttle_defaults_are_safe_and_bounded() {
    let config = Config::default();
    assert_eq!(config.login_throttle_account_limit, 5);
    assert_eq!(config.login_throttle_client_limit, 20);
    assert_eq!(config.login_throttle_window_seconds, 900);
    let mut config = Config {
        database_url: "postgres://localhost/test".into(),
        ..Config::default()
    };
    assert!(config.validate().is_ok());
    config.login_throttle_account_limit = 0;
    assert!(config.validate().is_err());
    config.login_throttle_account_limit = 50;
    config.login_throttle_client_limit = 10;
    assert!(
        config.validate().is_err(),
        "the client limit must not be below the account limit"
    );
    config.login_throttle_client_limit = 100;
    config.login_throttle_window_seconds = 0;
    assert!(config.validate().is_err());
}
