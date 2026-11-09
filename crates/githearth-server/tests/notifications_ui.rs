//! Notifications page (browser) contract tests (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

fn csrf(html: &str) -> String {
    hidden(html, "csrf_token")
}

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

/// Adds a second signed-in account, returning (id, cookie).
async fn other_account(f: &Fixture) -> (Uuid, String) {
    let (id, _username, token) = account(&f.pool).await;
    (id, format!("githearth_session={token}"))
}

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    cookie: String,
    account_id: Uuid,
    org_id: Uuid,
    org_slug: String,
    repo_name: String,
    repo_id: Uuid,
}

async fn fixture() -> Fixture {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-notifications-ui-{}", Uuid::new_v4()));
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
    let (owner, _username, token) = account(&pool).await;
    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Org')")
        .bind(org_id)
        .bind(&org_slug)
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

    let repo_name = tag("repo");
    let repo_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,description,visibility,archived,status,operation_id,created_by) \
         VALUES ($1,$2,$3,'','private',false,'active',$4,$5)",
    )
    .bind(repo_id)
    .bind(org_id)
    .bind(&repo_name)
    .bind(operation_id)
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();

    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    Fixture {
        app,
        pool,
        storage_root,
        cookie: format!("githearth_session={token}"),
        account_id: owner,
        org_id,
        org_slug,
        repo_name,
        repo_id,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

/// Inserts a pull request row and returns (id, number).
async fn pull_request(f: &Fixture, title: &str, number: i32) -> (Uuid, i32) {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
         VALUES ($1,$2,$3,$4,'',$5,'feature','main')",
    )
    .bind(id)
    .bind(f.repo_id)
    .bind(number)
    .bind(title)
    .bind(f.account_id)
    .execute(&f.pool)
    .await
    .unwrap();
    (id, number)
}

/// Inserts a notification row for `account_id`, returning (id, event_id).
async fn notification(
    f: &Fixture,
    account_id: Uuid,
    kind: &str,
    pull_request_id: Uuid,
    actor_id: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    let event_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO notifications (id,account_id,kind,event_id,pull_request_id,actor_id) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(account_id)
    .bind(kind)
    .bind(event_id)
    .bind(pull_request_id)
    .bind(actor_id)
    .execute(&f.pool)
    .await
    .unwrap();
    id
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn signed_out_get_redirects_to_login() {
    let f = fixture().await;
    let (status, _) = get(&f.app, "/notifications", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_accessible_notification_shows_actor_target_and_escaped_title() {
    let f = fixture().await;
    let (actor_id, actor_name, _actor_token) = account(&f.pool).await;
    let (pr_id, number) = pull_request(&f, "<script>&\"title", 1).await;
    notification(
        &f,
        f.account_id,
        "pull_request.merged",
        pr_id,
        Some(actor_id),
    )
    .await;

    let (status, page) = get(&f.app, "/notifications", Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&actor_name), "{page}");
    assert!(page.contains("merged"), "{page}");
    assert!(
        page.contains(&format!("{}/{}", f.org_slug, f.repo_name)),
        "{page}"
    );
    assert!(page.contains(&format!("#{number}")), "{page}");
    assert!(
        page.contains(&format!(
            "/organizations/{}/repositories/{}/pulls/{}",
            f.org_slug, f.repo_name, number
        )),
        "{page}"
    );
    assert!(page.contains("&lt;script&gt;&amp;&quot;title"), "{page}");
    assert!(!page.contains("<script>&\"title"), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_notification_on_an_unreadable_repository_shows_no_details() {
    let f = fixture().await;
    let (pr_id, _number) = pull_request(&f, "secret title", 1).await;
    notification(&f, f.account_id, "pull_request.merged", pr_id, None).await;

    // Remove the viewer's membership and revoke their grant so the repository
    // becomes unreadable.
    sqlx::query("DELETE FROM memberships WHERE organization_id = $1 AND account_id = $2")
        .bind(f.org_id)
        .bind(f.account_id)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE grants SET revoked_at = clock_timestamp(), revoked_by = $1 \
         WHERE scope_kind = 'organization' AND scope_id = $2 AND subject_id = $3 AND revoked_at IS NULL",
    )
    .bind(f.account_id)
    .bind(f.org_id)
    .bind(f.account_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let (status, page) = get(&f.app, "/notifications", Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("A pull request you can no longer access"),
        "{page}"
    );
    assert!(!page.contains("secret title"), "{page}");
    assert!(!page.contains(&f.repo_name), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mark_read_sets_read_once_and_404s_another_accounts_id() {
    let f = fixture().await;
    let (pr_id, _number) = pull_request(&f, "t", 1).await;
    let notif_id = notification(&f, f.account_id, "pull_request.merged", pr_id, None).await;

    // Another account's notification (different notification id).
    let (other_id, other_cookie) = other_account(&f).await;
    let other_notif = notification(&f, other_id, "pull_request.merged", pr_id, None).await;

    let (_, page) = get(&f.app, "/notifications", Some(&f.cookie)).await;
    let my_csrf = csrf(&page);

    let (status, location, _) = post_form(
        &f.app,
        &format!("/notifications/{notif_id}/read"),
        &f.cookie,
        &[("csrf_token", &my_csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/notifications"));

    let first_read_at: String =
        sqlx::query_scalar("SELECT read_at::text FROM notifications WHERE id = $1")
            .bind(notif_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!first_read_at.is_empty());

    // A second POST keeps the first read_at.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/notifications/{notif_id}/read"),
        &f.cookie,
        &[("csrf_token", &my_csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let second_read_at: String =
        sqlx::query_scalar("SELECT read_at::text FROM notifications WHERE id = $1")
            .bind(notif_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(first_read_at, second_read_at);

    // Another account trying to mark my notification read gets 404.
    let (_, other_page) = get(&f.app, "/notifications", Some(&other_cookie)).await;
    let other_csrf = csrf(&other_page);
    let (status, _, _) = post_form(
        &f.app,
        &format!("/notifications/{notif_id}/read"),
        &other_cookie,
        &[("csrf_token", &other_csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And the other account's own notification stays unread.
    let other_read: Option<String> =
        sqlx::query_scalar("SELECT read_at::text FROM notifications WHERE id = $1")
            .bind(other_notif)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(other_read.is_none(), "{other_read:?}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mark_all_read_only_touches_the_viewers_rows() {
    let f = fixture().await;
    let (pr_id, _number) = pull_request(&f, "t", 1).await;
    notification(&f, f.account_id, "pull_request.merged", pr_id, None).await;
    notification(&f, f.account_id, "pull_request.reviewed", pr_id, None).await;

    let (other_id, _other_cookie) = other_account(&f).await;
    notification(&f, other_id, "pull_request.merged", pr_id, None).await;

    let (_, page) = get(&f.app, "/notifications", Some(&f.cookie)).await;
    let csrf = csrf(&page);

    let (status, location, _) = post_form(
        &f.app,
        "/notifications/read-all",
        &f.cookie,
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/notifications"));

    let mine_unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND read_at IS NULL",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(mine_unread, 0);

    let other_unread: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND read_at IS NULL",
    )
    .bind(other_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(other_unread, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_bad_csrf_token_is_refused() {
    let f = fixture().await;
    let (pr_id, _number) = pull_request(&f, "t", 1).await;
    let notif_id = notification(&f, f.account_id, "pull_request.merged", pr_id, None).await;

    let (status, _, _) = post_form(
        &f.app,
        &format!("/notifications/{notif_id}/read"),
        &f.cookie,
        &[("csrf_token", "wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let read: Option<String> =
        sqlx::query_scalar("SELECT read_at::text FROM notifications WHERE id = $1")
            .bind(notif_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(read.is_none(), "{read:?}");

    // read-all with a bad CSRF token is also refused.
    let (status, _, _) = post_form(
        &f.app,
        "/notifications/read-all",
        &f.cookie,
        &[("csrf_token", "wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    finish(f).await;
}
