//! Issue and comment page contract tests (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::PgPool;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    base: String,
    api_base: String,
    token: String,
    cookie: String,
    org_id: Uuid,
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = app.clone().oneshot(request).await.unwrap();
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

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, _, body) = send(app, builder.body(Body::empty()).unwrap()).await;
    (status, body)
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
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
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

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, _, text) = send(
        app,
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    (status, serde_json::from_str(&text).unwrap_or_default())
}

fn csrf(html: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let rest = &html[html.find(marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

/// An account with a browser session; returns (id, username, session token).
async fn account(pool: &PgPool) -> (Uuid, String, String) {
    let id = Uuid::new_v4();
    let username = tag("user");
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash_password("password123456").unwrap())
        .execute(pool)
        .await
        .unwrap();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    (id, username, token)
}

async fn fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-issues-ui-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let (owner, _, token) = account(&pool).await;
    let org_id = Uuid::new_v4();
    let org = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Org')")
        .bind(org_id)
        .bind(&org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    let repo = tag("repo");
    let (status, body) = api(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org}/repositories"),
        &token,
        json!({"name": repo}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Fixture {
        app,
        pool,
        storage_root,
        base: format!("/organizations/{org}/repositories/{repo}"),
        api_base: format!("/api/v1/organizations/{org}/repositories/{repo}"),
        cookie: format!("githearth_session={token}"),
        token,
        org_id,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_view_and_comment_through_forms() {
    let f = fixture().await;
    let (status, repo_page) = get(&f.app, &f.base, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(repo_page.contains(&format!("{}/issues\"", f.base)));

    let (status, form) = get(&f.app, &format!("{}/issues/new", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("<label for=\"title\">"));
    let token = csrf(&form);
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("title", "<script>x</script> broken"),
            ("body", "Steps & details"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(location, format!("{}/issues/1", f.base));

    let (status, page) = get(&f.app, &location, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("&lt;script&gt;x&lt;/script&gt; broken"));
    assert!(!page.contains("<script>x"));
    assert!(page.contains("Steps &amp; details"));
    assert!(page.contains("No comments yet."));

    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/issues/1/comments", f.base),
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("body", "first <b>comment</b>"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, page) = get(&f.app, &location.unwrap(), Some(&f.cookie)).await;
    assert!(page.contains("first &lt;b&gt;comment&lt;/b&gt;"));

    // The API sees exactly what the forms created, with the same audit events.
    let (_, comments) = api(
        &f.app,
        "GET",
        &format!("{}/issues/1/comments", f.api_base),
        &f.token,
        json!(null),
    )
    .await;
    assert_eq!(comments["items"].as_array().unwrap().len(), 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind IN ('issue.created','issue.commented')",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 2);

    let (_, list) = get(&f.app, &format!("{}/issues", f.base), Some(&f.cookie)).await;
    assert!(list.contains(&format!("{}/issues/1\"", f.base)));
    assert!(list.contains("<th scope=\"col\">Title</th>"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn issue_and_comment_lists_paginate() {
    let f = fixture().await;
    for i in 1..=31 {
        let (status, _) = api(
            &f.app,
            "POST",
            &format!("{}/issues", f.api_base),
            &f.token,
            json!({"title": format!("issue {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    for i in 1..=31 {
        let (status, _) = api(
            &f.app,
            "POST",
            &format!("{}/issues/1/comments", f.api_base),
            &f.token,
            json!({"body": format!("comment {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (_, first) = get(&f.app, &format!("{}/issues", f.base), Some(&f.cookie)).await;
    assert!(first.contains("issue 31<"));
    assert!(!first.contains("issue 1<"));
    let next = format!("{}/issues?state=open&amp;after=2", f.base);
    assert!(first.contains(&next), "{first}");
    let (_, second) = get(&f.app, &next.replace("&amp;", "&"), Some(&f.cookie)).await;
    assert!(second.contains("issue 1<"));
    assert!(second.contains("First page"));
    assert!(!second.contains("rel=\"next\""));

    let (_, closed) = get(
        &f.app,
        &format!("{}/issues?state=closed", f.base),
        Some(&f.cookie),
    )
    .await;
    assert!(closed.contains("No issues."));
    let (status, _) = get(
        &f.app,
        &format!("{}/issues?state=bogus", f.base),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, issue) = get(&f.app, &format!("{}/issues/1", f.base), Some(&f.cookie)).await;
    assert!(issue.contains("comment 30<"));
    assert!(!issue.contains("comment 31<"));
    let (_, more) = get(
        &f.app,
        &format!("{}/issues/1?after=30", f.base),
        Some(&f.cookie),
    )
    .await;
    assert!(more.contains("comment 31<"));
    assert!(more.contains("First comments"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn forms_require_session_csrf_and_valid_input() {
    let f = fixture().await;
    let (status, _) = get(&f.app, &format!("{}/issues", f.base), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &f.cookie,
        &[("csrf_token", "wrong"), ("title", "t")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &f.cookie,
        &[("title", "t")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, form) = get(&f.app, &format!("{}/issues/new", f.base), Some(&f.cookie)).await;
    let token = csrf(&form);
    let (status, _, page) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("title", "   "),
            ("body", "kept text"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("role=\"alert\""));
    assert!(page.contains("kept text"));

    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues/1/comments", f.base),
        &f.cookie,
        &[("csrf_token", &token), ("body", "hi")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&f.app, &format!("{}/issues/abc", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &f.cookie,
        &[("csrf_token", &token), ("title", "ok")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues/1/comments", f.base),
        &f.cookie,
        &[("csrf_token", &token), ("body", "  ")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_users_get_404_and_readers_can_open_issues() {
    let f = fixture().await;
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/issues", f.api_base),
        &f.token,
        json!({"title": "private"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, _, stranger) = account(&f.pool).await;
    let cookie = format!("githearth_session={stranger}");
    for path in ["/issues", "/issues/new", "/issues/1"] {
        let (status, _) = get(&f.app, &format!("{}{path}", f.base), Some(&cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (_, home) = get(&f.app, "/", Some(&cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &cookie,
        &[("csrf_token", &csrf(&home)), ("title", "x")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Policy lets the `read` role open issues, so the form works for a reader too.
    let (reader, _, reader_token) = account(&f.pool).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(f.org_id)
    .bind(reader)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(reader)
        .bind(f.org_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let cookie = format!("githearth_session={reader_token}");
    let (status, page) = get(&f.app, &format!("{}/issues/1", f.base), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/issues", f.base),
        &cookie,
        &[("csrf_token", &csrf(&page)), ("title", "x")],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    finish(f).await;
}
