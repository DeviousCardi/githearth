//! Commit status API contract tests (PostgreSQL-backed).
//!
//! Statuses are posted against a repository and a commit id, keyed by context.
//! These tests exercise the JSON API directly (post, list, authorization,
//! validation, idempotency); the merge gate built on top of them is covered by
//! the merge intent/execute tests.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
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

async fn fixture_read_only_token(pool: &PgPool, account: Uuid) -> String {
    let token = format!("gth_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO access_tokens (id, account_id, name, token_hash, scopes) \
         VALUES ($1, $2, $3, sha256($4::bytea), $5)",
    )
    .bind(Uuid::new_v4())
    .bind(account)
    .bind("read-only")
    .bind(&token)
    .bind(vec!["repo:read".to_owned()])
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

async fn fixture_repository(pool: &PgPool, org_id: Uuid, name: &str, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, archived) \
         VALUES ($1, $2, $3, $4, $5, 'active', 'private', false)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(owner)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn add_member(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let account = fixture_account(pool, &tag("member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    account
}

/// An organization member with no grant: not authorized to read repositories.
async fn add_grantless_member(pool: &PgPool, org_id: Uuid) -> Uuid {
    let account = fixture_account(pool, &tag("member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    account
}

async fn setup() -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-status-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        body_limit_bytes: 200_000,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, storage_root, owner, org_id, org_slug)
}

fn cleanup(storage_root: &PathBuf) {
    let _ = std::fs::remove_dir_all(storage_root);
}

fn statuses_path(org: &str, repo: &str, sha: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/commits/{sha}/statuses")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn post_and_list_statuses_append_per_context() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "r", owner).await;
    let sha = "a".repeat(40);
    let base = statuses_path(&slug, "r", &sha);

    // Empty list.
    let (status, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Post a pending status.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "ci", "state": "pending", "description": "running", "target_url": "https://ci.example/1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["context"], "ci");
    assert_eq!(body["state"], "pending");
    assert_eq!(body["commit_id"], sha);
    assert_eq!(body["description"], "running");
    assert_eq!(body["target_url"], "https://ci.example/1");

    // A second result for the same context is appended, not replaced.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Newest first.
    assert_eq!(items[0]["state"], "success");
    assert_eq!(items[1]["state"], "pending");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn write_role_posts_and_read_role_cannot() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "r", owner).await;
    let base = statuses_path(&slug, "r", &"b".repeat(40));

    // read role: listing is fine, posting is not.
    let reader = add_member(&pool, org_id, "read").await;
    let reader_token = fixture_session(&pool, reader).await;
    let (status, _) = request(&app, "GET", &base, Some(&reader_token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&reader_token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // write role: posting succeeds.
    let writer = add_member(&pool, org_id, "write").await;
    let writer_token = fixture_session(&pool, writer).await;
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&writer_token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // owner still posts.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "lint", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_member_without_a_read_grant_cannot_list_or_post() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "priv", owner).await;
    let base = statuses_path(&slug, "priv", &"f".repeat(40));
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A member with no grant can neither list nor post.
    let stranger = add_grantless_member(&pool, org_id).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    let (status, _) = request(&app, "GET", &base, Some(&stranger_token), None, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&stranger_token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_statuses_are_rejected() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "r", owner).await;

    // The commit id must be a full lowercase hex id.
    for sha in ["main", &"A".repeat(40), &"a".repeat(39), &"g".repeat(40)] {
        let base = statuses_path(&slug, "r", sha);
        let (status, _) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"context": "ci", "state": "success"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "sha={sha:?}");
    }

    let base = statuses_path(&slug, "r", &"c".repeat(40));
    for body in [
        json!({"context": "ci", "state": "running"}),
        json!({"context": "", "state": "success"}),
        json!({"context": "ci"}),
        json!({"context": "ci", "state": "success", "extra": 1}),
        json!({"context": "githearth/legacy-require-checks", "state": "success"}),
    ] {
        let (status, _) = request(&app, "POST", &base, Some(&token), None, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // target_url must be http/https; anything else is 422.
    for url in [
        "javascript:alert(1)",
        "ftp://ci.example/1",
        "data:text/html,hello",
    ] {
        let (status, _) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"context": "ci", "state": "success", "target_url": url}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "target_url={url}");
    }
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "ci", "state": "success", "target_url": "https://ci.example/1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_token_cannot_post_status() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    fixture_repository(&pool, org_id, "ro", owner).await;
    let base = statuses_path(&slug, "ro", &"e".repeat(40));

    let token = fixture_read_only_token(&pool, owner).await;

    // A read-only scoped token can still list statuses.
    let (status, _) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);

    // Posting a status (status.create) is a write and must be refused.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_replay() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "i", owner).await;
    let base = statuses_path(&slug, "i", &"d".repeat(40));
    let key = tag("idem");
    let body = json!({"context": "ci", "state": "success"});

    let (status, r1) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status2, r2) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(r1, r2);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM commit_statuses WHERE repository_id = (SELECT id FROM repositories WHERE organization_id=$1 AND name='i')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage);
    pool.close().await;
}
