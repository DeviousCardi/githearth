//! M4.1 account administration, password lifecycle and failed-login
//! throttling (U01, U02, U03). Requires a disposable PostgreSQL database.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

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

async fn login_status(app: &Router, username: &str, password: &str) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await
}

async fn login(app: &Router, username: &str, password: &str) -> String {
    let (status, value) = login_status(app, username, password).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value["token"].as_str().unwrap().to_owned()
}

async fn fixture_account(pool: &PgPool, username: &str, password: &str, is_admin: bool) -> Uuid {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash,is_admin,password_changed_at) \
         VALUES ($1,$2,$3,$4,clock_timestamp()) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(hash)
    .bind(is_admin)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn temp_storage() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("githearth-accounts-{}", Uuid::new_v4()))
}

async fn pool_and_app(overrides: impl FnOnce(&mut Config)) -> (PgPool, Router, Config) {
    let mut config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: temp_storage(),
        ..Config::default()
    };
    overrides(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config.clone()).await.unwrap());
    (pool, app, config)
}

async fn api_pool() -> (PgPool, Router) {
    let (pool, app, _) = pool_and_app(|_| {}).await;
    (pool, app)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn administrator_creates_disables_and_reenables_accounts() {
    let (pool, app) = api_pool().await;
    let admin_name = tag("admin");
    let admin = fixture_account(&pool, &admin_name, "administrator-secret", true).await;
    let admin_session = login(&app, &admin_name, "administrator-secret").await;

    // The administrator creates an account with a chosen password (no default).
    let member_name = tag("member");
    let (status, created) = request(
        &app,
        "POST",
        "/api/v1/admin/accounts",
        Some(&admin_session),
        json!({"username": member_name, "password": "member-secret-pass"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["username"], member_name);
    assert_eq!(created["is_admin"], false);
    assert_eq!(created["disabled"], false);
    assert!(created.get("password").is_none());
    assert!(created.get("password_hash").is_none());
    let member_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    // No plaintext password is stored.
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = $1")
        .bind(member_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("member-secret-pass"));

    // The new account can sign in.
    let member_session = login(&app, &member_name, "member-secret-pass").await;
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/me",
            Some(&member_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );

    // A non-administrator cannot create, disable, enable or reset.
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/admin/accounts",
        Some(&member_session),
        json!({"username": tag("sneaky"), "password": "another-secret-pass"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{member_id}/disable"),
            Some(&member_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/admin/accounts",
            Some(&member_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // The administrator disables the account.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{member_id}/disable"),
            Some(&admin_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (disabled, disabled_by): (bool, Option<Uuid>) =
        sqlx::query_as("SELECT suspended, disabled_by FROM accounts WHERE id = $1")
            .bind(member_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(disabled);
    assert_eq!(disabled_by, Some(admin));

    // Its existing session and new logins stop working.
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/me",
            Some(&member_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_status(&app, &member_name, "member-secret-pass")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // Re-enable: bookkeeping is cleared and the account can sign in again.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{member_id}/enable"),
            Some(&admin_session),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (suspended, disabled_at): (bool, Option<String>) =
        sqlx::query_as("SELECT suspended, disabled_at::text FROM accounts WHERE id = $1")
            .bind(member_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!suspended);
    assert!(disabled_at.is_none());
    login(&app, &member_name, "member-secret-pass").await;

    // Each transition is audited, with the administrator as the actor.
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM audit_events WHERE actor_id = $1 ORDER BY created_at, id",
    )
    .bind(admin)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(kinds.contains(&"account.created".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"account.disabled".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"account.enabled".to_owned()), "{kinds:?}");
    // The outbox never carries the password.
    let payloads: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE payload::text LIKE '%member-secret-pass%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(payloads, 0);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn account_creation_validates_input_and_uses_no_default_password() {
    let (pool, app) = api_pool().await;
    let admin_name = tag("admin");
    fixture_account(&pool, &admin_name, "administrator-secret", true).await;
    let admin_session = login(&app, &admin_name, "administrator-secret").await;

    for body in [
        json!({"username": "UPPER", "password": "long-enough-password"}),
        json!({"username": "a", "password": "long-enough-password"}),
        json!({"username": tag("short-pw"), "password": "short"}),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/admin/accounts",
                Some(&admin_session),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    // A duplicate username is a conflict, not a second account.
    let name = tag("dupe");
    for expected in [StatusCode::CREATED, StatusCode::CONFLICT] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/admin/accounts",
                Some(&admin_session),
                json!({"username": name, "password": "long-enough-password"})
            )
            .await
            .0,
            expected
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disabling_the_last_administrator_is_refused() {
    // A dedicated database so the last-administrator check is deterministic.
    let url =
        std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let db = format!("githearth_accounts_lastadmin_{}", Uuid::new_v4().simple());
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&maint)
        .await
        .unwrap();
    let config = Config {
        database_url: format!("{origin}/{db}"),
        storage_root: temp_storage(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    let name = tag("admin");
    let admin = fixture_account(&pool, &name, "administrator-secret", true).await;
    let session = login(&app, &name, "administrator-secret").await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{admin}/disable"),
            Some(&session),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // With a second administrator it is allowed.
    let second = tag("admin2");
    let second_id = fixture_account(&pool, &second, "administrator-secret-2", true).await;
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{second_id}/disable"),
            Some(&session),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    // The administrator is the last active one again, so it is refused too.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/admin/accounts/{admin}/disable"),
            Some(&session),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    pool.close().await;
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .execute(&maint)
        .await
        .unwrap();
}
