//! Browser contract tests for password change and administrator reset (M4.1
//! U02). PostgreSQL-backed.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::PgPool;
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

async fn account(pool: &PgPool, is_admin: bool) -> (Uuid, String, String) {
    let id = Uuid::new_v4();
    let username = tag("user");
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin,password_changed_at) \
         VALUES ($1,$2,$3,$4,clock_timestamp())",
    )
    .bind(id)
    .bind(&username)
    .bind(hash_password("password123456").unwrap())
    .bind(is_admin)
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

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: std::path::PathBuf,
    admin_cookie: String,
    user_cookie: String,
    user_token: String,
}

async fn fixture() -> Fixture {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-passwords-ui-{}", Uuid::new_v4()));
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
    let (_admin_id, _admin_name, admin_token) = account(&pool, true).await;
    let (_user_id, _user_name, user_token) = account(&pool, false).await;
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    Fixture {
        app,
        pool,
        storage_root,
        admin_cookie: format!("githearth_session={admin_token}"),
        user_cookie: format!("githearth_session={user_token}"),
        user_token,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

async fn api_login(app: &Router, username: &str, password: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"username": username, "password": password}).to_string(),
        ))
        .unwrap();
    send(app, request).await.0
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_administrator_resets_a_password_through_the_form() {
    let f = fixture().await;
    let (_id, name, _token) = account(&f.pool, false).await;
    let (_, page) = get(&f.app, "/admin/accounts", Some(&f.admin_cookie)).await;
    let target: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&name)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{target}/password"),
        &f.admin_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("new_password", "reset-secret-password"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        api_login(&f.app, &name, "password123456").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api_login(&f.app, &name, "reset-secret-password").await,
        StatusCode::OK
    );

    // A non-administrator's reset form post is refused.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{target}/password"),
        &f.user_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("new_password", "should-not-apply"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn own_password_change_page_requires_the_current_password_and_signs_out() {
    let f = fixture().await;
    let (_, page) = get(&f.app, "/settings/password", Some(&f.user_cookie)).await;
    assert!(page.contains("Change password"), "{page}");

    // Mismatched confirmation re-renders without changing anything.
    let (status, _, body) = post_form(
        &f.app,
        "/settings/password",
        &f.user_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("current_password", "password123456"),
            ("new_password", "replacement-secret"),
            ("confirm_password", "different-secret"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("do not match"), "{body}");

    // A wrong current password is refused.
    let (_, page) = get(&f.app, "/settings/password", Some(&f.user_cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/password",
        &f.user_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("current_password", "wrong-password"),
            ("new_password", "replacement-secret"),
            ("confirm_password", "replacement-secret"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("current password is incorrect"), "{body}");

    // The correct current password changes it and signs the session out.
    let (_, page) = get(&f.app, "/settings/password", Some(&f.user_cookie)).await;
    let (status, location, _) = post_form(
        &f.app,
        "/settings/password",
        &f.user_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("current_password", "password123456"),
            ("new_password", "replacement-secret"),
            ("confirm_password", "replacement-secret"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/login"));

    // The session token is no longer accepted.
    let (status, _, _) = send(
        &f.app,
        Request::builder()
            .uri("/api/v1/me")
            .header("authorization", format!("Bearer {}", f.user_token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn signed_out_password_page_redirects() {
    let f = fixture().await;
    let (status, _, _) = send(
        &f.app,
        Request::builder()
            .uri("/settings/password")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    finish(f).await;
}
