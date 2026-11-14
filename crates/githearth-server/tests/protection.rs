use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
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

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

async fn get_html(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post_form(
    app: &Router,
    path: &str,
    cookie: &str,
    form: &[(&str, &str)],
) -> (StatusCode, Option<String>, String) {
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
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

    // A pattern longer than the limit is rejected.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "a".repeat(300)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

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

fn branch_settings_path(org: &str, repo: &str) -> String {
    format!("/organizations/{org}/repositories/{repo}/settings/branches")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_page_lists_creates_and_deletes_rules() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let token = fixture_session(&pool, owner).await;
    let cookie = format!("githearth_session={token}");
    let path = branch_settings_path(&slug, "r");

    // Signed out redirects to login.
    let (status, _) = get_html(&app, &path, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // The owner sees the empty page and the rule fields.
    let (status, html) = get_html(&app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Branch protection"), "{html}");
    assert!(html.contains("No branch protection rules yet."), "{html}");
    assert!(html.contains("name=\"required_approvals\""), "{html}");
    assert!(html.contains("name=\"required_status_checks\""), "{html}");
    assert!(html.contains("name=\"require_merge_queue\""), "{html}");

    // The repository page links to the new settings page.
    let (_, repo_page) = get_html(
        &app,
        &format!("/organizations/{slug}/repositories/r"),
        Some(&cookie),
    )
    .await;
    assert!(repo_page.contains("/settings/branches"), "{repo_page}");

    let (status, location, _) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &hidden(&html, "csrf_token")),
            ("idempotency_key", &hidden(&html, "idempotency_key")),
            ("pattern", "main"),
            ("required_approvals", "2"),
            ("required_status_checks", "ci, lint"),
            ("require_pull_request", "1"),
            ("require_signed_commits", "1"),
            ("require_merge_queue", "1"),
            ("require_code_owner_review", "1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.as_deref().unwrap().ends_with("/settings/branches"));

    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    assert!(html.contains("main"), "{html}");
    assert!(html.contains("ci, lint"), "{html}");
    assert!(html.contains(">2</td>"), "{html}");
    assert!(html.contains("Dismiss stale"), "{html}");
    assert!(html.contains("id=\"required_status_checks\""), "{html}");

    let (pattern, approvals, checks, require_pr, require_signed, require_queue, require_owners): (
        String,
        i32,
        Vec<String>,
        bool,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT pattern, required_approvals, required_status_checks, require_pull_request, \
                require_signed_commits, require_merge_queue, require_code_owner_review \
         FROM branch_protection_rules WHERE repository_id = $1",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pattern, "main");
    assert_eq!(approvals, 2);
    assert_eq!(checks, vec!["ci", "lint"]);
    assert!(require_pr && require_signed && require_queue && require_owners);

    // Invalid input is a re-rendered 400 with the submitted values kept.
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &hidden(&html, "csrf_token")),
            ("idempotency_key", &hidden(&html, "idempotency_key")),
            ("pattern", "bad//pattern"),
            ("required_approvals", "0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("value=\"bad//pattern\""), "{body}");

    // A duplicate pattern is a 409 with an explanatory message.
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &hidden(&html, "csrf_token")),
            ("idempotency_key", &hidden(&html, "idempotency_key")),
            ("pattern", "main"),
            ("required_approvals", "0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("already exists"), "{body}");

    // Delete through the page.
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    let rule_id: String =
        sqlx::query_scalar("SELECT id::text FROM branch_protection_rules WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (status, _, _) = post_form(
        &app,
        &format!("{path}/{rule_id}/delete"),
        &cookie,
        &[("csrf_token", &hidden(&html, "csrf_token"))],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    assert!(html.contains("No branch protection rules yet."), "{html}");

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 \
         AND kind IN ('branch_protection.created','branch_protection.deleted')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 2);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_reports_an_inactive_repository_and_bad_form_keys() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let token = fixture_session(&pool, owner).await;
    let cookie = format!("githearth_session={token}");
    let path = branch_settings_path(&slug, "r");
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    let csrf = hidden(&html, "csrf_token");

    // A malformed idempotency key re-renders the page with a 400.
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "bad key"),
            ("pattern", "main"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Reload the page"), "{body}");

    // A repository that is no longer active reports its 409 on the page.
    let rule_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_protection_rules (id, repository_id, pattern, created_by) \
         VALUES ($1, $2, 'main', $3)",
    )
    .bind(rule_id)
    .bind(repo_id)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, body) = post_form(
        &app,
        &format!("{path}/{rule_id}/delete"),
        &cookie,
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("Cannot apply that change"), "{body}");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_renders_validation_and_missing_rule_errors() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    fixture_repository(&pool, org_id, "r", owner).await;
    let token = fixture_session(&pool, owner).await;
    let cookie = format!("githearth_session={token}");
    let path = branch_settings_path(&slug, "r");
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    let csrf = hidden(&html, "csrf_token");

    // An invalid pattern is a 400 page with the API message.
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "00112233445566778899aabbccddeeff"),
            ("pattern", "../bad"),
            ("required_approvals", "0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("Invalid pattern") || body.contains("invalid"),
        "{body}"
    );

    // An out-of-range approval count is 422.
    let (status, _, _) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "ffeeddccbbaa99887766554433221101"),
            ("pattern", "release/*"),
            ("required_approvals", "0"),
            ("required_status_checks", "githearth/legacy-require-checks"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Deleting an unknown rule is 404, not 503.
    let (status, _, _) = post_form(
        &app,
        &format!("{path}/{}/delete", Uuid::new_v4()),
        &cookie,
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_duplicate_is_409_and_delete_removes_the_rule() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let token = fixture_session(&pool, owner).await;
    let cookie = format!("githearth_session={token}");
    let path = branch_settings_path(&slug, "r");

    let (status, html) = get_html(&app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = hidden(&html, "csrf_token");
    let create = [
        ("csrf_token", csrf.as_str()),
        ("idempotency_key", "00112233445566778899aabbccddeeff"),
        ("pattern", "main"),
        ("required_approvals", "0"),
        ("required_status_checks", ""),
    ];
    let (status, location, _) = post_form(&app, &path, &cookie, &create).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.is_some());

    // A second create with a fresh key is a duplicate: the page reports 409,
    // not a 503.
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "ffeeddccbbaa99887766554433221100"),
            ("pattern", "main"),
            ("required_approvals", "0"),
            ("required_status_checks", ""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("already exists"), "{body}");

    let rule_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM branch_protection_rules WHERE repository_id = $1 AND pattern = 'main'",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let delete_path = format!("{path}/{rule_id}/delete");

    // Bad CSRF is refused; a valid deletion redirects and removes the rule.
    let (status, _, _) = post_form(&app, &delete_path, &cookie, &[("csrf_token", "wrong")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, location, _) =
        post_form(&app, &delete_path, &cookie, &[("csrf_token", &csrf)]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.is_some());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM branch_protection_rules WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_writes_require_manage_and_csrf() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    fixture_repository(&pool, org_id, "r", owner).await;
    let path = branch_settings_path(&slug, "r");

    // A write member may read the page but not create or delete a rule.
    let writer = add_member(&pool, org_id, "write").await;
    let writer_token = fixture_session(&pool, writer).await;
    let writer_cookie = format!("githearth_session={writer_token}");
    let (status, html) = get_html(&app, &path, Some(&writer_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = hidden(&html, "csrf_token");
    let (status, _, body) = post_form(
        &app,
        &path,
        &writer_cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "00112233445566778899aabbccddeeff"),
            ("pattern", "main"),
            ("required_approvals", "0"),
            ("required_status_checks", ""),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM branch_protection_rules WHERE repository_id = (SELECT id FROM repositories WHERE organization_id = $1 AND name = 'r')")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "a write member must not create a rule");

    // Missing CSRF is rejected before any state change, even for the owner.
    let owner_token = fixture_session(&pool, owner).await;
    let owner_cookie = format!("githearth_session={owner_token}");
    let (status, _, _) = post_form(
        &app,
        &path,
        &owner_cookie,
        &[("csrf_token", "wrong"), ("pattern", "main")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A non-member gets the not-found page, not the rule list.
    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    let (status, _) = get_html(
        &app,
        &path,
        Some(&format!("githearth_session={stranger_token}")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn branch_settings_page_hides_forms_from_viewers_who_cannot_manage() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    sqlx::query(
        "INSERT INTO branch_protection_rules (id, repository_id, pattern, created_by) \
         VALUES ($1, $2, 'main', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();

    let reader = add_member(&pool, org_id, "read").await;
    let token = fixture_session(&pool, reader).await;
    let cookie = format!("githearth_session={token}");
    let path = branch_settings_path(&slug, "r");

    let (status, html) = get_html(&app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Branch protection"), "{html}");
    assert!(html.contains("main"), "{html}");
    assert!(!html.contains("Add a rule"), "{html}");
    assert!(!html.contains(">Delete</button>"), "{html}");

    cleanup(&storage);
    pool.close().await;
}
