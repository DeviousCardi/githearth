//! Real PostgreSQL contract test; explicitly opt in with --ignored.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, bootstrap_admin, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tower::ServiceExt;
use uuid::Uuid;

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
async fn login(app: &Router, username: &str, password: &str) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await
}
async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_foundation_contract() {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL to an EMPTY disposable database");
    let config = Config {
        database_url: url,
        database_timeout_seconds: 1,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tables, 0, "test refuses a nonempty database");
    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    let app = router(state);
    assert_eq!(
        request(&app, "GET", "/health/live", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    assert_eq!(count(&pool, "_sqlx_migrations").await, 1);
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE _sqlx_migrations SET success=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("UPDATE _sqlx_migrations SET success=true")
        .execute(&pool)
        .await
        .unwrap();

    let checksum: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1")
        .bind(vec![0u8; 48])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1")
        .bind(checksum)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE sessions RENAME TO missing_sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("ALTER TABLE missing_sessions RENAME TO sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    sqlx::query("INSERT INTO _sqlx_migrations (version,description,success,checksum,execution_time) VALUES (999,'unknown fixture',true,$1,0)")
        .bind(vec![0u8; 48]).execute(&pool).await.unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=999")
        .execute(&pool)
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        bootstrap_admin(&pool, "first-admin", "correct horse battery staple".into()),
        bootstrap_admin(&pool, "first-admin", "correct horse battery staple".into()),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let admin = first.or(second).unwrap();
    assert!(
        bootstrap_admin(&pool, "replacement", "another secure password".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(count(&pool, "accounts").await, 1);
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id=$1")
        .bind(admin)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("correct horse"));
    assert_eq!(
        login(&app, "first-admin", "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&app, "missing-user", "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, token) = login(&app, "first-admin", "correct horse battery staple").await;
    assert_eq!(status, StatusCode::OK);
    let token = token["token"].as_str().unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token.as_bytes());
    let (status, me) = request(&app, "GET", "/api/v1/me", Some(token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"], admin.to_string());
    assert!(me.get("password_hash").is_none());
    assert_eq!(
        request(&app, "GET", "/api/v1/me", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    for malformed in [
        "",
        "short",
        "not-a-hex-token",
        &"g".repeat(64),
        &"0".repeat(64),
    ] {
        assert_eq!(
            request(&app, "GET", "/api/v1/me", Some(malformed), Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    // Account provisioning is an operator fixture; there is no public registration endpoint.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,'other-user',$2)")
        .bind(other)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let (status, other_token) = login(&app, "other-user", "correct horse battery staple").await;
    assert_eq!(status, StatusCode::OK);
    let other_token = other_token["token"].as_str().unwrap();
    let (status, organization) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(other_token),
        json!({"slug":"private-team","name":"Private Team"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(organization["role"], "owner");
    let path = format!(
        "/api/v1/organizations/{}",
        organization["id"].as_str().unwrap()
    );
    assert_eq!(
        request(&app, "GET", &path, Some(other_token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    // Administrator does not imply tenant membership.
    assert_eq!(
        request(&app, "GET", &path, Some(token), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations",
            Some(token),
            Value::Null
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations?limit=1",
            Some(other_token),
            Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations?limit=101",
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(other_token),
        json!({"slug":"private-team","name":"Duplicate"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let owner: Uuid = sqlx::query("SELECT account_id FROM memberships")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("account_id");
    assert_eq!(owner, other);

    for payload in [
        json!({"slug":"","name":"Invalid"}),
        json!({"slug":"UPPERCASE","name":"Invalid"}),
        json!({"slug":"bad/slash","name":"Invalid"}),
        json!({"slug":"a".repeat(65),"name":"Invalid"}),
        json!({"slug":"valid-slug","name":"   "}),
        json!({"slug":"valid-slug","name":"x".repeat(201)}),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/organizations",
                Some(other_token),
                payload
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(count(&pool, "organizations").await, 1);

    let mut expected = vec![organization["id"].as_str().unwrap().to_owned()];
    for slug in ["cursor-second", "cursor-third"] {
        let (status, org) = request(
            &app,
            "POST",
            "/api/v1/organizations",
            Some(other_token),
            json!({"slug":slug,"name":slug}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        expected.push(org["id"].as_str().unwrap().to_owned());
    }
    let (status, admin_org) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(token),
        json!({"slug":"admin-private","name":"Admin private"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        request(
            &app,
            "GET",
            &format!(
                "/api/v1/organizations/{}",
                admin_org["id"].as_str().unwrap()
            ),
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    expected.sort();
    let mut seen = Vec::new();
    let mut cursor = String::new();
    for _ in 0..=expected.len() {
        let path = format!("/api/v1/organizations?limit=1{cursor}");
        let (status, page) = request(&app, "GET", &path, Some(other_token), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let page = page.as_array().unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        let id = page[0]["id"].as_str().unwrap().to_owned();
        cursor = format!("&after={id}");
        seen.push(id);
    }
    assert_eq!(
        seen, expected,
        "cursor pages must have no omissions, duplicates, or foreign organizations"
    );

    let before = [
        count(&pool, "organizations").await,
        count(&pool, "memberships").await,
        count(&pool, "audit_events").await,
        count(&pool, "outbox_events").await,
    ];
    // Fail the final insert, proving all earlier inserts roll back together.
    sqlx::query("ALTER TABLE outbox_events ADD CONSTRAINT forced_failure CHECK (kind <> 'organization.created') NOT VALID").execute(&pool).await.unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/organizations",
            Some(other_token),
            json!({"slug":"must-rollback","name":"Rollback"})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let after = [
        count(&pool, "organizations").await,
        count(&pool, "memberships").await,
        count(&pool, "audit_events").await,
        count(&pool, "outbox_events").await,
    ];
    assert_eq!(before, after);
    sqlx::query("ALTER TABLE outbox_events DROP CONSTRAINT forced_failure")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/logout",
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(other_token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE sessions SET expires_at=clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/login",
            None,
            json!({"username":"first-admin","password":"x".repeat(20000)})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        request(&app, "POST", "/api/v1/register", None, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // Closed pool represents DB dependency outage, without taking down the test runner's service.
    pool.close().await;
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&app, "GET", "/health/live", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}
