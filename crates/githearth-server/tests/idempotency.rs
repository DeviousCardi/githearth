use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, idempotency, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    header_key: Option<(&str, &str)>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some((k, v)) = header_key {
        builder = builder.header(k, v);
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

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn fixture_account(pool: &PgPool, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture_session(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
    token
}

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(id)
        .bind(&slug)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    (id, slug)
}

async fn setup() -> (Router, PgPool, PathBuf, Uuid, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-idem-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    (router(state), pool, storage_root, owner, org_slug)
}

fn cleanup(storage_root: &PathBuf) {
    let _ = std::fs::remove_dir_all(storage_root);
}

/// An abandoned `running` claim (older than the lease) whose create already
/// committed the `creating` row is re-claimed: the retry reuses the same
/// repository row instead of creating a second one.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn abandoned_create_reclaimed_reuses_existing_row() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;

    let key = tag("idem-crash");
    let repo_name = tag("crash-repo");
    let operation_id = Uuid::new_v4();
    let repo_id = Uuid::new_v4();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();

    // First attempt committed the `creating` row and recorded its operation id,
    // then crashed before the mark-active + store transaction.
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility) \
         VALUES ($1,$2,$3,$4,$5,'creating','private')",
    )
    .bind(repo_id)
    .bind(org_id)
    .bind(&repo_name)
    .bind(owner)
    .bind(operation_id)
    .execute(&pool)
    .await
    .unwrap();

    let path = format!("/api/v1/organizations/{org_slug}/repositories");
    let body = json!({"name": &repo_name});
    let hash = idempotency::request_hash("POST", &path, body.to_string().as_bytes());
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, operation_id, expires_at, created_at) \
         VALUES ($1,$2,$3,'running',$4,clock_timestamp() + interval '24 hours',clock_timestamp() - interval '120 seconds')",
    )
    .bind(owner)
    .bind(&key)
    .bind(&hash)
    .bind(operation_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, resp) = request(
        &app,
        "POST",
        &path,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        resp["id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        repo_id
    );

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(org_id)
    .bind(&repo_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage_root);
    pool.close().await;
}

/// An abandoned `running` claim with no recorded operation id (crash before the
/// first commit) is re-claimed and the create runs from scratch exactly once.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn abandoned_create_without_row_runs_fresh() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;

    let key = tag("idem-early");
    let repo_name = tag("early-repo");
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();

    let path = format!("/api/v1/organizations/{org_slug}/repositories");
    let body = json!({"name": &repo_name});
    let hash = idempotency::request_hash("POST", &path, body.to_string().as_bytes());
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, expires_at, created_at) \
         VALUES ($1,$2,$3,'running',clock_timestamp() + interval '24 hours',clock_timestamp() - interval '120 seconds')",
    )
    .bind(owner)
    .bind(&key)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = request(
        &app,
        "POST",
        &path,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repositories WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(org_id)
    .bind(&repo_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage_root);
    pool.close().await;
}

/// A `running` claim within the lease is not re-claimed: the retry sees 409.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fresh_running_claim_conflicts() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;

    let key = tag("idem-fresh");
    let path = format!("/api/v1/organizations/{org_slug}/repositories");
    let body = json!({"name": "fresh-conflict"});
    let hash = idempotency::request_hash("POST", &path, body.to_string().as_bytes());
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, expires_at) \
         VALUES ($1,$2,$3,'running',clock_timestamp() + interval '24 hours')",
    )
    .bind(owner)
    .bind(&key)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let (status, _) = request(
        &app,
        "POST",
        &path,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    cleanup(&storage_root);
    pool.close().await;
}
