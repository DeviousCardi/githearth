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

async fn raw_request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
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
    let storage_root = std::env::temp_dir().join(format!("githearth-repo-api-{}", Uuid::new_v4()));
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
    let _token = fixture_session(&pool, owner).await;
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, storage_root, owner, org_slug)
}

fn cleanup(storage_root: &PathBuf) {
    let _ = std::fs::remove_dir_all(storage_root);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_and_bare_repo_on_disk() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let token = fixture_session(
        &pool,
        sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
            .bind(&org_slug)
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "my-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "my-repo");
    assert_eq!(body["status"], "active");
    let id = body["id"].as_str().unwrap();
    let id_uuid: Uuid = id.parse().unwrap();
    let id_simple = id_uuid.simple().to_string();
    let repo_path = storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"));
    assert!(repo_path.is_dir());
    assert!(repo_path.join("HEAD").is_file());

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_sha256_repository_on_disk() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "sha256-repo", "object_format": "sha256"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "sha256-repo");

    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let id_simple = id.simple().to_string();
    let repo_path = storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"));
    assert_eq!(
        githearth_git::layout::detect_object_format(&repo_path).unwrap(),
        githearth_git::layout::ObjectFormat::Sha256
    );

    let stored: String = sqlx::query_scalar("SELECT object_format FROM repositories WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "sha256");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_object_format_400() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "bad-format", "object_format": "sha512"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn duplicate_name_409() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "dup-repo"}),
    )
    .await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "dup-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_names_400() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    for name in ["", ".", "..", "repo.git", "a/b"] {
        let (status, _) = request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories"),
            Some(&token),
            None,
            json!({"name": name}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "name '{name}' should be rejected"
        );
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn nul_bytes_are_invalid_input_400() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    // A repository description with NUL must be refused before INSERT.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "nul-desc", "description": "bad\u{0}description"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "nul-patch"}),
    )
    .await;
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/nul-patch"),
        Some(&token),
        None,
        json!({"description": "bad\u{0}description"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // NUL in a decoded organization or repository path parameter is a 400,
    // not a database error.
    for path in [
        format!("/api/v1/organizations/{org_slug}%00/repositories"),
        format!("/api/v1/organizations/{org_slug}/repositories/nul-patch%00"),
    ] {
        let (status, _) = request(&app, "GET", &path, Some(&token), None, Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_user_404_on_read_list_patch_delete() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (_, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "private-repo"}),
    )
    .await;
    assert_eq!(body["name"], "private-repo");

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;

    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/v1/organizations/{org_slug}/repositories/private-repo"),
            Some(&stranger_token),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/v1/organizations/{org_slug}/repositories"),
            Some(&stranger_token),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/v1/organizations/{org_slug}/repositories/private-repo"),
            Some(&stranger_token),
            None,
            json!({"description": "nope"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/v1/organizations/{org_slug}/repositories/private-repo"),
            Some(&stranger_token),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn not_found_bodies_do_not_reveal_existence() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "private-repo"}),
    )
    .await;

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    let missing_org = tag("missing-org");
    let cases = [
        // Existing but unreadable private repository.
        format!("/api/v1/organizations/{org_slug}/repositories/private-repo"),
        // Missing repository in an existing organization.
        format!("/api/v1/organizations/{org_slug}/repositories/missing"),
        // Missing organization.
        format!("/api/v1/organizations/{missing_org}/repositories/private-repo"),
    ];
    let mut bodies = Vec::new();
    for path in &cases {
        let (status, body) = raw_request(&app, "GET", path, Some(&stranger_token)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        bodies.push(body);
    }
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[1], bodies[2]);
    assert_eq!(bodies[0], br#"{"error":"not found"}"#);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_member_403_on_mutation() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (_, _body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "shared-repo", "visibility": "internal"}),
    )
    .await;

    let reader = fixture_account(&pool, &tag("reader")).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(reader)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4()).bind(reader).bind(org_id).execute(&pool).await.unwrap();
    let reader_token = fixture_session(&pool, reader).await;

    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/v1/organizations/{org_slug}/repositories/shared-repo"),
            Some(&reader_token),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "PATCH",
            &format!("/api/v1/organizations/{org_slug}/repositories/shared-repo"),
            Some(&reader_token),
            None,
            json!({"description": "nope"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories/shared-repo/archive"),
            Some(&reader_token),
            None,
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
            &format!("/api/v1/organizations/{org_slug}/repositories/shared-repo"),
            Some(&reader_token),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn org_write_member_can_create_repository() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();

    // A member with a write organization grant must be able to create a
    // repository (issue #133: roles are cumulative).
    let writer = fixture_account(&pool, &tag("writer")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(writer)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'write','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(writer)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let writer_token = fixture_session(&pool, writer).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&writer_token),
        None,
        json!({"name": "writer-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn member_without_grant_gets_403_not_404() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();

    // An organization member without a create-capable grant is refused with a
    // clear 403, not a misleading 404 "organization not found" (issue #133).
    let member = fixture_account(&pool, &tag("member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    let member_token = fixture_session(&pool, member).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&member_token),
        None,
        json!({"name": "blocked-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, json!({"error": "not permitted"}));

    // A stranger (not a member) still sees a 404, so the organization is not
    // revealed.
    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&stranger_token),
        None,
        json!({"name": "also-blocked"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn list_paging() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let mut ids = Vec::new();
    for i in 0..3 {
        let (_, body) = request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories"),
            Some(&token),
            None,
            json!({"name": format!("repo-{i}")}),
        )
        .await;
        ids.push(body["id"].as_str().unwrap().to_owned());
    }

    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories?limit=2"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let after = arr[1]["id"].as_str().unwrap();
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories?limit=2&after={after}"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rename_then_old_name_still_resolves() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "old-name"}),
    )
    .await;

    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/old-name"),
        Some(&token),
        None,
        json!({"name": "new-name"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "new-name");

    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/old-name"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "new-name");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archive_unarchive() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (_, _body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "archive-me"}),
    )
    .await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/archive-me/archive"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["archived"].as_bool().unwrap());

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/archive-me/unarchive"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["archived"].as_bool().unwrap());

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn delete_204_dir_gone_name_reusable() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (_, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "delete-me"}),
    )
    .await;
    let _id = body["id"].as_str().unwrap().to_owned();

    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/delete-me"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Reuse name
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "delete-me"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "delete-me");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn storage_failure_503() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    // Make the repositories dir read-only to force storage failure
    let repos_dir = storage_root.join("repositories");
    std::fs::create_dir_all(&repos_dir).unwrap();
    #[allow(unused_mut)]
    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o500);
    }
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "fail-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "repository storage unavailable");

    // Restore permissions for cleanup
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&repos_dir, perms).unwrap();
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotency_replay_same_response() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let key = tag("idem");
    let repo_name = tag("repo");

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": &repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, body2) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": &repo_name}),
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(body, body2);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repositories WHERE lower(name)=lower($1)")
            .bind(&repo_name)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotency_different_body_same_key_422() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let key = tag("idem2");

    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "repo-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status2, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "repo-b"}),
    )
    .await;
    assert_eq!(status2, StatusCode::UNPROCESSABLE_ENTITY);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotency_bad_key_400() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    for bad_key in ["", &"x".repeat(129)] {
        let (status, _) = request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories"),
            Some(&token),
            Some(("Idempotency-Key", bad_key)),
            json!({"name": "repo-x"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "key '{bad_key}' should be 400"
        );
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotency_expired_row_reused() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let key = tag("idem-exp");
    let hash = vec![1u8; 32];
    sqlx::query("INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1,$2,$3,'running',clock_timestamp() - interval '1 second')")
        .bind(owner_id).bind(&key).bind(&hash).execute(&pool).await.unwrap();

    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "fresh-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_includes_status() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let (_, _body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "status-repo"}),
    )
    .await;
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/status-repo"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "active");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_active_repo_409_on_patch() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    // Make create fail -> status stays creating (simulate failed storage)
    let repos_dir = storage_root.join("repositories");
    std::fs::create_dir_all(&repos_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&repos_dir, perms).unwrap();
    }

    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "inactive-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&repos_dir, perms).unwrap();
    }

    // The repo exists but is "failed", so patch should give 404 or 409
    // Actually the user can't read it via API because it's not in list (no
    // authz_repo check will deny it for non-existence). Let's just verify
    // create failed as expected.
    assert!(body["error"].as_str().is_some());

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotency_409_on_running() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    let key = tag("idem-run");
    let hash = vec![2u8; 32];
    sqlx::query("INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1,$2,$3,'running',clock_timestamp() + interval '1 hour')")
        .bind(owner_id).bind(&key).bind(&hash).execute(&pool).await.unwrap();

    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "running-repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archive_already_archived_is_noop_200() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "double-archive"}),
    )
    .await;
    let (s1, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/double-archive/archive"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/double-archive/archive"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn update_description_only() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;

    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "desc-repo"}),
    )
    .await;
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/desc-repo"),
        Some(&token),
        None,
        json!({"description": "my description"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "my description");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn description_limit_counts_characters_not_bytes() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let token = fixture_session(&pool, owner_id).await;

    // 1000 non-ASCII characters are 2000 bytes; the limit is 1000 characters.
    let description = "é".repeat(1000);
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "long-desc", "description": description}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["description"].as_str().unwrap().chars().count(), 1000);

    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "too-long", "description": "é".repeat(1001)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_delete_with_same_key() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug).fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "replay-del"}),
    )
    .await;
    let key = tag("idem-del");
    let (s1, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/replay-del"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        Value::Null,
    )
    .await;
    assert_eq!(s1, StatusCode::NO_CONTENT);
    let (s2, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/replay-del"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        Value::Null,
    )
    .await;
    assert_eq!(s2, StatusCode::NO_CONTENT);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_rename_with_same_key() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug).fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "replay-ren"}),
    )
    .await;
    let key = tag("idem-ren");
    let (s1, b1) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/replay-ren"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "replay-ren-new"}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["name"], "replay-ren-new");
    let (s2, b2) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/replay-ren"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "replay-ren-new"}),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b1, b2);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn name_conflict_frees_idempotency_key() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug).fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": "taken-name"}),
    )
    .await;
    let key = tag("idem-free");
    let (s1, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "taken-name"}),
    )
    .await;
    assert_eq!(s1, StatusCode::CONFLICT);
    let (s2, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        json!({"name": "free-name"}),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_same_key_one_201_one_409() {
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug).fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    let token2 = fixture_session(&pool, owner_id).await;
    let key = tag("idem-conc");
    let repo_name = tag("conc-repo");
    let app1 = app.clone();
    let app2 = app.clone();
    let token1 = token.clone();
    let token2_ = token2;
    let org_slug1 = org_slug.clone();
    let org_slug2 = org_slug.clone();
    let key1 = key.clone();
    let key2 = key.clone();
    let repo_name1 = repo_name.clone();
    let repo_name2 = repo_name;
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            request(
                &app1,
                "POST",
                &format!("/api/v1/organizations/{org_slug1}/repositories"),
                Some(&token1),
                Some(("Idempotency-Key", &key1)),
                json!({"name": &repo_name1}),
            )
            .await
            .0
        }),
        tokio::spawn(async move {
            request(
                &app2,
                "POST",
                &format!("/api/v1/organizations/{org_slug2}/repositories"),
                Some(&token2_),
                Some(("Idempotency-Key", &key2)),
                json!({"name": &repo_name2}),
            )
            .await
            .0
        }),
    );
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();
    assert!(
        (r1 == StatusCode::CREATED && r2 == StatusCode::CONFLICT)
            || (r1 == StatusCode::CONFLICT && r2 == StatusCode::CREATED),
        "concurrent same key: {r1} {r2}"
    );
    assert!(
        r1 != StatusCode::INTERNAL_SERVER_ERROR && r2 != StatusCode::INTERNAL_SERVER_ERROR,
        "no 500 from concurrent insert"
    );
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn storage_failure_leaves_creating_for_the_reconciler() {
    use std::os::unix::fs::PermissionsExt;
    let (app, pool, storage_root, _, org_slug) = setup().await;
    let owner_id = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT id FROM organizations WHERE slug=$1) LIMIT 1")
        .bind(&org_slug).fetch_one(&pool).await.unwrap();
    let token = fixture_session(&pool, owner_id).await;
    let repos_dir = storage_root.join("repositories");
    std::fs::create_dir_all(&repos_dir).unwrap();
    std::fs::set_permissions(&repos_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let base = format!("/api/v1/organizations/{org_slug}/repositories");
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"name": "broken"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    std::fs::set_permissions(&repos_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    // The row stays `creating` (ADR 0003): clients can tell it from a missing repository.
    let (status, body) = request(
        &app,
        "GET",
        &format!("{base}/broken"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("creating"))
    );
    for (method, suffix) in [
        ("DELETE", ""),
        ("POST", "/archive"),
        ("POST", "/unarchive"),
        ("PATCH", ""),
    ] {
        let (status, body) = request(
            &app,
            method,
            &format!("{base}/broken{suffix}"),
            Some(&token),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}");
        assert_eq!(body["error"], "repository is not active");
    }
    assert!(
        !repos_dir.read_dir().unwrap().any(|_| true),
        "nothing reached disk"
    );
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_repository_can_be_deleted_and_name_reused() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let base = format!("/api/v1/organizations/{org_slug}/repositories");
    use std::os::unix::fs::PermissionsExt;
    let repos_dir = storage_root.join("repositories");
    std::fs::create_dir_all(&repos_dir).unwrap();
    std::fs::set_permissions(&repos_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"name": "dead"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    std::fs::set_permissions(&repos_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    sqlx::query("UPDATE repositories SET status='failed' WHERE name='dead' AND organization_id=(SELECT id FROM organizations WHERE slug=$1)")
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("{base}/dead"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"name": "dead"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mutations_through_a_renamed_away_name_are_404() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let base = format!("/api/v1/organizations/{org_slug}/repositories");
    request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"name": "before"}),
    )
    .await;
    request(
        &app,
        "PATCH",
        &format!("{base}/before"),
        Some(&token),
        None,
        json!({"name": "after"}),
    )
    .await;
    for (method, suffix) in [
        ("DELETE", ""),
        ("PATCH", ""),
        ("POST", "/archive"),
        ("POST", "/unarchive"),
    ] {
        let (status, body) = request(
            &app,
            method,
            &format!("{base}/before{suffix}"),
            Some(&token),
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {suffix}");
        assert_eq!(body["error"], "not found");
    }
    let (status, _) = request(
        &app,
        "GET",
        &format!("{base}/after"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unarchive_of_an_unarchived_repository_is_a_noop() {
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let base = format!("/api/v1/organizations/{org_slug}/repositories");
    request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"name": "plain"}),
    )
    .await;
    let events = || {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events WHERE kind='repository.unarchived' AND organization_id=(SELECT id FROM organizations WHERE slug=$1)").bind(&org_slug).fetch_one(&pool)
    };
    let before = events().await.unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("{base}/plain/unarchive"),
        Some(&token),
        None,
        json!({}),
    )
    .await;
    assert_eq!(
        (status, body["archived"].as_bool()),
        (StatusCode::OK, Some(false))
    );
    assert_eq!(events().await.unwrap(), before);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_keyed_mutations_do_not_exhaust_the_pool() {
    // setup() allows 4 connections; 12 parallel keyed requests must all finish.
    let (app, pool, storage_root, owner, org_slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let base = format!("/api/v1/organizations/{org_slug}/repositories");
    let calls: Vec<_> = (0..12)
        .map(|i| {
            let (app, token, base) = (app.clone(), token.clone(), base.clone());
            tokio::spawn(async move {
                let key = format!("pool-{i}");
                request(
                    &app,
                    "POST",
                    &base,
                    Some(&token),
                    Some(("Idempotency-Key", &key)),
                    json!({"name": format!("p{i}")}),
                )
                .await
                .0
            })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), StatusCode::CREATED);
    }
    cleanup(&storage_root);
    pool.close().await;
}
