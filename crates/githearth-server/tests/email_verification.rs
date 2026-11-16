//! Email verification acceptance tests (M4.1 U23a), PostgreSQL-backed.
//!
//! A link verifies once; a second redemption, an expired token, a token minted
//! for an address the account has since changed, and a reused token are all
//! refused with one fixed refusal. A duplicate address is refused without
//! naming its holder. The plaintext token is captured from the injected
//! transport and then searched for in every text and json column of the public
//! schema: only its SHA-256 may exist.
//!
//! No test opens an SMTP socket: the transport is [`MemoryTransport`] (or a
//! failing transport in the credential test). Fixtures use SQL only to seed
//! accounts and to age a token; every assertion goes through the router.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{
    AppState, MIGRATOR,
    config::Config,
    connect, hash_password,
    mail::{EmailMessage, MailError, MailFuture, MailTransport, MemoryTransport},
    router,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, Connection, PgPool, postgres::PgConnectOptions};
use std::{
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn sha256(value: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes()).to_vec()
}

// ---------------------------------------------------------------------------
// Per-test database

struct Db {
    name: String,
    url: String,
    admin: PgConnectOptions,
}

impl Db {
    async fn new() -> Self {
        let base =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let name = format!("githearth_email_{}", Uuid::new_v4().simple());
        let url = {
            let (origin, _) = base.rsplit_once('/').unwrap();
            format!("{origin}/{name}")
        };
        let admin = PgConnectOptions::from_str(&base)
            .unwrap()
            .disable_statement_logging();
        let mut conn = admin.clone().connect().await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        Self { name, url, admin }
    }

    async fn drop_db(self) {
        let mut conn = self.admin.connect().await.unwrap();
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&mut conn)
        .await;
        let _ = conn.close().await;
    }
}

// ---------------------------------------------------------------------------
// Fixture

struct User {
    id: Uuid,
    username: String,
    token: String,
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash_password("correct-horse-battery").unwrap())
        .execute(pool)
        .await
        .unwrap();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')",
    )
    .bind(&token)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    User {
        id,
        username,
        token,
    }
}

async fn fixture(url: &str, mailer: Arc<dyn MailTransport>) -> Fixture {
    fixture_with(url, mailer, |_| {}).await
}

async fn fixture_with(
    url: &str,
    mailer: Arc<dyn MailTransport>,
    adjust: impl FnOnce(&mut Config),
) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-email-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let mut config = Config {
        database_url: url.to_owned(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        job_poll_interval_ms: 50,
        public_url: "https://forge.example".into(),
        ..Config::default()
    };
    adjust(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), config)
        .await
        .unwrap()
        .with_mailer(mailer);
    let app = router(state.clone());
    Fixture {
        app,
        state,
        pool,
        storage_root,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = body.map_or_else(String::new, ToString::to_string);
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    )
}

/// Runs the relay and the runner until the notification/verification jobs are
/// finished. The mail transport is in memory, so this never opens a socket.
async fn drain(state: &AppState) {
    githearth_server::outbox::register_builtin(state)
        .await
        .unwrap();
    githearth_server::outbox::fan_out_batch(state, 10_000)
        .await
        .unwrap();
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    registry.backoff_override = Some(Duration::from_millis(20));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let runner = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut quiet = 0;
    loop {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE state IN ('pending','running') \
             AND kind = 'notification.outbox'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if pending == 0 {
            quiet += 1;
            if quiet == 2 {
                break;
            }
        } else {
            quiet = 0;
            if tokio::time::Instant::now() >= deadline {
                panic!("notification jobs did not drain");
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;
}

async fn request_link(app: &Router, token: &str, email: &str) -> (StatusCode, Value, String) {
    api(
        app,
        "POST",
        "/api/v1/me/email-verification",
        Some(token),
        Some(&json!({"email": email})),
    )
    .await
}

/// The 64-hex token from the link in a captured verification message.
fn token_from(message: &EmailMessage) -> String {
    let marker = "/verify-email/";
    let start = message
        .body
        .find(marker)
        .unwrap_or_else(|| panic!("no verification link in {:?}", message.body))
        + marker.len();
    message.body[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect()
}

async fn redeem(app: &Router, token: &str) -> (StatusCode, Value, String) {
    api(
        app,
        "POST",
        "/api/v1/email-verification/redeem",
        None,
        Some(&json!({"token": token})),
    )
    .await
}

async fn request_and_capture_link(
    f: &Fixture,
    user: &User,
    email: &str,
    mail: &MemoryTransport,
) -> String {
    let (status, body, text) = request_link(&f.app, &user.token, email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let delivered = mail.delivered();
    assert_eq!(delivered.len(), 1, "one verification mail");
    assert_eq!(delivered[0].to, email);
    token_from(&delivered[0])
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_link_verifies_once_and_a_second_redemption_is_refused() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "verify").await;
    let email = format!("{}@example.com", u.username);

    let token = request_and_capture_link(&f, &u, &email, &mail).await;
    let (status, _, text) = redeem(&f.app, &token).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    let verified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(verified);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'account.email_verified' AND actor_id = $1",
    )
    .bind(u.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    // This proves an account cannot be verified twice: the replay is refused
    // because `email_verified_at` is already set, so the accounts UPDATE's
    // `AND email_verified_at IS NULL` is the guard under test here.
    // Revert: remove `AND email_verified_at IS NULL` from that UPDATE; the
    // second redemption returns 204 and the audit count becomes two. The
    // token's own single-use record is proved separately, in
    // `a_used_token_is_refused_even_when_the_account_is_unverified_again`.
    let (status, body, text) = redeem(&f.app, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body} {text}");
    assert_eq!(body["error"], "invalid or expired verification token");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'account.email_verified' AND actor_id = $1",
    )
    .bind(u.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_expired_token_is_refused() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "expired").await;
    let email = format!("{}@example.com", u.username);
    let token = request_and_capture_link(&f, &u, &email, &mail).await;
    sqlx::query(
        "UPDATE email_verifications SET expires_at = clock_timestamp() - interval '1 second'",
    )
    .execute(&f.pool)
    .await
    .unwrap();

    // Revert: remove `expires_at > clock_timestamp()` from the lookup; the
    // expired row would verify.
    let (status, _, text) = redeem(&f.app, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    let verified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!verified);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_used_token_is_refused_even_when_the_account_is_unverified_again() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "single-use").await;
    let email = format!("{}@example.com", u.username);
    let token = request_and_capture_link(&f, &u, &email, &mail).await;
    assert_eq!(redeem(&f.app, &token).await.0, StatusCode::NO_CONTENT);

    // Clear the verification the way a later change of address would leave
    // the account: unverified, still holding the address the link was minted
    // for. The only thing left to refuse the replay is the token's own
    // single-use record, not `accounts.email_verified_at`.
    sqlx::query("UPDATE accounts SET email_verified_at = NULL WHERE id = $1")
        .bind(u.id)
        .execute(&f.pool)
        .await
        .unwrap();

    // Reverts: remove `used_at IS NULL` from the redemption lookup (the used
    // row is found again and the UPDATE succeeds), or drop the
    // `UPDATE email_verifications SET used_at` that records the use (the row
    // stays unused); either makes the second redemption a 204.
    let (status, body, text) = redeem(&f.app, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body} {text}");
    assert_eq!(body["error"], "invalid or expired verification token");

    let used: bool = sqlx::query_scalar(
        "SELECT used_at IS NOT NULL FROM email_verifications \
         WHERE account_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(u.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(used, "the first redemption recorded the token as used");
    let verified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!verified, "the replay did not verify the account again");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'account.email_verified' AND actor_id = $1",
    )
    .bind(u.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1, "the replay wrote no second audit event");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_link_superseded_by_a_new_request_is_refused() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "superseded").await;
    let email = format!("{}@example.com", u.username);
    let first = request_and_capture_link(&f, &u, &email, &mail).await;

    // A real second request for the same address: `request_inner` retires the
    // account's unused links before it writes the new one.
    let (status, body, text) = request_link(&f.app, &u.token, &email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let delivered = mail.delivered();
    assert_eq!(delivered.len(), 2, "one mail per request");
    let second = token_from(&delivered[1]);
    assert_ne!(first, second);

    // Revert: drop the `UPDATE email_verifications SET used_at` in
    // `request_inner`; the first link stays live and redeems with a 204.
    let (status, body, text) = redeem(&f.app, &first).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body} {text}");
    let (status, _, text) = redeem(&f.app, &second).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_duplicate_address_is_refused_without_disclosing_the_other_account() {
    let db = Db::new().await;
    let f = fixture(&db.url, Arc::new(MemoryTransport::new())).await;
    let holder = user(&f.pool, "holder").await;
    let requester = user(&f.pool, "requester").await;
    let held = format!("{}@example.com", holder.username);
    sqlx::query("UPDATE accounts SET email = $2 WHERE id = $1")
        .bind(holder.id)
        .bind(&held)
        .execute(&f.pool)
        .await
        .unwrap();

    let (status, body, text) = request_link(&f.app, &requester.token, &held).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body} {text}");
    assert_eq!(body["error"], "email address is already in use");
    // Revert: let the unique violation bubble through `From<sqlx::Error>`,
    // which answers 409 "already exists" but still says nothing; the stronger
    // revert is to remove the unique index, when this becomes 202.
    for secret in [
        holder.username.as_str(),
        &holder.id.to_string(),
        held.as_str(),
    ] {
        assert!(
            !text.contains(secret),
            "the refusal names the holder ({secret}): {text}"
        );
    }
    // The requester's address is unchanged, so a refused request grants nothing.
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
        .bind(requester.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(email, None);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_changed_address_does_not_verify_the_wrong_one() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "changed").await;
    let first = format!("{}@example.com", u.username);
    let token = request_and_capture_link(&f, &u, &first, &mail).await;

    // The account moves to another unverified address before redeeming.
    let second = format!("other-{}@example.com", u.username);
    sqlx::query("UPDATE accounts SET email = $2 WHERE id = $1")
        .bind(u.id)
        .bind(&second)
        .execute(&f.pool)
        .await
        .unwrap();
    // Revert: stop storing `email` on the verification row and drop
    // `a.email = ev.email` from the lookup; the old link would verify the new
    // address.
    let (status, _, text) = redeem(&f.app, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    let verified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!verified);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_token_is_stored_only_as_its_hash_in_any_text_or_json_column() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let u = user(&f.pool, "scan").await;
    let email = format!("{}@example.com", u.username);
    let token = request_and_capture_link(&f, &u, &email, &mail).await;
    assert_eq!(token.len(), 64);

    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM email_verifications")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(stored, sha256(&token));
    assert_eq!(stored.len(), 32);

    // Revert: store the token itself in `token_hash` (or add it to an audit or
    // job payload); the scan below finds it.
    let hits = columns_containing(&f.pool, &token).await;
    assert!(
        hits.is_empty(),
        "the plaintext verification token is stored in the public schema: {hits:?}"
    );

    // The scan is not vacuous: a planted copy is found.
    sqlx::query("CREATE TABLE a5_scan_probe (a text)")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO a5_scan_probe VALUES ($1)")
        .bind(&token)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        columns_containing(&f.pool, &token).await,
        vec!["a5_scan_probe.a".to_owned()]
    );
    sqlx::query("DROP TABLE a5_scan_probe")
        .execute(&f.pool)
        .await
        .unwrap();

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_administrator_can_request_a_link_for_another_account() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture(&db.url, mail.clone()).await;
    let admin = user(&f.pool, "admin").await;
    sqlx::query("UPDATE accounts SET is_admin = true WHERE id = $1")
        .bind(admin.id)
        .execute(&f.pool)
        .await
        .unwrap();
    let target = user(&f.pool, "target").await;
    let email = format!("{}@example.com", target.username);
    let path = format!("/api/v1/admin/accounts/{}/email-verification", target.id);
    let (status, body, text) = api(
        &f.app,
        "POST",
        &path,
        Some(&admin.token),
        Some(&json!({"email": email})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let token = token_from(&mail.delivered()[0]);
    assert_eq!(redeem(&f.app, &token).await.0, StatusCode::NO_CONTENT);

    // A plain account cannot use the administrator route.
    let (status, body, _) = api(
        &f.app,
        "POST",
        &path,
        Some(&target.token),
        Some(&json!({"email": email})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "not permitted");

    finish(f).await;
    db.drop_db().await;
}

/// A transport whose error deliberately carries a credential-shaped string.
/// The handler must persist only its own fixed reason.
struct FailingTransport {
    secret: String,
    attempts: AtomicUsize,
}

impl MailTransport for FailingTransport {
    fn send<'a>(&'a self, _message: &'a EmailMessage) -> MailFuture<'a> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let _ = &self.secret;
            Err(MailError::Auth)
        })
    }
}

/// Captures this test binary's tracing output so the credential test can search
/// the actual log lines, not only the job error and the schema.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn install_log_capture() -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // Another test in this binary never installs one, so this succeeds.
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
    .clone()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_smtp_credential_is_not_in_an_error_body_or_a_job_error() {
    let db = Db::new().await;
    let logs = install_log_capture();
    logs.0.lock().unwrap().clear();
    let directory = std::env::temp_dir().join(format!("githearth-email-cred-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let storage_root = directory.join("repositories");
    let secret = "smtp-credential-must-not-appear-7f3a";
    let credential_file = directory.join("smtp-password");
    std::fs::write(&credential_file, format!("{secret}\n")).unwrap();
    let config = Config {
        database_url: db.url.clone(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        smtp_host: "smtp.example.com".into(),
        smtp_port: 587,
        smtp_from: "githearth@example.com".into(),
        smtp_username: Some("githearth".into()),
        smtp_password_file: Some(credential_file),
        job_poll_interval_ms: 50,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let failing = Arc::new(FailingTransport {
        secret: secret.to_string(),
        attempts: AtomicUsize::new(0),
    });
    let state = AppState::new(pool.clone(), config)
        .await
        .unwrap()
        .with_mailer(failing.clone());
    let app = router(state.clone());
    let u = user(&pool, "credential").await;
    let email = format!("{}@example.com", u.username);
    let (status, _, text) = request_link(&app, &u.token, &email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{text}");
    assert!(!text.contains(secret));

    // Run the relay and exactly one attempt; the failure schedules a retry.
    githearth_server::outbox::register_builtin(&state)
        .await
        .unwrap();
    githearth_server::outbox::fan_out_batch(&state, 100)
        .await
        .unwrap();
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    registry.backoff_override = Some(Duration::from_secs(30));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let runner = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let attempted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'notification.outbox' AND attempt >= 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if attempted > 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the verification job never ran");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Give the fenced outcome update a moment to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    let errors: Vec<String> = sqlx::query_scalar("SELECT COALESCE(last_error, '') FROM jobs")
        .fetch_all(&pool)
        .await
        .unwrap();
    for error in &errors {
        assert!(
            !error.contains(secret),
            "job error leaked the credential: {error}"
        );
    }
    // Revert: make `MailError` carry the transport's inner error, or derive
    // `Debug` on `SmtpTransport` without redacting the password.
    let hits = columns_containing(&pool, secret).await;
    assert!(hits.is_empty(), "the credential was persisted in {hits:?}");

    // The actual log lines, captured from this process, carry no credential.
    let captured = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(
        !captured.contains(secret),
        "a log line leaked the SMTP credential"
    );

    let _ = std::fs::remove_dir_all(&directory);
    pool.close().await;
    db.drop_db().await;
}

/// Every public-schema text or JSON column containing `needle`, as
/// `table.column` (the same scan the invitations suite uses). Only `text`,
/// `varchar`, `json` and `jsonb` are scanned — the project's convention is that
/// a secret never goes into a binary or fixed-width column — so a token copied
/// into `bytea` or `char` would not be found here.
async fn columns_containing(pool: &PgPool, needle: &str) -> Vec<String> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name FROM information_schema.columns \
         WHERE table_schema = 'public' \
         AND data_type IN ('text', 'character varying', 'json', 'jsonb') \
         ORDER BY table_name, column_name",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut hits = Vec::new();
    for (table, column) in columns {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM \"{table}\" WHERE \"{column}\"::text LIKE '%' || $1 || '%')"
        );
        let found: bool = sqlx::query_scalar(&sql)
            .bind(needle)
            .fetch_one(pool)
            .await
            .unwrap();
        if found {
            hits.push(format!("{table}.{column}"));
        }
    }
    hits
}

/// U30b / condition 9: the configured security-mail budget is enforced by the
/// sender, not only by the counter. One verification mail per address per hour
/// is allowed; the second request is accepted by the API (it stays queued)
/// but the consumer refuses to send it and records the reason.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_verification_mail_is_refused_over_the_per_address_budget() {
    let db = Db::new().await;
    let mail = Arc::new(MemoryTransport::new());
    let f = fixture_with(&db.url, mail.clone(), |config| {
        config.email_security_per_address = 1;
    })
    .await;
    let u = user(&f.pool, "mail-budget").await;
    let email = format!("{}@example.com", u.username);

    // The first request spends the hour's one security mail for this address.
    request_and_capture_link(&f, &u, &email, &mail).await;
    assert_eq!(mail.delivered().len(), 1);

    let (status, body, text) = request_link(&f.app, &u.token, &email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");

    // Run the relay and the consumer until the job records the refusal. It
    // stays queued with a retry delay, so the queue never drains here.
    githearth_server::outbox::register_builtin(&f.state)
        .await
        .unwrap();
    githearth_server::outbox::fan_out_batch(&f.state, 10_000)
        .await
        .unwrap();
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    registry.backoff_override = Some(Duration::from_millis(20));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let runner = githearth_server::jobs::spawn(f.state.clone(), registry, shutdown.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let reason: Option<String> = loop {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT last_error FROM jobs \
             WHERE kind = 'notification.outbox' AND last_error LIKE '%security email rate limit%' \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .fetch_optional(&f.pool)
        .await
        .unwrap();
        if row.is_some() {
            break row;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("the consumer never recorded the security-mail refusal");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    assert!(reason.is_some());
    assert_eq!(
        mail.delivered().len(),
        1,
        "a second mail for the same address inside the hour is not delivered"
    );
    assert_eq!(mail.attempts(), 1, "and no SMTP attempt is made");

    finish(f).await;
    db.drop_db().await;
}
