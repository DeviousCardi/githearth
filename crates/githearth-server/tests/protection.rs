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
    // Authorization reads grants; the membership role stays 'member' for display.
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

async fn setup() -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-prot-{}", Uuid::new_v4()));
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

fn protection_path(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/protection")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_list_update_delete_with_generation_bumps() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let base = protection_path(&slug, "r");

    // Empty list with generation 0.
    let (status, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["generation"], 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    // Create.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main", "allow_force_push": true, "require_pull_request": true, "required_approvals": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["pattern"], "main");
    assert_eq!(body["allow_force_push"], true);
    assert_eq!(body["require_pull_request"], true);
    assert_eq!(body["required_approvals"], 2);
    let rule_id = body["id"].as_str().unwrap().to_string();

    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM protection_generations WHERE repository_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generation, 1);

    // List reflects it.
    let (_, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(body["generation"], 1);
    assert_eq!(body["items"][0]["pattern"], "main");

    // Update: `pattern` is the documented field (a `path` alias is accepted).
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("{base}/{rule_id}"),
        Some(&token),
        None,
        json!({"pattern": "release/*", "required_status_checks": ["ci"], "allow_force_push": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pattern"], "release/*");
    assert_eq!(body["required_status_checks"], json!(["ci"]));
    assert_eq!(body["allow_force_push"], false);

    let (status, body) = request(
        &app,
        "PATCH",
        &format!("{base}/{rule_id}"),
        Some(&token),
        None,
        json!({"path": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pattern"], "main", "legacy `path` alias still works");

    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM protection_generations WHERE repository_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generation, 3, "each update bumps the generation");

    // Delete.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("{base}/{rule_id}"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let generation: i64 = sqlx::query_scalar(
        "SELECT generation FROM protection_generations WHERE repository_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(generation, 4);

    let (_, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn maintain_write_read_permissions() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    fixture_repository(&pool, org_id, "r", owner).await;
    let base = protection_path(&slug, "r");

    // maintain (org-level) can create.
    let maintainer = add_member(&pool, org_id, "maintain").await;
    let maintainer_token = fixture_session(&pool, maintainer).await;
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&maintainer_token),
        None,
        json!({"pattern": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // read (org-level) can list but not create.
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
        json!({"pattern": "feature"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // write (org-level) can read but not mutate.
    let writer = add_member(&pool, org_id, "write").await;
    let writer_token = fixture_session(&pool, writer).await;
    let (status, _) = request(&app, "GET", &base, Some(&writer_token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&writer_token),
        None,
        json!({"pattern": "wip"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // owner can still mutate.
    let owner_token = fixture_session(&pool, owner).await;
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&owner_token),
        None,
        json!({"pattern": "owner-only"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_user_404() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "priv", owner).await;
    let base = protection_path(&slug, "priv");
    request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main"}),
    )
    .await;

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    for (method, path) in [("GET", base.clone())] {
        let (status, _) = request(
            &app,
            method,
            &path,
            Some(&stranger_token),
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_replay() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "i", owner).await;
    let base = protection_path(&slug, "i");
    let key = tag("idem");
    let body = json!({"pattern": "main", "require_pull_request": true});

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
        "SELECT count(*) FROM branch_protection_rules WHERE repository_id = (SELECT id FROM repositories WHERE organization_id=$1 AND name='i')",
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
async fn invalid_pattern_400() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "v", owner).await;
    let base = protection_path(&slug, "v");

    for pattern in [
        "",
        "release*",
        "*",
        "release/**",
        "a//b",
        "/main",
        "main/",
        "release/*/x",
    ] {
        let (status, _) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"pattern": pattern}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "pattern={pattern:?}");
    }

    // required_approvals out of range.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main", "required_approvals": 11}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_required_status_checks_422_on_create_and_patch() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "c", owner).await;
    let base = protection_path(&slug, "c");

    // Reserved context, duplicates and surrounding whitespace are 422 on create.
    for checks in [
        json!(["githearth/legacy-require-checks"]),
        json!(["ci", "ci"]),
        json!(["ci", "lint", "ci"]),
        json!([" ci"]),
        json!(["ci "]),
        json!([" ci "]),
    ] {
        let (status, body) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"pattern": "main", "required_status_checks": checks}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    // A valid rule is created, then patched with each invalid list.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rule_id = body["id"].as_str().unwrap().to_string();

    for checks in [
        json!(["githearth/legacy-require-checks"]),
        json!(["ci", "ci"]),
        json!([" ci"]),
    ] {
        let (status, body) = request(
            &app,
            "PATCH",
            &format!("{base}/{rule_id}"),
            Some(&token),
            None,
            json!({"required_status_checks": checks}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn duplicate_pattern_409() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "d", owner).await;
    let base = protection_path(&slug, "d");

    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_and_signed_flags_default_and_round_trip() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    fixture_repository(&pool, org_id, "f", owner).await;
    let base = protection_path(&slug, "f");

    // A rule without the new flags defaults to dismissing stale approvals and
    // not requiring signed commits.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["dismiss_stale_approvals"], true);
    assert_eq!(body["require_signed_commits"], false);
    let main_id = body["id"].as_str().unwrap().to_string();

    // A rule can opt out and require signed commits.
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({
            "pattern": "release/*",
            "dismiss_stale_approvals": false,
            "require_signed_commits": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["dismiss_stale_approvals"], false);
    assert_eq!(body["require_signed_commits"], true);
    let release_id = body["id"].as_str().unwrap().to_string();

    // Patching flips each flag independently.
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("{base}/{release_id}"),
        Some(&token),
        None,
        json!({"require_signed_commits": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dismiss_stale_approvals"], false);
    assert_eq!(body["require_signed_commits"], false);

    let (status, body) = request(
        &app,
        "PATCH",
        &format!("{base}/{main_id}"),
        Some(&token),
        None,
        json!({"dismiss_stale_approvals": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dismiss_stale_approvals"], false);
    assert_eq!(body["require_signed_commits"], false);

    cleanup(&storage);
    pool.close().await;
}
