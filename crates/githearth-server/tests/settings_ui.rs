//! Account settings page (access tokens and SSH keys) contract tests
//! (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::Path;
use std::process::Command;
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

fn key(html: &str) -> String {
    hidden(html, "idempotency_key")
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

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: std::path::PathBuf,
    cookie: String,
    account_id: Uuid,
}

async fn fixture() -> Fixture {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-settings-ui-{}", Uuid::new_v4()));
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
    let (id, _username, token) = account(&pool).await;
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    Fixture {
        app,
        pool,
        storage_root,
        cookie: format!("githearth_session={token}"),
        account_id: id,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

/// A second signed-in account for isolation checks, returning its cookie.
async fn other_cookie(f: &Fixture) -> String {
    let (_id, _username, token) = account(&f.pool).await;
    format!("githearth_session={token}")
}

// ---------------------------------------------------------------------------
// SSH key generation
// ---------------------------------------------------------------------------

fn temp_key(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("githearth-settings-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(label)
}

fn generate_key(key_type: &str, path: &Path) {
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            key_type,
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "githearth-test",
            "-q",
        ])
        .output()
        .expect("ssh-keygen failed");
    assert!(
        output.status.success(),
        "ssh-keygen -t {key_type}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn public_key_line(path: &Path) -> String {
    std::fs::read_to_string(path.with_extension("pub")).unwrap()
}

fn key_fingerprint(path: &Path) -> String {
    let output = Command::new("ssh-keygen")
        .args(["-lf", &path.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn signed_out_get_redirects_to_login() {
    let f = fixture().await;
    let (status, _) = get(&f.app, "/settings", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn page_lists_only_the_accounts_tokens_and_keys() {
    let f = fixture().await;

    // Seed one token and one key for the session account.
    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, _page) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "ci token"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, _page) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "ci key"),
            ("public_key", &public_key),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Another account has its own token/key with a name the first account's page
    // must not contain.
    let bob_cookie = other_cookie(&f).await;
    let (_, bob_page) = get(&f.app, "/settings", Some(&bob_cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        "/settings/tokens",
        &bob_cookie,
        &[
            ("csrf_token", &csrf(&bob_page)),
            ("idempotency_key", &key(&bob_page)),
            ("name", "bobs secret token"),
            ("scope_write", "repo:write"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("ci token"), "{page}");
    assert!(page.contains("ci key"), "{page}");
    assert!(!page.contains("bobs secret token"), "{page}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_a_token_shows_its_plaintext_exactly_once() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, location, body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "ci token"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(location, None, "a token create must not redirect");
    assert!(
        body.contains("Copy it now; it won't be shown again."),
        "{body}"
    );
    let marker = "<pre style=\"white-space:pre-wrap;word-break:break-all\">";
    let start = body.find(marker).unwrap() + marker.len();
    let rest = &body[start..];
    let end = rest.find("</pre>").unwrap();
    let plaintext = rest[..end].trim().to_owned();
    assert!(plaintext.starts_with("gth_"), "{plaintext}");
    assert_eq!(plaintext.len(), 64 + 4);

    // A later GET does not contain the plaintext.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert!(!page.contains(&plaintext), "{page}");

    // The token works as a Bearer credential on the API.
    let (status, _, _) = send(
        &f.app,
        Request::builder()
            .method("GET")
            .uri("/api/v1/me")
            .header("authorization", format!("Bearer {plaintext}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn double_submitting_the_same_token_form_creates_one_token() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let csrf = csrf(&page);
    let key = key(&page);
    let form = [
        ("csrf_token", csrf.as_str()),
        ("idempotency_key", key.as_str()),
        ("name", "dedupe token"),
        ("scope_read", "repo:read"),
    ];

    let (status, _, body) = post_form(&f.app, "/settings/tokens", &f.cookie, &form).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Copy it now; it won't be shown again."),
        "{body}"
    );
    assert!(body.contains("gth_"), "{body}");

    let (status, _, body) = post_form(&f.app, "/settings/tokens", &f.cookie, &form).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            "This token was already created. Its value was shown once and cannot be shown again."
        ),
        "{body}"
    );
    assert!(!body.contains("gth_"), "{body}");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM access_tokens WHERE account_id = $1")
        .bind(f.account_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_bad_token_name_or_scope_re_renders_with_a_notice() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    // No scopes selected.
    let (status, _, body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "kept name"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("kept name"), "{body}");

    // Empty name.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "  "),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("role=\"alert\""), "{body}");

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM access_tokens WHERE account_id = $1 AND name = 'kept name'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn adding_an_ssh_key_redirects_and_lists_the_fingerprint() {
    let f = fixture().await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    let expected_fp = key_fingerprint(&key_path);

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, location, _) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "laptop"),
            ("public_key", &public_key),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/settings"));

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert!(page.contains("laptop"), "{page}");
    assert!(page.contains(&expected_fp), "{page}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_rsa_key_re_renders_with_a_notice_keeping_the_name() {
    let f = fixture().await;

    let rsa_path = temp_key("id_rsa");
    generate_key("rsa", &rsa_path);
    let rsa_line = public_key_line(&rsa_path);

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "rsa laptop"),
            ("public_key", &rsa_line),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("rsa laptop"), "{body}");
    // The entered key text is kept.
    let ssh3 = rsa_line.split_whitespace().nth(1).unwrap();
    assert!(body.contains(ssh3), "{body}");

    let _ = std::fs::remove_dir_all(rsa_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_duplicate_ssh_key_shows_a_notice() {
    let f = fixture().await;

    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);

    // First registration by the session account.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "first"),
            ("public_key", &public_key),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // A second account registering the same key gets a 409 rendered as a notice.
    let bob_cookie = other_cookie(&f).await;
    let (_, page) = get(&f.app, "/settings", Some(&bob_cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &bob_cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "duplicate"),
            ("public_key", &public_key),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("already registered"), "{body}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn revoking_a_token_and_a_key_redirects_and_marks_them_revoked() {
    let f = fixture().await;

    // Create a token.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (_, _, _body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "torevoke"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    let id: Uuid = sqlx::query_scalar(
        "SELECT t.id FROM access_tokens t WHERE t.account_id = $1 AND t.name = 'torevoke'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let token = csrf(&page);
    let (status, location, _) = post_form(
        &f.app,
        &format!("/settings/tokens/{id}/revoke"),
        &f.cookie,
        &[("csrf_token", &token)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/settings"));

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert!(page.contains("Revoked"), "{page}");

    // Create an SSH key and revoke it.
    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (_, _, _) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "keytorevoke"),
            ("public_key", &public_key),
        ],
    )
    .await;
    let key_id: Uuid = sqlx::query_scalar(
        "SELECT k.id FROM ssh_keys k WHERE k.account_id = $1 AND k.name = 'keytorevoke'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, location, _) = post_form(
        &f.app,
        &format!("/settings/ssh-keys/{key_id}/revoke"),
        &f.cookie,
        &[("csrf_token", &csrf(&page))],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/settings"));

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert!(page.contains("Revoked"), "{page}");
    // The revoked rows no longer offer a Revoke button.
    assert!(!page.contains(">Revoke</button>"), "{page}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn revoking_another_accounts_id_is_not_found() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (_, _, _) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "alices token"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    let id: Uuid = sqlx::query_scalar(
        "SELECT t.id FROM access_tokens t WHERE t.account_id = $1 AND t.name = 'alices token'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    let bob_cookie = other_cookie(&f).await;
    let (_, page) = get(&f.app, "/settings", Some(&bob_cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        &format!("/settings/tokens/{id}/revoke"),
        &bob_cookie,
        &[("csrf_token", &csrf(&page))],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_wrong_csrf_token_changes_nothing() {
    let f = fixture().await;

    // A create with a wrong CSRF token is refused and inserts nothing.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let k = key(&page);
    let (status, _, _) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", "wrong"),
            ("idempotency_key", &k),
            ("name", "blocked"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM access_tokens WHERE account_id = $1 AND name = 'blocked'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    // A revoke with a wrong CSRF token is refused and leaves the row intact.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (_, _, _) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "mine"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM access_tokens WHERE account_id = $1 AND name = 'mine'")
            .bind(f.account_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let (status, _, _) = post_form(
        &f.app,
        &format!("/settings/tokens/{id}/revoke"),
        &f.cookie,
        &[("csrf_token", "wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let revoked: Option<String> =
        sqlx::query_scalar("SELECT revoked_at::text FROM access_tokens WHERE id = $1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(revoked.is_none());

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn user_supplied_names_are_escaped() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "<script>alert(1)</script>"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{page}"
    );
    assert!(!page.contains("<script>alert(1)</script>"), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_bad_expiry_re_renders_with_a_notice() {
    let f = fixture().await;

    // Token form: a non-numeric expiry.
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "expiry token"),
            ("scope_read", "repo:read"),
            ("expires_in_days", "abc"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("expiry token"), "{body}");

    // SSH key form: an out-of-range expiry.
    let key_path = temp_key("id_ed25519");
    generate_key("ed25519", &key_path);
    let public_key = public_key_line(&key_path);
    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/ssh-keys",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("name", "expiry key"),
            ("public_key", &public_key),
            ("expires_in_days", "0"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("role=\"alert\""), "{body}");
    assert!(body.contains("expiry key"), "{body}");

    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_missing_idempotency_key_is_refused() {
    let f = fixture().await;

    let (_, page) = get(&f.app, "/settings", Some(&f.cookie)).await;
    let (status, _, body) = post_form(
        &f.app,
        "/settings/tokens",
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("name", "no key"),
            ("scope_read", "repo:read"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("Reload the page and submit the form again."),
        "{body}"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM access_tokens WHERE account_id = $1 AND name = 'no key'",
    )
    .bind(f.account_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);

    finish(f).await;
}
