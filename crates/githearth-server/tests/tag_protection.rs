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
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility) \
         VALUES ($1, $2, $3, $4, $5, 'active', 'private')",
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

async fn setup() -> (Router, PgPool, PathBuf, Uuid, Uuid, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-tprot-{}", Uuid::new_v4()));
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

fn base_path(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/tag-protection")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admin_only_create_list_delete_with_generation_bumps() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let base = base_path(&slug, "r");

    // Empty list with generation 0.
    let (status, body) = request(&app, "GET", &base, Some(&token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["generation"], 0);
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    // Create (owner is admin).
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "v*", "allowed_to_create": "admin", "allow_update": false, "allow_delete": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["pattern"], "v*");
    assert_eq!(body["allowed_to_create"], "admin");
    assert_eq!(body["allow_update"], false);
    assert_eq!(body["allow_delete"], false);
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
    assert_eq!(body["items"][0]["pattern"], "v*");

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
    assert_eq!(generation, 2);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn maintain_manages_rules_like_branch_protection() {
    let (app, pool, storage, _owner, org_id, slug) = setup().await;
    let _ = fixture_repository(&pool, org_id, "r", _owner).await;
    let base = base_path(&slug, "r");

    // maintain (org-level) may create a rule: tag protection shares the
    // `repository.update` (maintain) gate with branch protection (issue #135).
    let maintainer = add_member(&pool, org_id, "maintain").await;
    let maintainer_token = fixture_session(&pool, maintainer).await;
    let (status, _) = request(
        &app,
        "GET",
        &base,
        Some(&maintainer_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = request(
        &app,
        "POST",
        &base,
        Some(&maintainer_token),
        None,
        json!({"pattern": "v*"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let rule_id = body["id"].as_str().unwrap().to_string();

    // The same maintain member may also delete it.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("{base}/{rule_id}"),
        Some(&maintainer_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // read (org-level) may list but not manage tag protection.
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
        json!({"pattern": "v*"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_and_duplicate_pattern() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let _ = fixture_repository(&pool, org_id, "i", owner).await;
    let base = base_path(&slug, "i");

    let key = tag("idem");
    let body = json!({"pattern": "v*", "allowed_to_create": "write"});
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
        "SELECT count(*) FROM tag_protection_rules WHERE repository_id = (SELECT id FROM repositories WHERE organization_id=$1 AND name='i')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    // A different idempotency key with the same pattern is a 409.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "v*"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_pattern_and_role_400() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let token = fixture_session(&pool, owner).await;
    let _ = fixture_repository(&pool, org_id, "v", owner).await;
    let base = base_path(&slug, "v");

    for pattern in ["", "*", "v**", "/v", "v/", "a//b", "a/../b", "release**"] {
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

    // `v*` and `release/*` are valid.
    for pattern in ["v*", "release/*", "v1.0.0"] {
        let (status, _) = request(
            &app,
            "POST",
            &base,
            Some(&token),
            None,
            json!({"pattern": pattern}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "pattern={pattern:?}");
    }

    // An unknown create role is refused.
    let (status, _) = request(
        &app,
        "POST",
        &base,
        Some(&token),
        None,
        json!({"pattern": "x*", "allowed_to_create": "owner"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&storage);
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Settings UI (server-rendered tag protection page)
// ---------------------------------------------------------------------------

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
    let mut builder = Request::builder().uri(path);
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
async fn settings_page_lists_creates_and_deletes_rules() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let _ = fixture_repository(&pool, org_id, "r", owner).await;
    let token = fixture_session(&pool, owner).await;
    let cookie = format!("githearth_session={token}");
    let path = format!("/organizations/{slug}/repositories/r/settings/tags");

    // Signed out redirects to login.
    let (status, _) = get_html(&app, &path, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Owner sees the page with no rules yet.
    let (status, html) = get_html(&app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Tag protection"), "{html}");
    assert!(html.contains("No tag protection rules yet."), "{html}");

    // Create a rule from the form.
    let csrf = hidden(&html, "csrf_token");
    let idem = hidden(&html, "idempotency_key");
    let (status, location, _) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", &idem),
            ("pattern", "v*"),
            ("allowed_to_create", "maintain"),
            ("allow_update", "1"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.as_deref().unwrap().ends_with("/settings/tags"));

    // The rule now appears.
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    assert!(html.contains("v*"), "{html}");
    assert!(html.contains("maintain"), "{html}");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tag_protection_rules WHERE repository_id = (SELECT id FROM repositories WHERE organization_id=$1 AND name='r')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    // An invalid pattern is a 400 with the form and its values, not a 503.
    let (status, _, body) = post_form(
        &app,
        &path,
        &cookie,
        &[
            ("csrf_token", &hidden(&html, "csrf_token")),
            ("idempotency_key", &hidden(&html, "idempotency_key")),
            ("pattern", "bad//pattern"),
            ("allowed_to_create", "write"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("Choose a valid pattern"), "{body}");
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
            ("pattern", "v*"),
            ("allowed_to_create", "maintain"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("already exists"), "{body}");

    // Delete it.
    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    let csrf = hidden(&html, "csrf_token");
    let rule_id: String = sqlx::query_scalar(
        "SELECT id::text FROM tag_protection_rules WHERE repository_id = (SELECT id FROM repositories WHERE organization_id=$1 AND name='r')",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (status, _, _) = post_form(
        &app,
        &format!("{path}/{rule_id}/delete"),
        &cookie,
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_, html) = get_html(&app, &path, Some(&cookie)).await;
    assert!(html.contains("No tag protection rules yet."), "{html}");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn settings_page_hides_forms_from_viewers_who_cannot_manage() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;

    // Seed a rule directly so the read-only viewer has something to list.
    sqlx::query(
        "INSERT INTO tag_protection_rules \
         (id, repository_id, pattern, allowed_to_create, created_by) \
         VALUES ($1, $2, 'v*', 'admin', $3)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();

    // A reader can read the repository and list rules, but cannot manage
    // protection: the page shows the rules and hides every control.
    let reader = add_member(&pool, org_id, "read").await;
    let token = fixture_session(&pool, reader).await;
    let cookie = format!("githearth_session={token}");
    let path = format!("/organizations/{slug}/repositories/r/settings/tags");

    let (status, html) = get_html(&app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Tag protection"), "{html}");
    assert!(html.contains("v*"), "{html}");
    assert!(!html.contains("Add a rule"), "{html}");
    assert!(!html.contains(">Delete</button>"), "{html}");

    cleanup(&storage);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn database_check_matches_parse_pattern_and_create_roles() {
    let (_app, pool, storage, owner, org_id, _slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "chk", owner).await;

    // parse_pattern rejects a '.' or '..' segment; the migration CHECK must
    // reject the same rows so a direct insert cannot create a dead rule.
    for pattern in ["a/../b", "a/./b", ".", "..", ".*", "..*", "./*", "a/.."] {
        assert!(
            insert_tag_rule(&pool, repo_id, owner, pattern, "admin")
                .await
                .is_err(),
            "pattern {pattern:?} passed the pattern CHECK"
        );
    }
    // 'member' is no longer an accepted threshold.
    assert!(
        insert_tag_rule(&pool, repo_id, owner, "v2.0.0", "member")
            .await
            .is_err(),
        "'member' passed the role CHECK"
    );
    // The patterns parse_pattern accepts are still accepted by the CHECK.
    for pattern in ["v1.0.0", "v*", "release/*", "v1.*"] {
        insert_tag_rule(&pool, repo_id, owner, pattern, "admin")
            .await
            .unwrap();
    }

    cleanup(&storage);
    pool.close().await;
}

async fn insert_tag_rule(
    pool: &PgPool,
    repo_id: Uuid,
    created_by: Uuid,
    pattern: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tag_protection_rules \
         (id, repository_id, pattern, allowed_to_create, created_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(pattern)
    .bind(role)
    .bind(created_by)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn delete_errors_render_permission_and_unavailable_pages() {
    let (app, pool, storage, owner, org_id, slug) = setup().await;
    let repo_id = fixture_repository(&pool, org_id, "r", owner).await;
    let owner_token = fixture_session(&pool, owner).await;
    let owner_cookie = format!("githearth_session={owner_token}");
    let path = format!("/organizations/{slug}/repositories/r/settings/tags");
    let rule_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tag_protection_rules (id, repository_id, pattern, allowed_to_create, created_by) \
         VALUES ($1, $2, 'v*', 'maintain', $3)",
    )
    .bind(rule_id)
    .bind(repo_id)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();

    // A write member may read the page but not delete a rule: 403 page.
    let writer = add_member(&pool, org_id, "write").await;
    let writer_token = fixture_session(&pool, writer).await;
    let writer_cookie = format!("githearth_session={writer_token}");
    let (status, html) = get_html(&app, &path, Some(&writer_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = post_form(
        &app,
        &format!("{path}/{rule_id}/delete"),
        &writer_cookie,
        &[("csrf_token", &hidden(&html, "csrf_token"))],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("Not permitted"), "{body}");

    // A repository that is no longer active maps to the unavailable page.
    let (_, owner_html) = get_html(&app, &path, Some(&owner_cookie)).await;
    let owner_csrf = hidden(&owner_html, "csrf_token");
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, body) = post_form(
        &app,
        &format!("{path}/{rule_id}/delete"),
        &owner_cookie,
        &[("csrf_token", &owner_csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    cleanup(&storage);
    pool.close().await;
}
