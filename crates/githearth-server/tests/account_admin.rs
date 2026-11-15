//! Account administration acceptance tests (M4.1, U01–U03), PostgreSQL-backed.
//!
//! Every test creates its own database and storage root, so the file is safe to
//! run repeatedly and in parallel. Failed-sign-in windows are advanced by
//! ageing the stored attempt timestamps, never by sleeping.

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_PASSWORD: &str = "administratorpassword";
const MEMBER_PASSWORD: &str = "memberpasswordvalue";
const NEW_PASSWORD: &str = "replacementpassword1";

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL to a disposable database");
    let (origin, _) = url.rsplit_once('/').expect("URL must end in database name");
    let maint = PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maint)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maint)
        .await
        .unwrap();
    format!("{origin}/{name}")
}

/// Each fixture opens its own pool. Tests run in parallel by default, so a
/// small process-wide bound keeps the suite under the PostgreSQL server's
/// connection limit without serialising every test.
static FIXTURE_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn fixture_slots() -> Arc<Semaphore> {
    FIXTURE_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(4)))
        .clone()
}

/// One isolated server instance: its own database, storage root and config.
struct Fixture {
    app: Router,
    pool: PgPool,
    state: AppState,
    config: Config,
    storage_root: PathBuf,
    /// Held for the fixture's lifetime so only a few fixtures are live at once.
    _slot: OwnedSemaphorePermit,
}

impl Fixture {
    /// Builds a fixture, letting `adjust` see the config and the storage root.
    async fn from_config(adjust: impl FnOnce(&mut Config, &std::path::Path)) -> Self {
        let slot = fixture_slots()
            .acquire_owned()
            .await
            .expect("fixture semaphore is open");
        let db_name = format!(
            "githearth_acct_{}",
            Uuid::new_v4().to_string().replace('-', "")
        );
        let database_url = fresh_database(&db_name).await;
        // The storage root must leave room for <root>/run/hook-XXXXXXXX.sock.
        let short = Uuid::new_v4().simple().to_string();
        let storage_root = std::env::temp_dir().join(format!("gha{}", &short[..12]));
        std::fs::create_dir_all(&storage_root).unwrap();
        let mut config = Config {
            database_url,
            database_max_connections: 4,
            database_timeout_seconds: 3,
            storage_root: storage_root.clone(),
            login_throttle_max_per_username: 2,
            login_throttle_max_per_client: 1000,
            login_throttle_window_seconds: 900,
            ..Config::default()
        };
        adjust(&mut config, &storage_root);
        let pool = connect(&config).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
        let app = router(state.clone());
        Self {
            app,
            pool,
            state,
            config,
            storage_root,
            _slot: slot,
        }
    }

    async fn with_config(adjust: impl FnOnce(&mut Config)) -> Self {
        Self::from_config(|config, _| adjust(config)).await
    }

    async fn new() -> Self {
        Self::from_config(|_, _| {}).await
    }

    async fn finish(self) {
        let url = self.config.database_url.clone();
        let _ = std::fs::remove_dir_all(&self.storage_root);
        self.pool.close().await;
        // Drop this fixture's database too, so a repeated run does not exhaust
        // the shared server's catalog or connections.
        if let Some((origin, name)) = url.rsplit_once('/')
            && let Ok(maint) = PgPool::connect(&format!("{origin}/postgres")).await
        {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&maint)
                .await;
            maint.close().await;
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Sends a request from a fixed client address, as the router's ConnectInfo
/// layer does for a real connection.
async fn send_from(
    app: &Router,
    mut request: Request<Body>,
    ip: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let address: SocketAddr = format!("{ip}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(address));
    send(app, request).await
}

fn json_request(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn login_from(
    app: &Router,
    username: &str,
    password: &str,
    ip: Option<&str>,
) -> (StatusCode, String) {
    let request = json_request(
        "POST",
        "/api/v1/login",
        None,
        Some(serde_json::json!({"username": username, "password": password})),
    );
    let (status, _, body) = match ip {
        Some(ip) => send_from(app, request, ip).await,
        None => send(app, request).await,
    };
    let token = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["token"].as_str().map(str::to_owned))
        .unwrap_or(body);
    (status, token)
}

async fn login(app: &Router, username: &str, password: &str) -> (StatusCode, String) {
    login_from(app, username, password, None).await
}

async fn login_ok(app: &Router, username: &str, password: &str) -> String {
    let (status, token) = login(app, username, password).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    token
}

async fn get_json(app: &Router, path: &str, token: &str) -> (StatusCode, String) {
    let (status, _, body) = send(app, json_request("GET", path, Some(token), None)).await;
    (status, body)
}

/// A form submit carrying a CSRF token and an optional session cookie.
async fn post_form(
    app: &Router,
    path: &str,
    cookie: Option<&str>,
    fields: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut builder =
        Request::post(path).header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    send(app, builder.body(Body::from(body)).unwrap()).await
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The CSRF token a page carries for the signed-in session.
fn csrf_from(page: &str) -> String {
    // Every form starts with the hidden CSRF field; take the first value.
    let marker = "name=\"csrf_token\" value=\"";
    let start = page.find(marker).expect("page must carry a CSRF field") + marker.len();
    page[start..].split('"').next().unwrap().to_owned()
}

// ---------------------------------------------------------------------------
// Fixtures written directly to the database
// ---------------------------------------------------------------------------

async fn insert_account(pool: &PgPool, username: &str, password: &str, is_admin: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(username)
    .bind(hash_password(password).unwrap())
    .bind(is_admin)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A signed-in fixture account: one administrator and one plain member.
struct Members {
    admin_id: Uuid,
    admin_username: String,
    admin_token: String,
    member_id: Uuid,
    member_username: String,
    member_token: String,
}

async fn members(f: &Fixture) -> Members {
    let admin_username = tag("admin");
    let admin_id = insert_account(&f.pool, &admin_username, ADMIN_PASSWORD, true).await;
    let admin_token = login_ok(&f.app, &admin_username, ADMIN_PASSWORD).await;
    let member_username = tag("member");
    let member_id = insert_account(&f.pool, &member_username, MEMBER_PASSWORD, false).await;
    let member_token = login_ok(&f.app, &member_username, MEMBER_PASSWORD).await;
    Members {
        admin_id,
        admin_username,
        admin_token,
        member_id,
        member_username,
        member_token,
    }
}

async fn audit_kinds(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM audit_events ORDER BY seq")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn outbox_kinds(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT kind FROM outbox_events ORDER BY created_at, id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn create_request(admin_token: &str, username: &str, password: &str) -> Request<Body> {
    json_request(
        "POST",
        "/api/v1/admin/accounts",
        Some(admin_token),
        Some(serde_json::json!({"username": username, "password": password})),
    )
}

// ---------------------------------------------------------------------------
// U01: create, list, disable, re-enable
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn administrator_creates_disables_and_re_enables_an_account() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let username = tag("created");

    let (status, _, body) = send(
        &f.app,
        create_request(&m.admin_token, &username, MEMBER_PASSWORD),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["username"], username.as_str());
    assert_eq!(created["suspended"], false);
    assert_eq!(created["is_admin"], false);
    // No password, hash or credential of any kind is ever returned. The field
    // name `must_change_password` is documentation, not a secret.
    for forbidden in [MEMBER_PASSWORD, "password_hash", "hash", "token"] {
        assert!(
            !body.contains(forbidden),
            "response leaked {forbidden}: {body}"
        );
    }

    // The new account can sign in with the initial password.
    let initial_session = login_ok(&f.app, &username, MEMBER_PASSWORD).await;

    // It appears in the administrator's list, and the list carries no password.
    let (status, list) = get_json(&f.app, "/api/v1/admin/accounts?limit=100", &m.admin_token).await;
    assert_eq!(status, StatusCode::OK);
    let accounts: Vec<serde_json::Value> = serde_json::from_str(&list).unwrap();
    assert!(accounts.iter().any(|a| a["username"] == username.as_str()));
    assert!(!list.contains(MEMBER_PASSWORD));

    // Disable: the account is signed out and cannot sign in again.
    let id = created["id"].as_str().unwrap().to_owned();
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{id}/disable"),
            Some(&m.admin_token),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT suspended FROM accounts WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &initial_session).await.0,
        StatusCode::UNAUTHORIZED,
        "a disabled account's session is refused"
    );
    assert_eq!(
        login(&f.app, &username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "a disabled account cannot sign in"
    );
    // Disabling twice is the state the caller asked for, not an error.
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{id}/disable"),
                Some(&m.admin_token),
                None,
            ),
        )
        .await
        .0,
        StatusCode::OK
    );

    // Re-enable: sign-in works again, the revoked session does not.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{id}/enable"),
            Some(&m.admin_token),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = login_ok(&f.app, &username, MEMBER_PASSWORD).await;
    assert_ne!(restored, initial_session);
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &initial_session).await.0,
        StatusCode::UNAUTHORIZED,
        "re-enabling must not restore a revoked session"
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &restored).await.0,
        StatusCode::OK
    );

    // Every mutation wrote its audit record and outbox event in the same
    // transaction as the change; a no-op disable adds none.
    let kinds = audit_kinds(&f.pool).await;
    let outbox = outbox_kinds(&f.pool).await;
    for kind in ["account.created", "account.disabled", "account.enabled"] {
        assert!(
            kinds.iter().any(|k| k == kind),
            "missing {kind} in {kinds:?}"
        );
        assert!(
            outbox.iter().any(|k| k == kind),
            "missing {kind} in {outbox:?}"
        );
    }
    assert_eq!(
        kinds.iter().filter(|k| *k == "account.disabled").count(),
        1,
        "a repeated disable records nothing new: {kinds:?}"
    );
    // Exactly one outbox row per audit row, sharing its id.
    let unpaired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events o LEFT JOIN audit_events a ON a.id = o.id \
         WHERE a.id IS NULL",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(unpaired, 0);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn account_creation_validates_input_and_rejects_duplicates() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    // A short password and an invalid username are refused, and the refusal
    // never repeats the password.
    let short = tag("short");
    for (username, password) in [
        (short.as_str(), "too short"),
        ("AB", MEMBER_PASSWORD),
        ("Mixed-Case", MEMBER_PASSWORD),
    ] {
        let (status, _, body) =
            send(&f.app, create_request(&m.admin_token, username, password)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!body.contains(password));
    }

    // An unknown field is refused rather than silently ignored.
    let (status, _, _) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/admin/accounts",
            Some(&m.admin_token),
            Some(serde_json::json!({
                "username": tag("typo"),
                "password": MEMBER_PASSWORD,
                "is_admin": true,
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A duplicate username is a conflict, and the attempted password is not echoed.
    let username = tag("dup");
    assert_eq!(
        send(
            &f.app,
            create_request(&m.admin_token, &username, MEMBER_PASSWORD)
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let (status, _, body) = send(
        &f.app,
        create_request(&m.admin_token, &username, MEMBER_PASSWORD),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!body.contains(MEMBER_PASSWORD));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM accounts WHERE username = $1")
            .bind(&username)
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        1
    );

    // Concurrent creates of one username: exactly one wins.
    let racing = tag("race");
    let first = send(
        &f.app,
        create_request(&m.admin_token, &racing, MEMBER_PASSWORD),
    );
    let second = send(
        &f.app,
        create_request(&m.admin_token, &racing, MEMBER_PASSWORD),
    );
    let (one, two) = tokio::join!(first, second);
    let created = [one.0, two.0]
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    let conflicted = [one.0, two.0]
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!((created, conflicted), (1, 1));
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn account_list_uses_keyset_pagination() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    for _ in 0..3 {
        let username = tag("page");
        assert_eq!(
            send(
                &f.app,
                create_request(&m.admin_token, &username, MEMBER_PASSWORD)
            )
            .await
            .0,
            StatusCode::CREATED
        );
    }

    let (status, body) = get_json(&f.app, "/api/v1/admin/accounts?limit=2", &m.admin_token).await;
    assert_eq!(status, StatusCode::OK);
    let first: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(first.len(), 2);
    let last_id = first[1]["id"].as_str().unwrap().to_owned();
    assert!(first[0]["id"].as_str().unwrap() < last_id.as_str());

    let (status, body) = get_json(
        &f.app,
        &format!("/api/v1/admin/accounts?limit=100&after={last_id}"),
        &m.admin_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(!second.is_empty());
    for account in &second {
        assert!(account["id"].as_str().unwrap() > last_id.as_str());
    }
    // The cursor excludes the account it names, so pages never overlap.
    assert!(!second.iter().any(|a| a["id"] == last_id.as_str()));

    // An out-of-range limit is invalid input, like the organization list.
    let (status, _, _) = send(
        &f.app,
        json_request(
            "GET",
            "/api/v1/admin/accounts?limit=0",
            Some(&m.admin_token),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_administrators_are_refused_every_administration_endpoint() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    // A member's session is refused wherever an administrator is required, with
    // the codebase's standard `403 not permitted`.
    for request in [
        create_request(&m.member_token, &tag("nope"), MEMBER_PASSWORD),
        json_request("GET", "/api/v1/admin/accounts", Some(&m.member_token), None),
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{}/disable", m.admin_id),
            Some(&m.member_token),
            None,
        ),
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{}/enable", m.admin_id),
            Some(&m.member_token),
            None,
        ),
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{}/password-reset", m.admin_id),
            Some(&m.member_token),
            Some(serde_json::json!({})),
        ),
    ] {
        let (status, _, body) = send(&f.app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body, r#"{"error":"not permitted"}"#);
    }
    // Nothing was created or changed by the refused calls.
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE kind LIKE 'account.%'"
        )
        .fetch_one(&f.pool)
        .await
        .unwrap(),
        0
    );
    login_ok(&f.app, &m.admin_username, ADMIN_PASSWORD).await;
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT NOT suspended FROM accounts WHERE id = $1")
            .bind(m.member_id)
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );

    // An access token is not a session and cannot administer accounts, even
    // when it belongs to an administrator.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.admin_token),
            Some(serde_json::json!({"name": "admin token", "scopes": ["repo:write"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _, body) = send(
        &f.app,
        json_request("GET", "/api/v1/admin/accounts", Some(&token), None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // An anonymous caller is unauthorized, not forbidden.
    let (status, _, _) = send(
        &f.app,
        json_request("GET", "/api/v1/admin/accounts", None, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_administrator_cannot_disable_themselves_or_the_last_administrator() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    // Own account: refused even though another administrator exists.
    insert_account(&f.pool, &tag("second"), ADMIN_PASSWORD, true).await;
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{}/disable", m.admin_id),
            Some(&m.admin_token),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("own account"));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT NOT suspended FROM accounts WHERE id = $1")
            .bind(m.admin_id)
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );

    // The last enabled administrator cannot be disabled. This is the guard two
    // concurrent administrators rely on when they disable each other; the
    // actor here is an already disabled administrator so the rule is reachable
    // directly.
    let disabled_caller = insert_account(&f.pool, &tag("caller"), ADMIN_PASSWORD, true).await;
    sqlx::query("UPDATE accounts SET suspended = true WHERE id = $1")
        .bind(disabled_caller)
        .execute(&f.pool)
        .await
        .unwrap();
    let only_enabled = insert_account(&f.pool, &tag("last"), ADMIN_PASSWORD, true).await;
    sqlx::query("UPDATE accounts SET suspended = true WHERE is_admin AND id <> $1")
        .bind(only_enabled)
        .execute(&f.pool)
        .await
        .unwrap();
    let error =
        githearth_server::account_admin::disable_inner(&f.state, disabled_caller, only_enabled)
            .await
            .unwrap_err();
    assert_eq!(error.status_code(), StatusCode::CONFLICT);
    assert_eq!(
        error.message(),
        "the last enabled administrator cannot be disabled"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT NOT suspended FROM accounts WHERE id = $1")
            .bind(only_enabled)
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );
    // An unknown account is a 404, not a conflict.
    let missing =
        githearth_server::account_admin::disable_inner(&f.state, disabled_caller, Uuid::new_v4())
            .await
            .unwrap_err();
    assert_eq!(missing.status_code(), StatusCode::NOT_FOUND);
    // Enabling an unknown account is also a 404, and enabling an enabled one is
    // the state the caller asked for.
    let missing =
        githearth_server::account_admin::enable_inner(&f.state, only_enabled, Uuid::new_v4())
            .await
            .unwrap_err();
    assert_eq!(missing.status_code(), StatusCode::NOT_FOUND);
    githearth_server::account_admin::enable_inner(&f.state, only_enabled, only_enabled)
        .await
        .expect("enabling an enabled account is not an error");

    // Concurrent administrators disabling each other: exactly one succeeds and
    // an enabled administrator always remains. Only `a` and `b` are enabled
    // here, so exactly one of them is the last enabled administrator.
    sqlx::query("UPDATE accounts SET suspended = true WHERE is_admin")
        .execute(&f.pool)
        .await
        .unwrap();
    let a = insert_account(&f.pool, &tag("far"), ADMIN_PASSWORD, true).await;
    let b = insert_account(&f.pool, &tag("bar"), ADMIN_PASSWORD, true).await;
    let (one, two) = tokio::join!(
        githearth_server::account_admin::disable_inner(&f.state, a, b),
        githearth_server::account_admin::disable_inner(&f.state, b, a)
    );
    assert_eq!(
        [&one, &two].iter().filter(|r| r.is_ok()).count(),
        1,
        "exactly one disable must win"
    );
    assert!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM accounts WHERE is_admin AND NOT suspended"
        )
        .fetch_one(&f.pool)
        .await
        .unwrap()
            >= 1,
        "an enabled administrator always remains"
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U02: password change and administrator reset revoke sessions
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn own_password_change_requires_the_current_password_and_revokes_other_sessions() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let second = login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "own change", "scopes": ["repo:read"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pat = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let change = |current: &str, new: &str| {
        json_request(
            "POST",
            "/api/v1/user/password",
            Some(&m.member_token),
            Some(serde_json::json!({"current_password": current, "new_password": new})),
        )
    };

    // A wrong current password is refused and revokes nothing.
    let (status, _, body) = send(&f.app, change("wrong current password", NEW_PASSWORD)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains(NEW_PASSWORD));
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &second).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &m.member_token).await.0,
        StatusCode::OK
    );

    // Too short a new password is invalid input, before anything changes.
    assert_eq!(
        send(&f.app, change(MEMBER_PASSWORD, "short")).await.0,
        StatusCode::BAD_REQUEST
    );

    // Changing the password of an account that no longer exists (or is
    // disabled) is unauthorized, and nothing is stored.
    let orphan = Uuid::new_v4();
    let error = githearth_server::account_admin::change_password(
        &f.state,
        orphan,
        None,
        MEMBER_PASSWORD,
        githearth_server::account_admin::Secret::new(NEW_PASSWORD),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status_code(), StatusCode::UNAUTHORIZED);

    // The correct current password changes it and keeps this session only.
    let (status, _, body) = send(&f.app, change(MEMBER_PASSWORD, NEW_PASSWORD)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_empty());
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &m.member_token).await.0,
        StatusCode::OK,
        "the session that made the change stays signed in"
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &second).await.0,
        StatusCode::UNAUTHORIZED,
        "other sessions are revoked"
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED,
        "access tokens are revoked by an own password change"
    );
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;

    // The change is audited, and no password appears in any audit payload.
    let payloads: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind = 'account.password_changed'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["account_id"], m.member_id.to_string());
    assert!(!payloads[0].to_string().contains(NEW_PASSWORD));
    assert!(!payloads[0].to_string().contains(MEMBER_PASSWORD));
    f.finish().await;
}

async fn request_reset(f: &Fixture, admin_token: &str, id: Uuid) -> (StatusCode, String) {
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{id}/password-reset"),
            Some(admin_token),
            Some(serde_json::json!({})),
        ),
    )
    .await;
    (status, body)
}

fn reset_code(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .unwrap_or_else(|_| panic!("reset response is not JSON: {body}"))["code"]
        .as_str()
        .unwrap_or_else(|| panic!("reset response has no code: {body}"))
        .to_owned()
}

/// The code the administration page shows once, in its single `<pre>` block.
fn reset_code_from_page(page: &str) -> String {
    let marker = "role=\"alert\">";
    let start = page
        .find(marker)
        .unwrap_or_else(|| panic!("the reset page has no code block: {page}"))
        + marker.len();
    page[start..]
        .split('<')
        .next()
        .expect("code block ends")
        .trim()
        .to_owned()
}

async fn redeem(f: &Fixture, username: &str, code: &str, password: &str) -> (StatusCode, String) {
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/password-reset/redeem",
            None,
            Some(serde_json::json!({
                "username": username,
                "code": code,
                "new_password": password,
            })),
        ),
    )
    .await;
    (status, body)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn administrator_password_reset_revokes_every_credential() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let other = login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "reset target", "scopes": ["repo:write"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pat = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(pat.starts_with("gth_"));

    let (status, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = reset_code(&body);
    assert_eq!(code.len(), 64);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(parsed["expires_at"].is_string(), "{body}");
    // The administrator never supplies or receives a password.
    assert!(!body.contains(MEMBER_PASSWORD));
    assert!(!body.contains(NEW_PASSWORD));

    // Every existing credential of the target account is refused.
    for token in [&m.member_token, &other] {
        assert_eq!(
            get_json(&f.app, "/api/v1/me", token).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED,
        "a personal access token must not survive a reset"
    );
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "the old password is refused until the code is redeemed"
    );
    // The browser is signed out too, and the flag is what keeps the account
    // out until it sets a password of its own.
    let (status, headers, _) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", format!("githearth_session={}", m.member_token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/login"
    );
    let must_change: bool =
        sqlx::query_scalar("SELECT must_change_password FROM accounts WHERE id = $1")
            .bind(m.member_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(must_change);

    // The one-time code sets the new password; it then works exactly once.
    assert_eq!(
        redeem(&f, &m.member_username, &code, NEW_PASSWORD).await.0,
        StatusCode::NO_CONTENT
    );
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;
    let must_change: bool =
        sqlx::query_scalar("SELECT must_change_password FROM accounts WHERE id = $1")
            .bind(m.member_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!must_change, "storing a password clears the flag");
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        redeem(&f, &m.member_username, &code, NEW_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "a used code is refused"
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED,
        "nothing resurrects a revoked token"
    );

    // The administrator's own session is untouched.
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &m.admin_token).await.0,
        StatusCode::OK
    );

    // An unknown account is a 404 and changes nothing.
    assert_eq!(
        request_reset(&f, &m.admin_token, Uuid::new_v4()).await.0,
        StatusCode::NOT_FOUND
    );

    let kinds = audit_kinds(&f.pool).await;
    assert!(
        kinds.iter().any(|k| k == "account.password_reset"),
        "{kinds:?}"
    );
    let payloads: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind = 'account.password_reset'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["access_tokens_revoked"], 1);
    assert_eq!(payloads[0]["ssh_keys_revoked"], 0);
    assert!(!payloads[0].to_string().contains(&code));
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn password_reset_codes_are_wrong_refused_and_expire() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let code = reset_code(&body);
    // A wrong code is refused and changes nothing.
    assert_eq!(
        redeem(&f, &m.member_username, &"0".repeat(64), NEW_PASSWORD)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );

    // A second reset supersedes the first unredeemed code.
    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let second = reset_code(&body);
    assert_eq!(
        redeem(&f, &m.member_username, &code, NEW_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "a superseded code is refused"
    );
    assert_eq!(
        redeem(&f, &m.member_username, &second, NEW_PASSWORD)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;

    // An expired code is refused even though it was never used.
    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let third = reset_code(&body);
    sqlx::query(
        "UPDATE password_reset_tokens SET expires_at = clock_timestamp() - interval '1 second' \
         WHERE used_at IS NULL",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        redeem(&f, &m.member_username, &third, "anotherpassword123")
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "an expired code is refused"
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_wrong_reset_code_does_not_take_an_authentication_slot() {
    // One slot only: the old code would wait the full 2 s queue and answer 429
    // after doing Argon2 work, so a 401 well inside that window proves the
    // wrong-code path never hashes.
    let f = Fixture::with_config(|c| c.auth_concurrency = 1).await;
    let m = members(&f).await;
    let permit = f.state.auth_slots.clone().acquire_owned().await.unwrap();
    let started = std::time::Instant::now();
    let (status, body) = redeem(&f, &m.member_username, &"0".repeat(64), NEW_PASSWORD).await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "a wrong code must not wait for the auth slot: {elapsed:?}"
    );
    assert_eq!(f.state.auth_slots.available_permits(), 0);
    drop(permit);
    assert_eq!(f.state.auth_slots.available_permits(), 1);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn one_reset_code_succeeds_once_under_concurrent_redemption() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let code = reset_code(&body);
    let first = redeem(&f, &m.member_username, &code, NEW_PASSWORD);
    let second = redeem(&f, &m.member_username, &code, "anotherpassword123");
    let (one, two) = tokio::join!(first, second);
    let statuses = [one.0, two.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::NO_CONTENT)
            .count(),
        1,
        "exactly one redemption wins: {statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::UNAUTHORIZED)
            .count(),
        1,
        "the other is refused: {statuses:?}"
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn only_usable_administrators_count_for_the_last_administrator_guard() {
    let f = Fixture::new().await;
    let usable = insert_account(&f.pool, &tag("usable-admin"), ADMIN_PASSWORD, true).await;
    let pending = insert_account(&f.pool, &tag("pending-admin"), ADMIN_PASSWORD, true).await;
    sqlx::query("UPDATE accounts SET must_change_password = true WHERE id = $1")
        .bind(pending)
        .execute(&f.pool)
        .await
        .unwrap();

    // A reset-pending administrator cannot sign in, so it must not satisfy the
    // rule that protects the last *usable* administrator.
    let disabled = githearth_server::account_admin::disable_inner(&f.state, pending, usable)
        .await
        .unwrap_err();
    assert_eq!(disabled.status_code(), StatusCode::CONFLICT);
    assert_eq!(
        disabled.message(),
        "the last enabled administrator cannot be disabled"
    );
    let demoted = githearth_server::account_admin::demote_inner(&f.state, pending, usable)
        .await
        .unwrap_err();
    assert_eq!(demoted.status_code(), StatusCode::CONFLICT);
    assert_eq!(
        demoted.message(),
        "the last enabled administrator cannot be demoted"
    );

    // Self-demote is refused even when a reset-pending administrator exists.
    let own = githearth_server::account_admin::demote_inner(&f.state, usable, usable)
        .await
        .unwrap_err();
    assert_eq!(own.status_code(), StatusCode::CONFLICT);
    assert_eq!(
        own.message(),
        "an administrator cannot demote their own account"
    );

    // The usable administrator still is one, and may remove the pending one.
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT is_admin AND NOT must_change_password FROM accounts WHERE id = $1"
        )
        .bind(usable)
        .fetch_one(&f.pool)
        .await
        .unwrap()
    );
    githearth_server::account_admin::demote_inner(&f.state, usable, pending)
        .await
        .expect("a usable administrator may demote a reset-pending one");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disabling_an_account_retires_its_live_reset_code() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let code = reset_code(&body);
    for action in ["disable", "enable"] {
        let (status, _, body) = send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{}/{action}", m.member_id),
                Some(&m.admin_token),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{action}: {body}");
    }
    // Re-enabling does not revive a code issued before the disable.
    assert_eq!(
        redeem(&f, &m.member_username, &code, NEW_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "a reset code must not survive a disable"
    );
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind = 'account.disabled' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(payload["reset_codes_retired"], 1);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_administration_list_shows_a_reset_pending_account() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    request_reset(&f, &m.admin_token, m.member_id).await;
    let (status, body) = get_json(&f.app, "/api/v1/admin/accounts?limit=100", &m.admin_token).await;
    assert_eq!(status, StatusCode::OK);
    let accounts: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let member_id = m.member_id.to_string();
    let member = accounts
        .iter()
        .find(|a| a["id"] == member_id.as_str())
        .expect("the member is listed");
    assert_eq!(member["must_change_password"], true);

    let (status, _, page) = send(
        &f.app,
        Request::get("/admin/accounts")
            .header("cookie", format!("githearth_session={}", m.admin_token))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("reset pending"), "{page}");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_flagged_account_recovers_through_the_reset_page_without_an_api_client() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let (_, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    let code = reset_code(&body);

    // The sign-in page advertises the browser route.
    let (status, _, login) =
        send(&f.app, Request::get("/login").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(login.contains("Have a reset code?"), "{login}");
    assert!(login.contains("href=\"/reset\""), "{login}");

    // The page issues its own CSRF cookie, exactly like the sign-in form.
    let (status, headers, page) =
        send(&f.app, Request::get("/reset").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Reset your password"), "{page}");
    let csrf = csrf_from(&page);
    let cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    // A wrong code is the fixed 401 text, not a busy page.
    let (status, _, page) = post_form(
        &f.app,
        "/reset",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("username", &m.member_username),
            ("code", &"0".repeat(64)),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{page}");
    assert!(page.contains("Invalid or expired reset code."), "{page}");
    assert!(!page.contains("authentication busy"), "{page}");

    // The real code sets the password and returns the owner to sign-in.
    let (status, headers, body) = post_form(
        &f.app,
        "/reset",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("username", &m.member_username),
            ("code", &code),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/login?reset=1"
    );
    let (status, _, login) = send(
        &f.app,
        Request::get("/login?reset=1").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(login.contains("Password changed."), "{login}");
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_administrator_reset_never_accepts_a_chosen_password() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let chosen = "administratorchosen1";

    // The reset endpoint takes no password: an attempt to supply one is a
    // rejected request, not a reset that installs it.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{}/password-reset", m.member_id),
            Some(&m.admin_token),
            Some(serde_json::json!({"new_password": chosen})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        login(&f.app, &m.member_username, chosen).await.0,
        StatusCode::UNAUTHORIZED,
        "an administrator-chosen password must never become usable"
    );
    // The account is untouched because the request was refused.
    login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    f.finish().await;
}

/// A minimal HTTP client for the Git smart-HTTP endpoint: returns the status
/// code and the whole response, including the body of a refusal.
async fn basic_git_status(port: u16, user: &str, password: &str) -> (u16, String) {
    use base64::Engine;
    let credential = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let request = format!(
        "GET /nope/nope.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\
Host: 127.0.0.1\r\n\
Authorization: Basic {credential}\r\n\
Connection: close\r\n\r\n"
    );
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to loopback");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no HTTP status in response: {text}"));
    (status, text)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn git_http_basic_authentication_is_throttled_like_a_sign_in() {
    let f = Fixture::with_config(|c| {
        c.login_throttle_max_per_username = 2;
        c.login_throttle_max_per_client = 1000;
    })
    .await;
    let m = members(&f).await;
    let port = serve(&f.app).await;

    for _ in 0..2 {
        let (status, body) = basic_git_status(port, &m.member_username, "wrong password").await;
        assert_eq!(status, 401, "{body}");
    }
    // The bound is shared with the API and the browser form: even the right
    // password is refused inside the window, and the body is the same one.
    let (status, body) = basic_git_status(port, &m.member_username, MEMBER_PASSWORD).await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("too many sign-in attempts"), "{body}");

    let ghost = tag("ghost");
    for _ in 0..2 {
        let (status, body) = basic_git_status(port, &ghost, "wrong password").await;
        assert_eq!(status, 401, "{body}");
    }
    let (status, body) = basic_git_status(port, &ghost, "wrong password").await;
    assert_eq!(status, 429, "{body}");
    assert!(body.contains("too many sign-in attempts"), "{body}");
    assert!(!body.contains(&ghost), "the refusal reveals no username");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_reset_is_refused_by_git_over_http_and_ssh_until_the_code_is_redeemed() {
    if Command::new("ssh").arg("-V").output().is_err() {
        panic!("ssh binary not found: the SSH admission path is not proven");
    }
    let f = Fixture::from_config(|c, root| {
        c.ssh_enabled = true;
        c.ssh_bind = "127.0.0.1:0".parse().unwrap();
        c.hook_program = PathBuf::from(env!("CARGO_BIN_EXE_githearth-server"));
        c.ssh_host_key_path = root.join("ssh_host_key");
        c.git_timeout_seconds = 120;
    })
    .await;
    githearth_server::ssh::generate_host_key(&f.config.ssh_host_key_path).unwrap();

    let m = members(&f).await;
    let (org, repo, _repo_id) = repository_for(&f, m.member_id).await;

    // A personal access token and a registered SSH key, both live.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "reset target", "scopes": ["repo:write"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pat = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let key_dir = std::env::temp_dir().join(format!("githearth-acct-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .expect("ssh-keygen");
    assert!(output.status.success());
    let public_key = std::fs::read_to_string(client_key.with_extension("pub")).unwrap();
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "reset key", "public_key": public_key})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let port = serve(&f.app).await;
    let http_url = format!("http://x:{pat}@127.0.0.1:{port}/{org}/{repo}.git");
    let first = std::env::temp_dir().join(format!("githearth-acct-pat-{}", Uuid::new_v4()));
    git_ok(git_cmd(&[
        "clone",
        "--",
        &http_url,
        first.to_str().unwrap(),
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&first);

    let (_handle, port_rx) = githearth_server::ssh::spawn(
        f.state.clone(),
        githearth_server::ssh_keys::authenticator(f.pool.clone()),
    );
    let ssh_port = port_rx.await.unwrap().port();
    let ssh_command = format!(
        "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        client_key.display()
    );
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{org}/{repo}.git");
    let ssh_first = std::env::temp_dir().join(format!("githearth-acct-ssh-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&["clone", &ssh_url, ssh_first.to_str().unwrap()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_command);
    git_ok(cmd).await;
    let _ = std::fs::remove_dir_all(&ssh_first);

    // The reset hands the administrator a code, revokes every credential and
    // leaves the account denied everywhere until it is redeemed.
    let (status, body) = request_reset(&f, &m.admin_token, m.member_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = reset_code(&body);

    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED,
        "the access token is revoked by a reset"
    );
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "the password is disabled while a change is required"
    );

    let refused = std::env::temp_dir().join(format!("githearth-acct-pat-{}", Uuid::new_v4()));
    let out = git_run(git_cmd(&[
        "clone",
        "--",
        &http_url,
        refused.to_str().unwrap(),
    ]))
    .await;
    assert!(
        !out.status.success(),
        "a revoked access token must not clone over HTTP"
    );
    assert!(!refused.exists());

    let ssh_refused = std::env::temp_dir().join(format!("githearth-acct-ssh-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&["clone", &ssh_url, ssh_refused.to_str().unwrap()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_command);
    let out = git_run(cmd).await;
    assert!(
        !out.status.success(),
        "a revoked SSH key must not clone over SSH"
    );
    assert!(!ssh_refused.exists());

    // The code restores a usable password, and nothing else: the old token and
    // key stay revoked, which is what a password reset promises.
    assert_eq!(
        redeem(&f, &m.member_username, &code, NEW_PASSWORD).await.0,
        StatusCode::NO_CONTENT
    );
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED
    );
    let new_url = format!(
        "http://{}:{NEW_PASSWORD}@127.0.0.1:{port}/{org}/{repo}.git",
        m.member_username
    );
    let fresh = std::env::temp_dir().join(format!("githearth-acct-pat-{}", Uuid::new_v4()));
    git_ok(git_cmd(&["clone", "--", &new_url, fresh.to_str().unwrap()])).await;
    let _ = std::fs::remove_dir_all(&fresh);

    let _ = std::fs::remove_dir_all(&key_dir);
    let _ = std::fs::remove_dir_all(&refused);
    let _ = std::fs::remove_dir_all(&ssh_refused);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sensitive_actions_require_reauthentication_and_the_prompt_reruns_them() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    let age_admin_session = || {
        sqlx::query(
            "UPDATE sessions SET authenticated_at = clock_timestamp() - interval '16 minutes' \
             WHERE account_id = $1",
        )
        .bind(m.admin_id)
        .execute(&f.pool)
    };

    age_admin_session().await.unwrap();
    let username = tag("after-reauth");
    let (status, _, body) = send(
        &f.app,
        create_request(&m.admin_token, &username, MEMBER_PASSWORD),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(denial["error"], "reauthentication required");
    assert_eq!(denial["reason"], "sensitive");

    // A wrong password neither refreshes the window nor reveals what is wrong
    // beyond the fixed sentence.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/me/reauthenticate",
            Some(&m.admin_token),
            Some(serde_json::json!({"password": "wrong current password"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("wrong current password"));

    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/me/reauthenticate",
            Some(&m.admin_token),
            Some(serde_json::json!({"password": ADMIN_PASSWORD})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        send(
            &f.app,
            create_request(&m.admin_token, &username, MEMBER_PASSWORD)
        )
        .await
        .0,
        StatusCode::CREATED
    );

    // The settings page offers the same prompt and repeats the action after it.
    age_admin_session().await.unwrap();
    let cookie = format!("githearth_session={}", m.admin_token);
    let (status, _, page) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let csrf = csrf_from(&page);
    let page_user = tag("page-after-reauth");
    let (status, _, page) = post_form(
        &f.app,
        "/admin/accounts",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("username", &page_user),
            ("password", MEMBER_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("Confirm your password"), "{page}");

    let (status, headers, _) = post_form(
        &f.app,
        "/settings/reauthenticate",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("password", ADMIN_PASSWORD),
            ("return_to", "/admin/accounts"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/admin/accounts"
    );
    let (status, headers, _) = post_form(
        &f.app,
        "/admin/accounts",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("username", &page_user),
            ("password", MEMBER_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/admin/accounts?notice=created"
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_accounts_link_is_only_offered_to_administrators() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    for (token, offered) in [(&m.admin_token, true), (&m.member_token, false)] {
        let (status, _, page) = send(
            &f.app,
            Request::get("/settings")
                .header("cookie", format!("githearth_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            page.contains("/admin/accounts"),
            offered,
            "the accounts link is offered only to administrators: {page}"
        );
    }
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn administrators_can_be_promoted_and_demoted_but_not_the_last_one() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let action = |id: Uuid, verb: &str| {
        json_request(
            "POST",
            &format!("/api/v1/admin/accounts/{id}/{verb}"),
            Some(&m.admin_token),
            None,
        )
    };

    let (status, _, body) = send(&f.app, action(m.member_id, "promote")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["is_admin"],
        true
    );
    let (status, _, body) = send(&f.app, action(m.member_id, "demote")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["is_admin"],
        false
    );

    // Self-demote is refused exactly as the page already refuses it, so a
    // usable administrator cannot strand the site by removing their own role.
    let (status, _, body) = send(&f.app, action(m.admin_id, "demote")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("own account"), "{body}");

    // A disabled account cannot be promoted.
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{}/disable", m.member_id),
                Some(&m.admin_token),
                None,
            ),
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, _, body) = send(&f.app, action(m.member_id, "promote")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("disabled account"), "{body}");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reset_admin_issues_a_single_use_credential_and_revokes_every_credential() {
    let f = Fixture::new().await;
    let username = tag("forgotten");
    let id = insert_account(&f.pool, &username, MEMBER_PASSWORD, false).await;
    let old_session = login_ok(&f.app, &username, MEMBER_PASSWORD).await;
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&old_session),
            Some(serde_json::json!({"name": "before reset", "scopes": ["repo:read"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pat = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // A registered SSH key, so "revokes the old administrator sessions" also
    // covers the key class on the path the SSH server itself calls.
    let key_dir = std::env::temp_dir().join(format!("githearth-reset-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .expect("ssh-keygen");
    assert!(output.status.success());
    let public_key = std::fs::read_to_string(client_key.with_extension("pub")).unwrap();
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&old_session),
            Some(serde_json::json!({"name": "before reset", "public_key": public_key})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fingerprint: String =
        sqlx::query_scalar("SELECT fingerprint FROM ssh_keys WHERE account_id = $1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .unwrap();

    // The forgotten account is already throttled; break-glass clears the
    // failed-sign-in counter along with the other credentials.
    for _ in 0..2 {
        assert_eq!(
            login(&f.app, &username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&f.app, &username, MEMBER_PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let credential = githearth_server::reset_admin(&f.pool, &username)
        .await
        .unwrap();
    assert_eq!(credential.id, id);
    assert_eq!(credential.code.len(), 64, "a 256-bit code, like the API's");
    assert!(credential.code.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!credential.expires_at.is_empty());

    // No credential survives: old session, token and SSH key.
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &old_session).await.0,
        StatusCode::UNAUTHORIZED,
        "break-glass revokes the old session"
    );
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::UNAUTHORIZED,
        "break-glass revokes access tokens"
    );
    let mut conn = f.pool.acquire().await.unwrap();
    assert!(
        githearth_server::ssh_keys::authenticate(&mut conn, &fingerprint)
            .await
            .unwrap()
            .is_none(),
        "break-glass revokes SSH keys"
    );
    drop(conn);

    // Administration is restored, and the account must set its own password
    // before anything works.
    let (is_admin, suspended, must_change): (bool, bool, bool) = sqlx::query_as(
        "SELECT is_admin, suspended, must_change_password FROM accounts WHERE username = $1",
    )
    .bind(&username)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(is_admin, "reset-admin restores administration");
    assert!(!suspended);
    assert!(must_change, "the old password cannot be used at all");
    assert_eq!(
        login(&f.app, &username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );

    // The credential works exactly once and stores the account's own password.
    assert_eq!(
        redeem(&f, &username, &credential.code, NEW_PASSWORD)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        redeem(&f, &username, &credential.code, NEW_PASSWORD)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "a used credential is refused"
    );
    let token = login_ok(&f.app, &username, NEW_PASSWORD).await;
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &token).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &f.app,
            create_request(&token, &tag("restored-admin-created"), MEMBER_PASSWORD)
        )
        .await
        .0,
        StatusCode::CREATED,
        "the restored administrator can administer"
    );

    // An expired credential is refused and leaves the account locked until a
    // fresh one is issued, which is the same rule the API's codes follow.
    let expired = githearth_server::reset_admin(&f.pool, &username)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE password_reset_tokens SET expires_at = clock_timestamp() - interval '1 minute' \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        redeem(&f, &username, &expired.code, NEW_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "an expired credential is refused"
    );
    assert_eq!(
        login(&f.app, &username, NEW_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "the account stays in must_change_password until it redeems a live code"
    );

    // The action is audited on the site chain, and the code never appears in
    // the audit or outbox record.
    let resets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE kind = 'administrator.reset' AND actor_id = $1 AND organization_id IS NULL",
    )
    .bind(id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(resets, 2, "one row per break-glass run");
    let payloads: Vec<String> = sqlx::query_scalar(
        "SELECT payload::text FROM outbox_events WHERE kind = 'administrator.reset'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert!(!payloads.is_empty());
    for payload in &payloads {
        assert!(!payload.contains(&credential.code));
        assert!(!payload.contains(&expired.code));
        assert!(!payload.contains(NEW_PASSWORD));
    }
    assert!(
        githearth_server::reset_admin(&f.pool, &tag("nobody"))
            .await
            .is_err(),
        "an unknown account cannot be reset"
    );
    let _ = std::fs::remove_dir_all(&key_dir);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_wrong_current_password_is_bounded_like_a_failed_login() {
    let f = Fixture::with_config(|c| c.login_throttle_max_per_username = 2).await;
    let m = members(&f).await;
    let change = |current: &str| {
        json_request(
            "POST",
            "/api/v1/user/password",
            Some(&m.member_token),
            Some(serde_json::json!({"current_password": current, "new_password": NEW_PASSWORD})),
        )
    };
    for _ in 0..2 {
        assert_eq!(
            send(&f.app, change("wrong current password")).await.0,
            StatusCode::FORBIDDEN
        );
    }
    // Beyond the bound the endpoint refuses before hashing, even for the right
    // current password.
    assert_eq!(
        send(&f.app, change(MEMBER_PASSWORD)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // The window passing restores the endpoint without intervention.
    age_login_attempts(&f.pool, 901).await;
    assert_eq!(
        send(&f.app, change(MEMBER_PASSWORD)).await.0,
        StatusCode::NO_CONTENT
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U03: failed sign-in throttling
// ---------------------------------------------------------------------------

/// Moves every recorded attempt `seconds` into the past, which is how a test
/// advances past a window without sleeping.
async fn age_login_attempts(pool: &PgPool, seconds: i64) {
    sqlx::query("UPDATE login_attempts SET attempted_at = attempted_at - $1 * interval '1 second'")
        .bind(seconds)
        .execute(pool)
        .await
        .unwrap();
}

/// A fixture whose throttle bounds are the running configuration's defaults,
/// not the suite's small values, so U18 asserts what an operator actually gets.
/// Only the delay steps are shortened, to keep the test's wall time small; the
/// delay's own shape is asserted separately.
async fn defaults_fixture() -> Fixture {
    let defaults = Config::default();
    Fixture::from_config(|c, _| {
        c.login_throttle_max_per_username = defaults.login_throttle_max_per_username;
        c.login_throttle_max_per_client = defaults.login_throttle_max_per_client;
        c.login_throttle_window_seconds = defaults.login_throttle_window_seconds;
        c.login_throttle_delay_base_ms = 5;
        c.login_throttle_delay_max_ms = 20;
    })
    .await
}

/// U18: 5 failures per account and 20 per address inside 900 s, taken from the
/// running configuration rather than from a literal in the test.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_plan_throttle_defaults_are_the_running_configuration() {
    let f = defaults_fixture().await;
    let username = tag("defaults");
    insert_account(&f.pool, &username, MEMBER_PASSWORD, false).await;

    // The fifth failure is accepted; the sixth attempt is refused — even with
    // the correct password — before it is hashed.
    for attempt in 1..=5 {
        assert_eq!(
            login(&f.app, &username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} is inside the five-failure bound"
        );
    }
    assert_eq!(
        login(&f.app, &username, MEMBER_PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the account bound is 5, not 10"
    );

    // The window is 900 s: the refusal survives most of it and clears once the
    // recorded failures have left it (aged in two steps to stay clear of the
    // boundary).
    age_login_attempts(&f.pool, 880).await;
    assert_eq!(
        login(&f.app, &username, MEMBER_PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the window has not passed yet"
    );
    age_login_attempts(&f.pool, 30).await;
    login_ok(&f.app, &username, MEMBER_PASSWORD).await;

    // The address bound is 20, not 50: twenty failures from one address for
    // twenty distinct usernames are accepted, the twenty-first is refused.
    let attacker = "203.0.113.20";
    for probe in 1..=20 {
        assert_eq!(
            login_from(
                &f.app,
                &tag(&format!("probe{probe}")),
                "wrong password",
                Some(attacker)
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED,
            "address failure {probe} is inside the bound"
        );
    }
    assert_eq!(
        login_from(&f.app, &tag("probe21"), "wrong password", Some(attacker))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS,
        "the address bound is 20, not 50"
    );
    // Another caller without that address is unaffected.
    let other = tag("other");
    insert_account(&f.pool, &other, MEMBER_PASSWORD, false).await;
    login_ok(&f.app, &other, MEMBER_PASSWORD).await;
    f.finish().await;
}

/// The delay is a function of the failure count, so it starts after the fifth
/// failure and doubles per further failure. The account is left inside its
/// bound (10 here) so the delayed attempts are accepted, which is the case the
/// evidence found missing.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_fifth_failure_starts_the_exponential_delay() {
    let f = Fixture::with_config(|c| {
        c.login_throttle_max_per_username = 10;
        c.login_throttle_max_per_client = 1000;
        c.login_throttle_delay_base_ms = 500;
        c.login_throttle_delay_max_ms = 2_000;
    })
    .await;
    let m = members(&f).await;

    // The first four failures are not delayed; their fastest time is the
    // baseline. (Asserting against the fastest, not one sample, keeps a busy
    // test machine from masquerading as a delay.)
    let mut baseline = Duration::MAX;
    for _ in 0..4 {
        let started = Instant::now();
        assert_eq!(
            login(&f.app, &m.member_username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
        baseline = baseline.min(started.elapsed());
    }
    // The fifth failure is recorded without a wait.
    assert_eq!(
        login(&f.app, &m.member_username, "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );

    let started = Instant::now();
    assert_eq!(
        login(&f.app, &m.member_username, "wrong password").await.0,
        StatusCode::UNAUTHORIZED,
        "the sixth attempt is still accepted below the ten-failure bound"
    );
    let sixth = started.elapsed();
    let started = Instant::now();
    assert_eq!(
        login(&f.app, &m.member_username, "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    let seventh = started.elapsed();

    assert!(
        sixth >= Duration::from_millis(450) && sixth > baseline,
        "the sixth attempt waits after the fifth failure: fastest baseline \
         {baseline:?}, sixth {sixth:?}"
    );
    assert!(
        seventh >= sixth + Duration::from_millis(350),
        "the delay doubles per failure: sixth {sixth:?}, seventh {seventh:?}"
    );
    f.finish().await;
}

/// U18: token and SSH-key authentication are exempt from the per-account
/// ceiling, so an attacker cannot lock the owner out through it.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_throttled_account_still_uses_its_token_and_ssh_key() {
    let f = Fixture::with_config(|c| c.login_throttle_max_per_username = 2).await;
    let m = members(&f).await;

    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "throttle exempt", "scopes": ["repo:read"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pat = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let key_dir = std::env::temp_dir().join(format!("githearth-throttle-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .expect("ssh-keygen");
    assert!(output.status.success());
    let public_key = std::fs::read_to_string(client_key.with_extension("pub")).unwrap();
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "throttle exempt", "public_key": public_key})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let fingerprint: String =
        sqlx::query_scalar("SELECT fingerprint FROM ssh_keys WHERE account_id = $1")
            .bind(m.member_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();

    // The password path is refused, even for the right password.
    for _ in 0..2 {
        assert_eq!(
            login(&f.app, &m.member_username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    // The token and the key are not charged to the password bound.
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &pat).await.0,
        StatusCode::OK,
        "an access token is exempt from the per-account ceiling"
    );
    let mut conn = f.pool.acquire().await.unwrap();
    assert!(
        githearth_server::ssh_keys::authenticate(&mut conn, &fingerprint)
            .await
            .unwrap()
            .is_some(),
        "an SSH key is exempt from the per-account ceiling"
    );
    drop(conn);
    let _ = std::fs::remove_dir_all(&key_dir);
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_sign_ins_are_bounded_per_username_and_never_lock_out() {
    let f = Fixture::with_config(|c| {
        c.login_throttle_max_per_username = 2;
        c.login_throttle_max_per_client = 1000;
    })
    .await;
    let m = members(&f).await;

    for _ in 0..2 {
        assert_eq!(
            login(&f.app, &m.member_username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    // Even the right password is refused inside the window, with a generic
    // message that reveals nothing about the account.
    let (status, body) = login(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!body.contains(&m.member_username));
    assert!(!body.contains("password"));

    // The throttle state holds no username spelling and no password.
    for subject in sqlx::query_scalar::<_, String>("SELECT subject FROM login_attempts")
        .fetch_all(&f.pool)
        .await
        .unwrap()
    {
        assert!(!subject.contains(&m.member_username));
        assert!(!subject.contains("wrong"));
    }

    // No permanent lock: once the window passes, the account signs in again.
    age_login_attempts(&f.pool, 901).await;
    login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;

    // A successful sign-in clears the username's counter, so an administrator
    // who mistypes once is not progressively locked out.
    assert_eq!(
        login(&f.app, &m.member_username, "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    for _ in 0..2 {
        assert_eq!(
            login(&f.app, &m.member_username, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    // An unknown username is bounded identically and reveals nothing.
    let ghost = tag("ghost");
    for _ in 0..2 {
        assert_eq!(
            login(&f.app, &ghost, "wrong password").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&f.app, &ghost, "wrong password").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_sign_ins_are_bounded_per_client_address() {
    let f = Fixture::with_config(|c| {
        c.login_throttle_max_per_username = 1000;
        c.login_throttle_max_per_client = 2;
    })
    .await;
    let m = members(&f).await;

    // Two failed guesses from one address for two different usernames use up
    // that address's bound, whatever username is tried next.
    let attacker = "203.0.113.7";
    for username in [tag("first"), tag("second")] {
        assert_eq!(
            login_from(&f.app, &username, "wrong password", Some(attacker))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login_from(&f.app, &m.admin_username, ADMIN_PASSWORD, Some(attacker))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS,
        "the address bound refuses even the right password"
    );
    // Another address is unaffected, so one noisy client cannot lock out
    // everyone.
    login_ok(&f.app, &m.admin_username, ADMIN_PASSWORD).await;

    // Ageing the address's rows restores it.
    age_login_attempts(&f.pool, 901).await;
    assert_eq!(
        login_from(&f.app, &m.admin_username, ADMIN_PASSWORD, Some(attacker))
            .await
            .0,
        StatusCode::OK
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn browser_sign_in_reports_throttling_without_revealing_the_account() {
    let f = Fixture::with_config(|c| c.login_throttle_max_per_username = 1).await;
    let m = members(&f).await;

    async fn browser_login(app: &Router, username: &str, password: &str) -> (StatusCode, String) {
        let token = "0123456789abcdef0123456789abcdef";
        let (status, _, body) = post_form(
            app,
            "/login",
            Some(&format!("githearth_csrf={token}")),
            &[
                ("username", username),
                ("password", password),
                ("csrf_token", token),
            ],
        )
        .await;
        (status, body)
    }

    // The first failed attempt is an ordinary bad-credential redirect.
    assert_eq!(
        browser_login(&f.app, &m.member_username, "wrong password")
            .await
            .0,
        StatusCode::SEE_OTHER
    );
    // The second is refused with the throttle page, for a real and an unknown
    // username alike.
    let (status, body) = browser_login(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(body.contains("Too many sign-in attempts"), "{body}");
    assert!(!body.contains(&m.member_username));
    // An unknown username reaches the same page after its own attempt, so the
    // throttle reveals nothing about which accounts exist.
    let ghost = tag("ghost");
    assert_eq!(
        browser_login(&f.app, &ghost, "wrong password").await.0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser_login(&f.app, &ghost, "wrong password").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U01: a disabled account is refused on every admission path
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_disabled_account_is_refused_on_sessions_access_tokens_and_the_browser() {
    let f = Fixture::new().await;
    let m = members(&f).await;

    // An access token and a browser cookie for the member.
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "member token", "scopes": ["repo:write"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let access_token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookie = format!("githearth_session={}", m.member_token);

    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{}/disable", m.member_id),
                Some(&m.admin_token),
                None,
            ),
        )
        .await
        .0,
        StatusCode::OK
    );

    // Bearer session.
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &m.member_token).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Access token.
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &access_token).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Browser UI: a page redirects to sign-in and renders no signed-in content.
    let (status, headers, _) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/login"
    );
    // Login.
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U01: the server-rendered pages
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_settings_page_changes_the_password_with_csrf_and_keeps_the_session() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let cookie = format!("githearth_session={}", m.member_token);
    let other = login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;

    let (status, _, page) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Change password"), "{page}");
    assert!(page.contains("/settings/password"), "{page}");
    // The form never carries a password value.
    assert!(!page.contains(MEMBER_PASSWORD));
    let csrf = csrf_from(&page);

    // Without the CSRF token nothing happens.
    let (status, _, _) = post_form(
        &f.app,
        "/settings/password",
        Some(&cookie),
        &[
            ("csrf_token", "00000000000000000000000000000000"),
            ("current_password", MEMBER_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        login(&f.app, &m.member_username, MEMBER_PASSWORD).await.0,
        StatusCode::OK
    );

    // A wrong current password re-renders the page with a notice and no values.
    let (status, _, page) = post_form(
        &f.app,
        "/settings/password",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("current_password", "wrong current password"),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("current password is incorrect"), "{page}");
    assert!(!page.contains(NEW_PASSWORD));
    assert!(!page.contains("wrong current password"));

    // The right current password redirects and keeps this session only.
    let (status, headers, _) = post_form(
        &f.app,
        "/settings/password",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("current_password", MEMBER_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/settings?password=changed"
    );
    login_ok(&f.app, &m.member_username, NEW_PASSWORD).await;
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &other).await.0,
        StatusCode::UNAUTHORIZED
    );
    // The settings page still renders for the kept session, with a notice.
    let (status, _, page) = send(
        &f.app,
        Request::get("/settings?password=changed")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Password changed."), "{page}");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_administration_page_lists_creates_disables_and_enables_accounts() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let cookie = format!("githearth_session={}", m.admin_token);

    let (status, _, page) = send(
        &f.app,
        Request::get("/admin/accounts")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Create account"), "{page}");
    assert!(page.contains(&m.member_username), "{page}");
    assert!(page.contains(&escaped(&m.member_username)), "{page}");
    // The administrator's own row offers no disable button.
    assert!(page.contains("you"), "{page}");
    let csrf = csrf_from(&page);

    // Create through the form, with a username that must be escaped.
    let username = format!("new-{}", Uuid::new_v4().simple());
    let (status, headers, body) = post_form(
        &f.app,
        "/admin/accounts",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("username", &username),
            ("password", MEMBER_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/admin/accounts?notice=created"
    );
    let created_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&username)
        .fetch_one(&f.pool)
        .await
        .unwrap();

    // A CSRF failure changes nothing and is a 403 page.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{created_id}/disable"),
        Some(&cookie),
        &[("csrf_token", "00000000000000000000000000000000")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Disable, then the page offers "Enable" for that account.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{created_id}/disable"),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, _, page) = send(
        &f.app,
        Request::get("/admin/accounts")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        page.contains(&format!("/admin/accounts/{created_id}/enable")),
        "{page}"
    );

    // Re-enable through the form.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{created_id}/enable"),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT NOT suspended FROM accounts WHERE id = $1")
            .bind(created_id)
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );

    // Reset the password through the form; it shows a one-time code, never a
    // password, and revokes the account's sessions.
    let target_token = login_ok(&f.app, &username, MEMBER_PASSWORD).await;
    let (status, _, body) = post_form(
        &f.app,
        &format!("/admin/accounts/{created_id}/password-reset"),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("one-time code"), "{body}");
    assert!(!body.contains(MEMBER_PASSWORD));
    assert!(!body.contains(NEW_PASSWORD));
    let code = reset_code_from_page(&body);
    assert_eq!(
        get_json(&f.app, "/api/v1/me", &target_token).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        redeem(&f, &username, &code, NEW_PASSWORD).await.0,
        StatusCode::NO_CONTENT
    );
    login_ok(&f.app, &username, NEW_PASSWORD).await;

    // Disabling the administrator's own account through the form is refused.
    let (status, _, body) = post_form(
        &f.app,
        &format!("/admin/accounts/{}/disable", m.admin_id),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("cannot disable your own account"), "{body}");

    // An unknown account is a 404 page for both actions.
    let unknown = Uuid::new_v4();
    for action in ["disable", "enable"] {
        let (status, _, page) = post_form(
            &f.app,
            &format!("/admin/accounts/{unknown}/{action}"),
            Some(&cookie),
            &[("csrf_token", &csrf)],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
        assert!(page.contains("Not found"), "{page}");
    }
    let (status, _, page) = post_form(
        &f.app,
        &format!("/admin/accounts/{unknown}/password-reset"),
        Some(&cookie),
        &[("csrf_token", &csrf), ("new_password", NEW_PASSWORD)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_administration_page_reports_each_action_and_repeats_nothing() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let cookie = format!("githearth_session={}", m.admin_token);
    let (_, _, page) = send(
        &f.app,
        Request::get("/admin/accounts")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let csrf = csrf_from(&page);

    for (notice, expected) in [
        ("created", "Account created."),
        ("disabled", "Account disabled."),
        ("enabled", "Account enabled."),
        ("reset", "Password reset."),
    ] {
        let (status, _, page) = send(
            &f.app,
            Request::get(format!("/admin/accounts?notice={notice}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(expected), "{notice}: {page}");
    }
    // An unknown notice renders no message rather than echoing the parameter.
    let (status, _, page) = send(
        &f.app,
        Request::get("/admin/accounts?notice=%3Cscript%3Ealert(1)%3C/script%3E")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("script"), "{page}");

    // Disabling an already disabled account is the requested state and records
    // nothing new.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{}/disable", m.member_id),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{}/disable", m.member_id),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE kind = 'account.disabled'"
        )
        .fetch_one(&f.pool)
        .await
        .unwrap(),
        1
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_throttled_settings_password_form_explains_and_changes_nothing() {
    let f = Fixture::with_config(|c| c.login_throttle_max_per_username = 1).await;
    let m = members(&f).await;
    let cookie = format!("githearth_session={}", m.member_token);
    let (_, _, page) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let csrf = csrf_from(&page);
    // One wrong attempt uses the bound; the next submit is refused with the
    // throttle notice and the password is unchanged.
    let (status, _, _) = post_form(
        &f.app,
        "/settings/password",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("current_password", "wrong current password"),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, page) = post_form(
        &f.app,
        "/settings/password",
        Some(&cookie),
        &[
            ("csrf_token", &csrf),
            ("current_password", MEMBER_PASSWORD),
            ("new_password", NEW_PASSWORD),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(page.contains("Too many attempts"), "{page}");
    assert!(!page.contains(NEW_PASSWORD));
    age_login_attempts(&f.pool, 901).await;
    login_ok(&f.app, &m.member_username, MEMBER_PASSWORD).await;
    f.finish().await;
}

fn escaped(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_non_administrator_gets_the_refusal_page_and_no_administration_content() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let cookie = format!("githearth_session={}", m.member_token);

    let (status, _, page) = send(
        &f.app,
        Request::get("/admin/accounts")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("Not permitted"), "{page}");
    assert!(!page.contains(&m.admin_username), "{page}");

    // A form post without a valid CSRF token is a CSRF refusal, and with one it
    // is still the administrator refusal.
    let (status, _, _) = post_form(
        &f.app,
        &format!("/admin/accounts/{}/disable", m.admin_id),
        Some(&cookie),
        &[("csrf_token", "00000000000000000000000000000000")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, _, settings) = send(
        &f.app,
        Request::get("/settings")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let csrf = csrf_from(&settings);
    let (status, _, page) = post_form(
        &f.app,
        &format!("/admin/accounts/{}/disable", m.admin_id),
        Some(&cookie),
        &[("csrf_token", &csrf)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("Not permitted"), "{page}");

    // A signed-out visitor is redirected to sign in.
    let (status, _, _) = send(
        &f.app,
        Request::get("/admin/accounts").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U01: Git over HTTP and Git over SSH
// ---------------------------------------------------------------------------

fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "http.lowSpeedLimit=1", "-c", "http.lowSpeedTime=30"])
        .args(args);
    cmd
}

async fn git_run(mut cmd: Command) -> std::process::Output {
    match tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || cmd.output()),
    )
    .await
    {
        Ok(Ok(Ok(out))) => out,
        Ok(Ok(Err(e))) => panic!("git failed to launch: {e}"),
        Ok(Err(_)) => panic!("spawn_blocking join error"),
        Err(_) => panic!("git timed out"),
    }
}

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Creates an organization, a repository row and a bare repository with one
/// commit, owned by `owner`.
async fn repository_for(f: &Fixture, owner: Uuid) -> (String, String, Uuid) {
    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Account Admin Org')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(org_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind(&repo_name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(&f.pool)
        .await
        .unwrap();
    let repo_path = f
        .state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();
    let work = std::env::temp_dir().join(format!("githearth-acct-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    git_ok(git_cmd(&["init", "-b", "main", work.to_str().unwrap()])).await;
    std::fs::write(work.join("README.md"), "# hello\n").unwrap();
    git_ok(git_cmd(&["-C", work.to_str().unwrap(), "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]))
    .await;
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work.to_str().unwrap(),
        "main:refs/heads/main",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);
    (org_slug, repo_name, repo_id)
}

/// Serves the router on a loopback port and returns it.
async fn serve(app: &Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_disabled_account_is_refused_by_git_over_http() {
    let f = Fixture::new().await;
    let m = members(&f).await;
    let (org, repo, _repo_id) = repository_for(&f, m.member_id).await;
    let port = serve(&f.app).await;
    let clone_dir = std::env::temp_dir().join(format!("githearth-acct-clone-{}", Uuid::new_v4()));

    let url = format!(
        "http://{}:{}@127.0.0.1:{port}/{org}/{repo}.git",
        m.member_username, MEMBER_PASSWORD
    );
    git_ok(git_cmd(&["clone", "--", &url, clone_dir.to_str().unwrap()])).await;
    let _ = std::fs::remove_dir_all(&clone_dir);

    // Disable the account; a fresh clone must be refused.
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{}/disable", m.member_id),
                Some(&m.admin_token),
                None,
            ),
        )
        .await
        .0,
        StatusCode::OK
    );
    let refused_dir = std::env::temp_dir().join(format!("githearth-acct-clone-{}", Uuid::new_v4()));
    let out = git_run(git_cmd(&[
        "clone",
        "--",
        &url,
        refused_dir.to_str().unwrap(),
    ]))
    .await;
    assert!(
        !out.status.success(),
        "a disabled account must not clone over HTTP"
    );
    assert!(!refused_dir.exists());
    let _ = std::fs::remove_dir_all(&refused_dir);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_disabled_account_is_refused_by_git_over_ssh() {
    if Command::new("ssh").arg("-V").output().is_err() {
        panic!("ssh binary not found: the SSH admission path is not proven");
    }
    let f = Fixture::from_config(|c, root| {
        c.ssh_enabled = true;
        c.ssh_bind = "127.0.0.1:0".parse().unwrap();
        c.hook_program = PathBuf::from(env!("CARGO_BIN_EXE_githearth-server"));
        c.ssh_host_key_path = root.join("ssh_host_key");
        c.git_timeout_seconds = 120;
    })
    .await;
    githearth_server::ssh::generate_host_key(&f.config.ssh_host_key_path).unwrap();

    let m = members(&f).await;
    let (org, repo, _repo_id) = repository_for(&f, m.member_id).await;

    // Register a client key for the member through the settings API.
    let key_dir = std::env::temp_dir().join(format!("githearth-acct-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .expect("ssh-keygen");
    assert!(output.status.success());
    let public_key = std::fs::read_to_string(client_key.with_extension("pub")).unwrap();
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/ssh-keys",
            Some(&m.member_token),
            Some(serde_json::json!({"name": "account admin", "public_key": public_key})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (_handle, port_rx) = githearth_server::ssh::spawn(
        f.state.clone(),
        githearth_server::ssh_keys::authenticator(f.pool.clone()),
    );
    let ssh_port = port_rx.await.unwrap().port();
    let ssh_command = format!(
        "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        client_key.display()
    );
    let url = format!("ssh://git@127.0.0.1:{ssh_port}/{org}/{repo}.git");

    let first = std::env::temp_dir().join(format!("githearth-acct-ssh-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&["clone", &url, first.to_str().unwrap()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_command);
    git_ok(cmd).await;
    let _ = std::fs::remove_dir_all(&first);

    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &format!("/api/v1/admin/accounts/{}/disable", m.member_id),
                Some(&m.admin_token),
                None,
            ),
        )
        .await
        .0,
        StatusCode::OK
    );
    let refused = std::env::temp_dir().join(format!("githearth-acct-ssh-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&["clone", &url, refused.to_str().unwrap()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_command);
    let out = git_run(cmd).await;
    assert!(
        !out.status.success(),
        "a disabled account must not clone over SSH"
    );
    assert!(!refused.exists());

    let _ = std::fs::remove_dir_all(&key_dir);
    let _ = std::fs::remove_dir_all(&refused);
    f.finish().await;
}
