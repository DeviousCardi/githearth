use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Connection, PgPool};
use std::path::PathBuf;
use std::time::Duration;
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

async fn fixture_repository(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    owner: Uuid,
    visibility: &str,
    archived: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, archived) \
         VALUES ($1, $2, $3, $4, $5, 'active', $6, $7)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(owner)
    .bind(Uuid::new_v4())
    .bind(visibility)
    .bind(archived)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn setup() -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    setup_with_connections(4).await
}

async fn setup_with_connections(
    max_connections: u32,
) -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    setup_with_pool(max_connections, 1).await
}

async fn setup_with_pool(
    max_connections: u32,
    acquire_timeout_seconds: u64,
) -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-issues-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: max_connections,
        database_timeout_seconds: acquire_timeout_seconds,
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

fn issues_path(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/issues")
}

async fn create_issue(
    app: &Router,
    token: &str,
    org: &str,
    repo: &str,
    title: &str,
    body: &str,
) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &issues_path(org, repo),
        Some(token),
        None,
        json!({"title": title, "body": body}),
    )
    .await
}

async fn add_member(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let account = fixture_account(pool, &tag("member")).await;
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,$3)")
        .bind(org_id)
        .bind(account)
        .bind(role)
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

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_and_read_issue() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "r", owner, "private", false).await;
    let repo = "r";

    let (status, body) = create_issue(&app, &token, &slug, repo, "hello", "world").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["number"], 1);
    assert_eq!(body["title"], "hello");
    assert_eq!(body["body"], "world");
    assert_eq!(body["state"], "open");
    assert_eq!(body["author_id"], owner.to_string());
    assert_eq!(body["closed_at"], Value::Null);

    let (status, body) = request(
        &app,
        "GET",
        &format!("{}/1", issues_path(&slug, repo)),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "hello");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn per_repository_numbering() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "a", owner, "private", false).await;
    fixture_repository(&pool, org_id, "b", owner, "private", false).await;

    for i in 1..=3 {
        let (_, body) = create_issue(&app, &token, &slug, "a", &format!("a{i}"), "").await;
        assert_eq!(body["number"], i);
    }
    let (_, body) = create_issue(&app, &token, &slug, "b", "b1", "").await;
    assert_eq!(body["number"], 1);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn list_paginates_by_number_desc() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "p", owner, "private", false).await;

    for i in 1..=5 {
        create_issue(&app, &token, &slug, "p", &format!("t{i}"), "").await;
    }

    // Default state=open, ordered by number DESC.
    let (status, body) = request(
        &app,
        "GET",
        &format!("{}?limit=2", issues_path(&slug, "p")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["number"], 5);
    assert_eq!(items[1]["number"], 4);
    assert_eq!(body["next"], 4);

    let (_, body) = request(
        &app,
        "GET",
        &format!("{}?limit=2&after=4", issues_path(&slug, "p")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["number"], 3);
    assert_eq!(items[1]["number"], 2);
    assert_eq!(body["next"], 2);

    let (_, body) = request(
        &app,
        "GET",
        &format!("{}?limit=2&after=2", issues_path(&slug, "p")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 1);
    assert_eq!(body["next"], Value::Null);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn state_filter_defaults_open_and_lists_all() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "s", owner, "private", false).await;

    create_issue(&app, &token, &slug, "s", "open1", "").await;
    let (_, created) = create_issue(&app, &token, &slug, "s", "close1", "").await;
    let number = created["number"].as_i64().unwrap();

    request(
        &app,
        "PATCH",
        &format!("{}/{number}", issues_path(&slug, "s")),
        Some(&token),
        None,
        json!({"state": "closed"}),
    )
    .await;

    // Default open excludes the closed issue.
    let (_, body) = request(
        &app,
        "GET",
        &issues_path(&slug, "s"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let (_, body) = request(
        &app,
        "GET",
        &format!("{}?state=all", issues_path(&slug, "s")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    let (_, body) = request(
        &app,
        "GET",
        &format!("{}?state=closed", issues_path(&slug, "s")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn close_and_reopen() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "c", owner, "private", false).await;
    create_issue(&app, &token, &slug, "c", "x", "").await;

    let path = format!("{}/1", issues_path(&slug, "c"));
    let (status, body) = request(
        &app,
        "PATCH",
        &path,
        Some(&token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "closed");
    assert!(body["closed_at"].is_string());

    let (status, body) = request(
        &app,
        "PATCH",
        &path,
        Some(&token),
        None,
        json!({"state": "open"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "open");
    assert_eq!(body["closed_at"], Value::Null);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn edit_title_and_body() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "e", owner, "private", false).await;
    create_issue(&app, &token, &slug, "e", "original", "").await;

    let (status, body) = request(
        &app,
        "PATCH",
        &format!("{}/1", issues_path(&slug, "e")),
        Some(&token),
        None,
        json!({"title": "edited", "body": "new body"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "edited");
    assert_eq!(body["body"], "new body");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn comment_pagination_oldest_first() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "m", owner, "private", false).await;
    create_issue(&app, &token, &slug, "m", "x", "").await;

    let base = format!("{}/1/comments", issues_path(&slug, "m"));
    for i in 0..5 {
        let (status, body) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"body": format!("comment {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["seq"], i + 1);
    }

    let (status, body) = request(
        &app,
        "GET",
        &format!("{base}?limit=2"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["body"], "comment 0");
    assert_eq!(items[1]["body"], "comment 1");
    let next = body["next"].as_i64().unwrap().to_string();

    let (_, body) = request(
        &app,
        "GET",
        &format!("{base}?limit=2&after={next}"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["body"], "comment 2");
    assert_eq!(items[1]["body"], "comment 3");
    let next = body["next"].as_i64().unwrap().to_string();
    let (_, body) = request(
        &app,
        "GET",
        &format!("{base}?limit=2&after={next}"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"][0]["body"], "comment 4");
    assert_eq!(body["next"], Value::Null);

    // A cursor that is not a positive integer is rejected, not answered with an empty page.
    for bad in ["0", "-1", "abc", "1.5"] {
        let (status, _) = request(
            &app,
            "GET",
            &format!("{base}?after={bad}"),
            Some(&token),
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "after={bad}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_user_404() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "priv", owner, "private", false).await;
    create_issue(&app, &token, &slug, "priv", "secret", "").await;

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;

    for (method, path) in [
        ("GET", format!("{}/1", issues_path(&slug, "priv"))),
        ("GET", issues_path(&slug, "priv")),
        ("POST", issues_path(&slug, "priv")),
        ("PATCH", format!("{}/1", issues_path(&slug, "priv"))),
        ("GET", format!("{}/1/comments", issues_path(&slug, "priv"))),
    ] {
        let (status, _) = request(
            &app,
            method,
            &path,
            Some(&stranger_token),
            None,
            if method == "POST" || method == "PATCH" {
                json!({"title": "nope"})
            } else {
                Value::Null
            },
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_member_cannot_create_403() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;
    // Internal repo: any organization member can read, but only read+ roles create.
    fixture_repository(&pool, org_id, "shared", owner, "internal", false).await;

    let member = add_member(&pool, org_id, "member").await;
    let member_token = fixture_session(&pool, member).await;

    let (status, _) = request(
        &app,
        "GET",
        &issues_path(&slug, "shared"),
        Some(&member_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = create_issue(&app, &member_token, &slug, "shared", "nope", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The owner can still create.
    let (status, _) = create_issue(&app, &owner_token, &slug, "shared", "ok", "").await;
    assert_eq!(status, StatusCode::CREATED);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archived_repo_reads_work_mutations_403() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "arc", owner, "private", true).await;

    // Creating an issue is refused by the engine.
    let (status, _) = create_issue(&app, &token, &slug, "arc", "nope", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Seed an issue directly for read/comment/edit checks.
    let issue_id = Uuid::new_v4();
    sqlx::query("INSERT INTO issues (id, repository_id, number, title, body, author_id) VALUES ($1, (SELECT id FROM repositories WHERE organization_id=$2 AND name='arc'), 1, 'seed', '', $3)")
        .bind(issue_id)
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

    // Reading still works.
    let (status, _) = request(
        &app,
        "GET",
        &format!("{}/1", issues_path(&slug, "arc")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Commenting is refused (archived-no-new-content).
    let (status, _) = request(
        &app,
        "POST",
        &format!("{}/1/comments", issues_path(&slug, "arc")),
        Some(&token),
        None,
        json!({"body": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Editing is refused (archived-read-only via issue.close).
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("{}/1", issues_path(&slug, "arc")),
        Some(&token),
        None,
        json!({"title": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_issue_replay() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "i", owner, "private", false).await;

    let key = tag("idem");
    let (status, body) = request(
        &app,
        "POST",
        &issues_path(&slug, "i"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"title": "dup", "body": "b"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, body2) = request(
        &app,
        "POST",
        &issues_path(&slug, "i"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"title": "dup", "body": "b"}),
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(body, body2);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issues WHERE repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 'i')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_comment_replay() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "j", owner, "private", false).await;
    create_issue(&app, &token, &slug, "j", "x", "").await;

    let base = format!("{}/1/comments", issues_path(&slug, "j"));
    let key = tag("idem-c");
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"body": "hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, body2) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"body": "hello"}),
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(body, body2);

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issue_comments WHERE issue_id = (SELECT i.id FROM issues i JOIN repositories r ON r.id = i.repository_id WHERE r.organization_id = $1 AND r.name = 'j' AND i.number = 1)",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_input_400() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "v", owner, "private", false).await;
    create_issue(&app, &token, &slug, "v", "ok", "").await;

    let cases: Vec<(&str, String, Value)> = vec![
        // Empty title
        ("POST", issues_path(&slug, "v"), json!({"title": ""})),
        // Whitespace-only title (create)
        ("POST", issues_path(&slug, "v"), json!({"title": "  "})),
        // Whitespace-only title in a patch: the API must match create and the UI.
        (
            "PATCH",
            format!("{}/1", issues_path(&slug, "v")),
            json!({"title": " "}),
        ),
        // Oversize title
        (
            "POST",
            issues_path(&slug, "v"),
            json!({"title": "x".repeat(257)}),
        ),
        // Oversize body
        (
            "POST",
            issues_path(&slug, "v"),
            json!({"title": "ok", "body": "x".repeat(65537)}),
        ),
        // Bad state query
        (
            "GET",
            format!("{}?state=bogus", issues_path(&slug, "v")),
            Value::Null,
        ),
        // Bad limit
        (
            "GET",
            format!("{}?limit=0", issues_path(&slug, "v")),
            Value::Null,
        ),
        (
            "GET",
            format!("{}?limit=101", issues_path(&slug, "v")),
            Value::Null,
        ),
        // Bad state in patch
        (
            "PATCH",
            format!("{}/1", issues_path(&slug, "v")),
            json!({"state": "bogus"}),
        ),
        // Empty patch
        ("PATCH", format!("{}/1", issues_path(&slug, "v")), json!({})),
        // Empty comment
        (
            "POST",
            format!("{}/1/comments", issues_path(&slug, "v")),
            json!({"body": ""}),
        ),
    ];

    for (method, path, body) in cases {
        let (status, _) = request(&app, method, &path, Some(&token), None, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn issue_trigger_violations() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "t", owner, "private", false).await;
    create_issue(&app, &token, &slug, "t", "x", "").await;

    let issue_id: Uuid =
        sqlx::query_scalar("SELECT id FROM issues WHERE number = 1 AND repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 't')")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // Immutable columns raise a check_violation (23514).
    for (col, val) in [("number", "999"), ("author_id", "gen_random_uuid()::uuid")] {
        let err = sqlx::query(&format!("UPDATE issues SET {col} = {val} WHERE id = $1"))
            .bind(issue_id)
            .execute(&pool)
            .await
            .unwrap_err();
        let code = err
            .as_database_error()
            .unwrap()
            .code()
            .map(|c| c.to_string());
        assert_eq!(code, Some("23514".into()), "immutable {col}");
    }

    // closed_at tracks state: closing sets it, reopening clears it.
    sqlx::query("UPDATE issues SET state = 'closed' WHERE id = $1")
        .bind(issue_id)
        .execute(&pool)
        .await
        .unwrap();
    let closed_at: Option<String> =
        sqlx::query_scalar("SELECT closed_at::text FROM issues WHERE id = $1")
            .bind(issue_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(closed_at.is_some());

    sqlx::query("UPDATE issues SET state = 'open' WHERE id = $1")
        .bind(issue_id)
        .execute(&pool)
        .await
        .unwrap();
    let closed_at: Option<String> =
        sqlx::query_scalar("SELECT closed_at::text FROM issues WHERE id = $1")
            .bind(issue_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(closed_at.is_none());

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn missing_issue_404() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "n", owner, "private", false).await;

    for (method, path, body) in [
        (
            "GET",
            format!("{}/99", issues_path(&slug, "n")),
            Value::Null,
        ),
        (
            "PATCH",
            format!("{}/99", issues_path(&slug, "n")),
            json!({"title": "x"}),
        ),
        (
            "POST",
            format!("{}/99/comments", issues_path(&slug, "n")),
            json!({"body": "x"}),
        ),
    ] {
        let (status, _) = request(&app, method, &path, Some(&token), None, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn edits_need_an_active_repository() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo = fixture_repository(&pool, org_id, "going", owner, "private", false).await;
    create_issue(&app, &token, &slug, "going", "x", "").await;
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("{}/1", issues_path(&slug, "going"));
    for patch in [
        json!({"title": "y"}),
        json!({"body": "z"}),
        json!({"state": "closed"}),
    ] {
        let (status, body) = request(&app, "PATCH", &path, Some(&token), None, patch).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_patches_do_not_lose_updates() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "cc", owner, "private", false).await;
    create_issue(&app, &token, &slug, "cc", "original", "body").await;

    // Concurrent PATCHes: one renames, one closes. Both must survive; exactly one issue.closed.
    // Hold the issue row lock first so both PATCH transactions block on their
    // own FOR UPDATE, forcing them to genuinely overlap and serialize on the
    // row, rather than racing to see which happens to run first.
    let mut gate = pool.acquire().await.unwrap();
    let issue_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM issues WHERE repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 'cc') AND number = 1",
    )
    .bind(org_id)
    .fetch_one(&mut *gate)
    .await
    .unwrap();
    let mut gate_tx = gate.begin().await.unwrap();
    sqlx::query("SELECT id FROM issues WHERE id = $1 FOR UPDATE")
        .bind(issue_id)
        .execute(&mut *gate_tx)
        .await
        .unwrap();

    let app1 = app.clone();
    let app2 = app.clone();
    let token1 = token.clone();
    let token2 = token.clone();
    let path = format!("{}/1", issues_path(&slug, "cc"));
    let (path1, path2) = (path.clone(), path);
    let mut f1 = tokio::spawn(async move {
        request(
            &app1,
            "PATCH",
            &path1,
            Some(&token1),
            None,
            json!({"title": "renamed"}),
        )
        .await
    });
    let mut f2 = tokio::spawn(async move {
        request(
            &app2,
            "PATCH",
            &path2,
            Some(&token2),
            None,
            json!({"state": "closed"}),
        )
        .await
    });

    // Give both tasks a chance to reach their FOR UPDATE and block on it.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    // Both PATCHes are blocked on the held row lock: neither completes within a short window.
    let f1_blocked = tokio::time::timeout(Duration::from_millis(300), &mut f1)
        .await
        .is_err();
    let f2_blocked = tokio::time::timeout(Duration::from_millis(300), &mut f2)
        .await
        .is_err();
    // Release the lock and let both proceed.
    gate_tx.commit().await.unwrap();
    drop(gate);

    let (s1, _) = f1.await.unwrap();
    let (s2, _) = f2.await.unwrap();
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);

    let (_, body) = request(
        &app,
        "GET",
        &format!("{}/1", issues_path(&slug, "cc")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["title"], "renamed");
    assert_eq!(body["state"], "closed");

    // Exactly one issue.closed event, not a follow-up issue.updated overwriting the state.
    let closed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind = 'issue.closed'",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(closed, 1);

    // The lock was actually contended: both PATCHes blocked on the row lock.
    assert!(
        f1_blocked && f2_blocked,
        "expected both PATCHes to block on the held row lock"
    );

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_comments_page_without_skips_or_duplicates() {
    // 20 posts queue for 4 connections while their transactions serialize on
    // the issue's counter row; allow the queue more than the default 1 s.
    let (app, pool, storage, owner, org_id, slug) = setup_with_pool(4, 10).await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "mp", owner, "private", false).await;
    create_issue(&app, &token, &slug, "mp", "x", "").await;

    let base = format!("{}/1/comments", issues_path(&slug, "mp"));
    // Concurrent comment POSTs: the per-issue sequence must be allocated
    // gap-free so the commits are ordered 1..N with no skips or duplicates.
    let mut handles = Vec::new();
    for i in 0..20 {
        let app = app.clone();
        let token = token.clone();
        let base = base.clone();
        handles.push(tokio::spawn(async move {
            let (status, _) = request(
                &app,
                "POST",
                &base,
                Some(&token),
                None,
                json!({"body": format!("c{i}")}),
            )
            .await;
            status
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), StatusCode::CREATED);
    }

    // Page through all comments; the sequence column must be exactly 1..=20.
    let seqs: Vec<i32> = sqlx::query_scalar(
        "SELECT seq FROM issue_comments WHERE issue_id = (SELECT id FROM issues WHERE repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 'mp') AND number = 1) ORDER BY seq",
    )
    .bind(org_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
    assert_eq!(sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM issue_comments WHERE issue_id = (SELECT id FROM issues WHERE repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 'mp') AND number = 1)",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap(), 20);

    // The API pages them back in exact sequence order with a numeric cursor.
    let mut after: Option<i32> = None;
    let mut seen = Vec::new();
    loop {
        let mut url = format!("{base}?limit=3");
        if let Some(a) = after {
            url.push_str(&format!("&after={a}"));
        }
        let (status, body) = request(&app, "GET", &url, Some(&token), None, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap().clone();
        for it in &items {
            seen.push(it["seq"].as_i64().unwrap() as i32);
        }
        match body["next"].as_i64() {
            Some(n) => after = Some(n as i32),
            None => break,
        }
    }
    assert_eq!(seen, (1..=20).collect::<Vec<_>>());

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn new_comment_bumps_issue_updated_at() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo_id = fixture_repository(&pool, org_id, "up", owner, "private", false).await;
    create_issue(&app, &token, &slug, "up", "x", "").await;

    // Backdate the issue so any forward movement is attributable to the comment.
    sqlx::query("UPDATE issues SET updated_at = now() - interval '1 hour' WHERE repository_id = $1 AND number = 1")
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();
    let before: String = sqlx::query_scalar(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM issues WHERE repository_id = $1 AND number = 1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let base = format!("{}/1/comments", issues_path(&slug, "up"));
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"body": "bump"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let after: String = sqlx::query_scalar(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM issues WHERE repository_id = $1 AND number = 1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(
        after, before,
        "a new comment must bump the issue's updated_at"
    );

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reads_of_non_active_repository_404() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo = fixture_repository(&pool, org_id, "nonact", owner, "private", false).await;
    create_issue(&app, &token, &slug, "nonact", "x", "").await;

    // A non-active repository reads like a missing one: 404.
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();

    for path in [
        format!("{}/1", issues_path(&slug, "nonact")),
        issues_path(&slug, "nonact"),
        format!("{}/1/comments", issues_path(&slug, "nonact")),
    ] {
        let (status, _) = request(&app, "GET", &path, Some(&token), None, Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }

    // An archived active repository stays readable.
    let arc = fixture_repository(&pool, org_id, "arcact", owner, "private", true).await;
    sqlx::query("INSERT INTO issues (id, repository_id, number, title, body, author_id) VALUES ($1, $2, 1, 'seed', '', $3)")
        .bind(Uuid::new_v4())
        .bind(arc)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = request(
        &app,
        "GET",
        &format!("{}/1", issues_path(&slug, "arcact")),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    cleanup(&storage);
    pool.close().await;
}

/// A mutation whose response store fails must roll back the whole transaction
/// (business data, audit event and outbox), not commit a response-less write.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn store_failure_rolls_back_mutation_audit_and_outbox() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo = fixture_repository(&pool, org_id, "boom", owner, "private", false).await;

    // Narrowly-targeted trigger: force only this key's response store to fail.
    let key = tag("idem-boom");
    let fn_name = format!("fn_{}", Uuid::new_v4().simple());
    let trg_name = format!("trg_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE FUNCTION {fn_name}() RETURNS trigger AS $$ \
         BEGIN \
           IF NEW.status = 'done' AND NEW.key = $1 THEN \
             RAISE EXCEPTION 'forced' USING ERRCODE = 'check_violation'; \
           END IF; \
           RETURN NEW; \
         END $$ LANGUAGE plpgsql"
    ))
    .bind(&key)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trg_name} BEFORE UPDATE ON operations FOR EACH ROW EXECUTE FUNCTION {fn_name}()"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let before_audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind = 'issue.created'",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (status, _) = request(
        &app,
        "POST",
        &issues_path(&slug, "boom"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"title": "forced", "body": ""}),
    )
    .await;
    assert!(
        status.is_server_error(),
        "store failure should 5xx, got {status}"
    );

    let issues: i64 = sqlx::query_scalar("SELECT count(*) FROM issues WHERE repository_id = $1")
        .bind(repo)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        issues, 0,
        "mutation committed despite failed response store"
    );
    let after_audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind = 'issue.created'",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after_audit, before_audit, "audit event not rolled back");
    let outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE organization_id = $1 AND kind = 'issue.created'",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outbox, 0, "outbox event not rolled back");

    sqlx::query(&format!("DROP TRIGGER {trg_name} ON operations"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {fn_name}()"))
        .execute(&pool)
        .await
        .unwrap();

    cleanup(&storage);
    pool.close().await;
}

/// A keyed create replay works even when the pool allows a single connection.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn keyed_create_replay_with_single_connection() {
    let (app, pool, storage, owner, org_id, slug) = setup_with_connections(1).await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "one", owner, "private", false).await;

    let key = tag("idem-1conn");
    let (status, body) = request(
        &app,
        "POST",
        &issues_path(&slug, "one"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"title": "a", "body": "b"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, body2) = request(
        &app,
        "POST",
        &issues_path(&slug, "one"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"title": "a", "body": "b"}),
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(body, body2);

    cleanup(&storage);
    pool.close().await;
}

/// A keyed comment replay works even when the pool allows a single connection.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn keyed_comment_replay_with_single_connection() {
    let (app, pool, storage, owner, org_id, slug) = setup_with_connections(1).await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "onec", owner, "private", false).await;
    create_issue(&app, &token, &slug, "onec", "x", "").await;

    let base = format!("{}/1/comments", issues_path(&slug, "onec"));
    let key = tag("idem-c1");
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"body": "hello"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, body2) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"body": "hello"}),
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(body, body2);

    cleanup(&storage);
    pool.close().await;
}

/// A delete that commits while a mutation is in flight must win: the mutation
/// re-checks the status under a row lock inside its own transaction.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mutations_lose_to_a_delete_committed_mid_request() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    // `deleting` cannot go back to `active`, so every case gets its own repository.
    let cases: [(&str, &str, Value); 6] = [
        ("POST", "issues", json!({"title": "late"})),
        ("PATCH", "issues/1", json!({"title": "late"})),
        ("POST", "issues/1/comments", json!({"body": "late"})),
        ("PATCH", "", json!({"description": "late"})),
        ("POST", "archive", json!(null)),
        (
            "POST",
            "access",
            json!({"subject_kind": "account", "subject_id": owner, "role": "read"}),
        ),
    ];
    for (i, (method, suffix, body)) in cases.into_iter().enumerate() {
        let name = format!("racing-{i}");
        let repo = fixture_repository(&pool, org_id, &name, owner, "private", false).await;
        create_issue(&app, &token, &slug, &name, "x", "").await;
        let base = format!("/api/v1/organizations/{slug}/repositories/{name}");
        let path = if suffix.is_empty() {
            base
        } else {
            format!("{base}/{suffix}")
        };
        // Hold an uncommitted delete: the handler's pre-check still sees `active`.
        let mut deleter = pool.begin().await.unwrap();
        sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
            .bind(repo)
            .execute(&mut *deleter)
            .await
            .unwrap();
        let call = tokio::spawn({
            let app = app.clone();
            let token = token.clone();
            async move { request(&app, method, &path, Some(&token), None, body).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        deleter.commit().await.unwrap();
        let (status, body) = call.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}: {body}");
        let issues: i64 =
            sqlx::query_scalar("SELECT count(*) FROM issues WHERE repository_id = $1")
                .bind(repo)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(issues, 1);
    }
    cleanup(&storage);
    pool.close().await;
}

/// Two writes to one repository row must serialize, not deadlock: an outside
/// share lock lines both requests up on the row before either can proceed.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_repository_writes_serialize_without_deadlock() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo = fixture_repository(&pool, org_id, "busy", owner, "private", false).await;
    let base = format!("/api/v1/organizations/{slug}/repositories/busy");
    for pair in [
        [
            ("PATCH", base.clone(), json!({"description": "one"})),
            ("PATCH", base.clone(), json!({"description": "two"})),
        ],
        [
            ("POST", format!("{base}/archive"), json!(null)),
            ("POST", format!("{base}/unarchive"), json!(null)),
        ],
    ] {
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM repositories WHERE id = $1 FOR SHARE")
            .bind(repo)
            .execute(&mut *holder)
            .await
            .unwrap();
        let calls: Vec<_> = pair
            .into_iter()
            .map(|(method, path, body)| {
                let app = app.clone();
                let token = token.clone();
                tokio::spawn(
                    async move { request(&app, method, &path, Some(&token), None, body).await },
                )
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(300)).await;
        holder.commit().await.unwrap();
        for call in calls {
            let (status, body) = call.await.unwrap();
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
    cleanup(&storage);
    pool.close().await;
}
