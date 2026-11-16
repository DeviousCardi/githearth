//! M11a 8A acceptance tests: change of address and self-service recovery
//! (case U32), the account-link rule, and the identity schema's shape.
//!
//! PostgreSQL-backed like the M4.1 suites: each test creates its own database
//! and storage root, seeds fixtures with SQL, and asserts through the router.
//! The mail transport is in memory, so no test opens an SMTP socket.
//!
//! Fail-without-the-mechanism notes are on the individual tests.

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode, header},
};
use githearth_server::{
    AppState, MIGRATOR,
    config::Config,
    connect, hash_password,
    identity::{
        ExternalIdentity, bind_account_data_key, bind_organization_data_key, link_external_identity,
    },
    mail::{EmailMessage, MemoryTransport},
    router,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, Connection, PgPool, postgres::PgConnectOptions};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
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

/// The documented derivation of the key lane's reference, written out
/// independently of the database function so the two must agree:
/// `SHA-256("githearth-owner-data-key-v1\0" || kind || 0x00 || uuid16)`, first
/// 16 bytes, UUID version 5 and variant bits set.
fn derived_data_key(kind: &str, owner: Uuid) -> Uuid {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"githearth-owner-data-key-v1\0");
    digest.update(kind.as_bytes());
    digest.update([0u8]);
    digest.update(owner.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.finalize()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Inserts the per-secret data-key row `secrets::seal_owned` writes: a random
/// key id wrapped under the owner's key, with the owner recorded.
async fn insert_owned_data_key(
    pool: &PgPool,
    id: Uuid,
    owner_key: Uuid,
    owner_kind: &str,
    owner: Uuid,
) {
    sqlx::query(
        "INSERT INTO secret_data_keys \
             (id, kek_id, algorithm, wrap_method, wrapped_key, owner_kind, owner_id, owner_key_id) \
         VALUES ($1, $2, 'aes256gcm', 'owner-aes256gcm', $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(owner_key.to_string())
    .bind(vec![0u8; 60])
    .bind(owner_kind)
    .bind(owner)
    .bind(owner_key)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_stored_secret(pool: &PgPool, id: Uuid, data_key: Uuid, name: &str) {
    sqlx::query(
        "INSERT INTO stored_secrets (id, name, data_key_id, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(name)
    .bind(data_key)
    .bind(vec![0u8; 12])
    .bind(vec![0u8; 16])
    .execute(pool)
    .await
    .unwrap();
}

/// Every public text/json column whose value contains `needle`. `bytea` and
/// `char` are outside this scan by design: a ciphertext is not a plaintext
/// column, and the review's task is to prove no *text* column kept a copy.
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

// ---------------------------------------------------------------------------
// Per-test database and fixture
// ---------------------------------------------------------------------------

struct Db {
    name: String,
    url: String,
    admin: PgConnectOptions,
}

impl Db {
    async fn new() -> Self {
        let base =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let name = format!("githearth_identity_{}", Uuid::new_v4().simple());
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

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    mail: Arc<MemoryTransport>,
}

async fn fixture(url: &str) -> Fixture {
    fixture_with(url, |_| {}).await
}

async fn fixture_with(url: &str, adjust: impl FnOnce(&mut Config)) -> Fixture {
    let mail = Arc::new(MemoryTransport::new());
    let storage_root = std::env::temp_dir().join(format!("githearth-identity-{}", Uuid::new_v4()));
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
    // A resolved client address makes audit write the client-IP digest, which
    // needs the installation key. It must live outside the storage root, or
    // the storage tar would carry it.
    if config.installation_key_file.as_os_str().is_empty() {
        config.installation_key_file =
            githearth_server::secrets::test_key_file(&std::env::temp_dir(), "identity").unwrap();
    }
    adjust(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), config)
        .await
        .unwrap()
        .with_mailer(mail.clone());
    let app = router(state.clone());
    Fixture {
        app,
        state,
        pool,
        storage_root,
        mail,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

struct User {
    id: Uuid,
    username: String,
    email: String,
    session: String,
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    user_with_verified(pool, prefix, true).await
}

/// Seeds one account with an address in `accounts.email` and, unless
/// `verified`, no verification time. The session is a normal bearer session.
async fn user_with_verified(pool: &PgPool, prefix: &str, verified: bool) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    let email = format!("{username}@example.com");
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash_password("correct-horse-battery").unwrap())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE accounts SET email = $2, email_verified_at = \
             CASE WHEN $3 THEN clock_timestamp() ELSE NULL END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(&email)
    .bind(verified)
    .execute(pool)
    .await
    .unwrap();
    let session = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')",
    )
    .bind(&session)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    User {
        id,
        username,
        email,
        session,
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

struct Raw {
    status: StatusCode,
    body: Vec<u8>,
    content_type: Option<String>,
    content_length: Option<String>,
}

impl Raw {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Raw {
    raw_from(app, None, method, path, token, body).await
}

/// [`raw`] with a resolved client address, so the per-client budgets can be
/// exercised the way the router's `client_ip_middleware` sets them up.
async fn raw_from(
    app: &Router,
    client: Option<IpAddr>,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Raw {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = body.map_or_else(String::new, ToString::to_string);
    let mut request = builder.body(Body::from(body)).unwrap();
    if let Some(client) = client {
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(client, 12345)));
    }
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Raw {
        status,
        body: bytes.to_vec(),
        content_type,
        content_length,
    }
}

/// A request whose JSON body must parse; tests that only need the status can
/// use [`raw`].
async fn json_call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value, String) {
    let response = raw(app, method, path, token, body).await;
    let text = response.text();
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    (response.status, value, text)
}

/// Runs the outbox fan-out and the job runner until no notification job is
/// pending. Mail goes to the in-memory transport, never a socket.
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

fn mails_to(mail: &MemoryTransport, to: &str) -> Vec<EmailMessage> {
    mail.delivered()
        .into_iter()
        .filter(|message| message.to == to)
        .collect()
}

fn token_from(message: &EmailMessage, marker: &str) -> String {
    let start = message
        .body
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker} link in {:?}", message.body))
        + marker.len();
    message.body[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect()
}

async fn request_change(f: &Fixture, session: &str, email: &str) -> (StatusCode, Value, String) {
    json_call(
        &f.app,
        "POST",
        "/api/v1/me/email-change",
        Some(session),
        Some(&json!({"email": email})),
    )
    .await
}

/// Requests a change, drains the mail, and returns the token from the link sent
/// to the new address.
async fn request_change_token(f: &Fixture, user: &User, new_email: &str) -> String {
    let (status, body, text) = request_change(f, &user.session, new_email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let delivered = mails_to(&f.mail, new_email);
    assert_eq!(delivered.len(), 1, "one change link to the new address");
    token_from(&delivered[0], "/change-email/")
}

async fn request_recovery(f: &Fixture, email: &str) -> (StatusCode, Value, String) {
    json_call(
        &f.app,
        "POST",
        "/api/v1/password-recovery/request",
        None,
        Some(&json!({"email": email})),
    )
    .await
}

/// [`request_recovery`] from a specific client address, for the per-client
/// budget tests.
async fn request_recovery_from(
    f: &Fixture,
    client: IpAddr,
    email: &str,
) -> (StatusCode, Value, String) {
    let response = raw_from(
        &f.app,
        Some(client),
        "POST",
        "/api/v1/password-recovery/request",
        None,
        Some(&json!({"email": email})),
    )
    .await;
    let text = response.text();
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    (response.status, value, text)
}

/// Requests recovery, drains the mail, and returns the token from the link.
async fn request_recovery_token(f: &Fixture, email: &str) -> String {
    let (status, body, text) = request_recovery(f, email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let delivered = mails_to(&f.mail, email);
    assert!(!delivered.is_empty(), "a reset link to {email}");
    token_from(delivered.last().unwrap(), "/reset-password/")
}

async fn request_recovery_token_from(f: &Fixture, client: IpAddr, email: &str) -> String {
    let (status, body, text) = request_recovery_from(f, client, email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    let delivered = mails_to(&f.mail, email);
    assert!(!delivered.is_empty(), "a reset link to {email}");
    token_from(delivered.last().unwrap(), "/reset-password/")
}

async fn confirm_recovery(f: &Fixture, email: &str, token: &str, password: &str) -> Raw {
    confirm_recovery_from(f, None, email, token, password).await
}

async fn confirm_recovery_from(
    f: &Fixture,
    client: Option<IpAddr>,
    email: &str,
    token: &str,
    password: &str,
) -> Raw {
    raw_from(
        &f.app,
        client,
        "POST",
        "/api/v1/password-recovery/confirm",
        None,
        Some(&json!({
            "email": email,
            "token": token,
            "new_password": password,
        })),
    )
    .await
}

async fn confirm_change(f: &Fixture, session: &str, token: &str) -> Raw {
    raw(
        &f.app,
        "POST",
        "/api/v1/email-change/confirm",
        Some(session),
        Some(&json!({"token": token})),
    )
    .await
}

// ---------------------------------------------------------------------------
// Change of address (U32)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_change_of_address_waits_for_the_new_address_and_the_old_address_keeps_mail_until_then() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let u = user(&f.pool, "move").await;
    let old = u.email.clone();
    let new = format!("new-{}@example.com", Uuid::new_v4().simple());

    let change_token = request_change_token(&f, &u, &new).await;

    // Mechanism under test: the request did not write `accounts.email`. If it
    // did, the old-address recovery below would send nothing and this fails.
    let (stored, verified): (String, bool) =
        sqlx::query_as("SELECT email, email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored, old, "the old address is still the account address");
    assert!(verified);
    assert!(
        mails_to(&f.mail, &old).is_empty(),
        "the request itself addresses only the new address"
    );

    // The old address still receives account mail before the new one verifies.
    let old_token = request_recovery_token(&f, &old).await;
    assert_eq!(old_token.len(), 64);
    assert_eq!(
        mails_to(&f.mail, &old)[0].subject,
        "[GitHearth] Reset your password"
    );

    // Confirm with the signed-in owner.
    let response = confirm_change(&f, &u.session, &change_token).await;
    assert_eq!(
        response.status,
        StatusCode::NO_CONTENT,
        "{}",
        response.text()
    );
    drain(&f.state).await;

    let (stored, verified): (String, bool) =
        sqlx::query_as("SELECT email, email_verified_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored, new, "the new address replaced the old one");
    assert!(verified, "the new address is verified by this act");

    // The old address is told; it is the one notice the change sends there.
    let notices: Vec<_> = mails_to(&f.mail, &old)
        .into_iter()
        .filter(|message| message.subject.contains("was changed"))
        .collect();
    assert_eq!(notices.len(), 1, "one notice to the old address");
    // The confirmation and the notice are different facts and must not share a
    // Message-ID: a receiving server that deduplicates on it would drop the
    // notice as a copy of the confirmation.
    let confirmation = mails_to(&f.mail, &new)
        .into_iter()
        .find(|message| message.subject.contains("Confirm your new email"))
        .expect("the confirmation mail");
    assert_ne!(
        confirmation.id, notices[0].id,
        "the notice has its own Message-ID"
    );

    // And now the old address receives no more account mail, while the new one
    // does. This is the other half of "stops only after the new one verifies".
    let old_before = mails_to(&f.mail, &old).len();
    let (status, _, text) = request_recovery(&f, &old).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{text}");
    drain(&f.state).await;
    assert_eq!(
        mails_to(&f.mail, &old).len(),
        old_before,
        "the old address no longer receives a reset link"
    );
    let _ = request_recovery_token(&f, &new).await;
    assert!(
        mails_to(&f.mail, &new)
            .iter()
            .any(|message| message.subject == "[GitHearth] Reset your password"),
        "the new address does"
    );

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_expired_reused_or_wrong_account_change_token_is_refused() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;

    // Wrong account: the token belongs to A, the session to B.
    let a = user(&f.pool, "owner").await;
    let b = user(&f.pool, "other").await;
    let a_new = format!("a-new-{}@example.com", Uuid::new_v4().simple());
    let a_token = request_change_token(&f, &a, &a_new).await;
    let wrong = confirm_change(&f, &b.session, &a_token).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.text());
    assert_eq!(
        wrong.body,
        br#"{"error":"invalid or expired email change token"}"#
    );

    // Expired.
    let e = user(&f.pool, "expired").await;
    let e_new = format!("e-new-{}@example.com", Uuid::new_v4().simple());
    let e_token = request_change_token(&f, &e, &e_new).await;
    sqlx::query(
        "UPDATE email_changes SET expires_at = clock_timestamp() - interval '1 minute' \
         WHERE account_id = $1 AND used_at IS NULL",
    )
    .bind(e.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let expired = confirm_change(&f, &e.session, &e_token).await;
    assert_eq!(expired.status, StatusCode::UNAUTHORIZED);

    // Reused: one success, then the same token again.
    let r = user(&f.pool, "reuse").await;
    let r_new = format!("r-new-{}@example.com", Uuid::new_v4().simple());
    let r_token = request_change_token(&f, &r, &r_new).await;
    let first = confirm_change(&f, &r.session, &r_token).await;
    assert_eq!(first.status, StatusCode::NO_CONTENT, "{}", first.text());
    let reused = confirm_change(&f, &r.session, &r_token).await;
    assert_eq!(reused.status, StatusCode::UNAUTHORIZED);

    // Unknown.
    let unknown = confirm_change(&f, &r.session, &"a".repeat(64)).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    // Every refusal is byte-for-byte the same: status, body, content-type and
    // content-length. A client cannot tell why a change link did not work.
    for (label, case) in [
        ("expired", &expired),
        ("reused", &reused),
        ("unknown", &unknown),
    ] {
        assert_eq!(case.status, wrong.status, "{label} status");
        assert_eq!(case.body, wrong.body, "{label} body");
        assert_eq!(
            case.content_type, wrong.content_type,
            "{label} content-type"
        );
        assert_eq!(
            case.content_length, wrong.content_length,
            "{label} content-length"
        );
    }

    // The refused attempts changed no address; only the successful one did.
    for (account, expected, label) in [(&a, &a.email, "wrong-account"), (&e, &e.email, "expired")] {
        let stored: String = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
            .bind(account.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(&stored, expected, "{label} attempt changed the address");
    }
    let r_stored: String = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
        .bind(r.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(r_stored, r_new);

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Self-service recovery (U32)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn recovery_refuses_unknown_wrong_account_and_expired_tokens_with_one_body() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let a = user(&f.pool, "recover").await;
    let b = user(&f.pool, "stranger").await;

    // Wrong account: A's token, B's address.
    let wrong_token = request_recovery_token(&f, &a.email).await;
    let wrong = confirm_recovery(&f, &b.email, &wrong_token, "new-password-value").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.text());
    assert_eq!(
        wrong.body,
        br#"{"error":"invalid or expired recovery token"}"#
    );

    // Expired.
    let expired_token = request_recovery_token(&f, &a.email).await;
    sqlx::query(
        "UPDATE password_reset_tokens SET expires_at = clock_timestamp() - interval '1 minute' \
         WHERE account_id = $1 AND used_at IS NULL AND purpose = 'self_service'",
    )
    .bind(a.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let expired = confirm_recovery(&f, &a.email, &expired_token, "new-password-value").await;
    assert_eq!(expired.status, StatusCode::UNAUTHORIZED);

    // Unknown.
    let unknown = confirm_recovery(&f, &a.email, &"b".repeat(64), "new-password-value").await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    // Reused: one success, then the same token again.
    let reused_token = request_recovery_token(&f, &a.email).await;
    let first = confirm_recovery(&f, &a.email, &reused_token, "new-password-value").await;
    assert_eq!(first.status, StatusCode::NO_CONTENT, "{}", first.text());
    let reused = confirm_recovery(&f, &a.email, &reused_token, "new-password-value").await;
    assert_eq!(reused.status, StatusCode::UNAUTHORIZED);

    for (label, case) in [
        ("expired", &expired),
        ("unknown", &unknown),
        ("reused", &reused),
    ] {
        assert_eq!(case.status, wrong.status, "{label} status");
        assert_eq!(case.body, wrong.body, "{label} body");
        assert_eq!(
            case.content_type, wrong.content_type,
            "{label} content-type"
        );
        assert_eq!(
            case.content_length, wrong.content_length,
            "{label} content-length"
        );
    }

    // The success changed the password exactly once, revoked the old session
    // and is audited once.
    let (status, _, _) = json_call(&f.app, "GET", "/api/v1/me", Some(&a.session), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old session is revoked"
    );

    let (status, _, text) = json_call(
        &f.app,
        "POST",
        "/api/v1/login",
        None,
        Some(&json!({
            "username": a.username,
            "password": "new-password-value",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the new password signs in: {text}");

    let (status, _, _) = json_call(
        &f.app,
        "POST",
        "/api/v1/login",
        None,
        Some(&json!({
            "username": a.username,
            "password": "correct-horse-battery",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old password is gone");

    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'account.password_recovered' AND actor_id = $1",
    )
    .bind(a.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_reset_request_above_the_per_address_velocity_is_refused() {
    let db = Db::new().await;
    // Two security mails per address per hour. The endpoint must call M4.2's
    // limiter: the third request for the same address is refused, a different
    // address is not.
    let f = fixture_with(&db.url, |config| {
        config.email_security_per_address = 2;
    })
    .await;
    let address = format!("velocity-{}@example.com", Uuid::new_v4().simple());
    for expected in [StatusCode::ACCEPTED, StatusCode::ACCEPTED] {
        let (status, body, text) = request_recovery(&f, &address).await;
        assert_eq!(status, expected, "{body} {text}");
    }
    let (status, body, text) = request_recovery(&f, &address).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body} {text}");
    assert_eq!(body["error"], "request rate limit exceeded");

    let other = format!("other-{}@example.com", Uuid::new_v4().simple());
    let (status, body, text) = request_recovery(&f, &other).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");

    // Above the budget no row is written either, so the refusal is not only a
    // response shape.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE email = $1")
            .bind(&address)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "an unknown address writes no token");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn anonymous_recovery_requests_are_budgeted_per_client_before_the_address_budget() {
    let db = Db::new().await;
    // Two security mails per client per hour, and a per-address budget high
    // enough that only the client budget can refuse. Distinct addresses are
    // the attack: each one would otherwise add a map entry.
    let f = fixture_with(&db.url, |config| {
        config.anonymous_security_per_address = 2;
        config.email_security_per_address = 100;
    })
    .await;
    let first: IpAddr = "203.0.113.10".parse().unwrap();
    let second: IpAddr = "203.0.113.11".parse().unwrap();

    for _ in 0..2 {
        let address = format!("budget-{}@example.com", Uuid::new_v4().simple());
        let (status, body, text) = request_recovery_from(&f, first, &address).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    }
    let third = format!("budget-{}@example.com", Uuid::new_v4().simple());
    let (status, body, text) = request_recovery_from(&f, first, &third).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body} {text}");
    assert_eq!(body["error"], "request rate limit exceeded");

    // The budget is keyed by client, not by destination address: a second
    // client is unaffected.
    let other = format!("budget-{}@example.com", Uuid::new_v4().simple());
    let (status, body, text) = request_recovery_from(&f, second, &other).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn anonymous_recovery_confirmation_charges_the_same_per_client_budget() {
    let db = Db::new().await;
    let f = fixture_with(&db.url, |config| {
        config.anonymous_security_per_address = 2;
        config.email_security_per_address = 100;
    })
    .await;
    let u = user(&f.pool, "confirm-budget").await;
    let spent: IpAddr = "203.0.113.20".parse().unwrap();
    let fresh: IpAddr = "203.0.113.21".parse().unwrap();

    // One request and one refused confirmation spend the client's two slots;
    // the third attempt is refused before the token lookup and before Argon2.
    let original: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = $1")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let token = request_recovery_token_from(&f, spent, &u.email).await;
    let wrong = confirm_recovery_from(
        &f,
        Some(spent),
        &u.email,
        &"b".repeat(64),
        "new-password-value",
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.text());
    let refused =
        confirm_recovery_from(&f, Some(spent), &u.email, &token, "new-password-value").await;
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        refused.text()
    );
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = $1")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(stored, original, "the refused confirmation changed nothing");

    // A second client has its own budget and can redeem the still-live token.
    let ok = confirm_recovery_from(&f, Some(fresh), &u.email, &token, "new-password-value").await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.text());

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_unverified_address_receives_no_recovery_link_and_no_change_link() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let u = user_with_verified(&f.pool, "unverified", false).await;

    let (status, body, text) = request_recovery(&f, &u.email).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body} {text}");
    drain(&f.state).await;
    assert!(
        mails_to(&f.mail, &u.email).is_empty(),
        "an unverified address receives nothing"
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE account_id = $1")
            .bind(u.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);

    // A change of address replaces a verified address; an unverified one uses
    // M4.1's verification flow instead.
    let new = format!("wanted-{}@example.com", Uuid::new_v4().simple());
    let (status, body, text) = request_change(&f, &u.session, &new).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body} {text}");
    assert_eq!(body["error"], "a verified email address is required");
    drain(&f.state).await;
    assert!(mails_to(&f.mail, &new).is_empty());

    // An address that belongs to no account gets the same 202 and no mail.
    let stranger = format!("stranger-{}@example.com", Uuid::new_v4().simple());
    let (status, _, text) = request_recovery(&f, &stranger).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{text}");
    drain(&f.state).await;
    assert!(mails_to(&f.mail, &stranger).is_empty());

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_recovery_request_for_an_unknown_address_is_indistinguishable_from_a_known_one() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let known = user(&f.pool, "known").await;
    let unknown = format!("missing-{}@example.com", Uuid::new_v4().simple());

    let a = request_recovery(&f, &known.email).await;
    let b = request_recovery(&f, &unknown).await;
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1);
    assert_eq!(a.2, b.2, "the response body is identical");

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Secrets in the schema (Minor 1)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn no_change_or_recovery_token_and_no_plaintext_password_is_stored_in_a_text_or_json_column()
{
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let u = user(&f.pool, "scan").await;
    let new = format!("scan-new-{}@example.com", Uuid::new_v4().simple());

    let change_token = request_change_token(&f, &u, &new).await;
    let confirmed = confirm_change(&f, &u.session, &change_token).await;
    assert_eq!(
        confirmed.status,
        StatusCode::NO_CONTENT,
        "{}",
        confirmed.text()
    );
    drain(&f.state).await;

    let recovery_token = request_recovery_token(&f, &new).await;
    let new_password = "scan-run-only-password-value";
    let reset = confirm_recovery(&f, &new, &recovery_token, new_password).await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT, "{}", reset.text());

    let needles = [
        ("change token", change_token.clone()),
        ("recovery token", recovery_token.clone()),
        ("new password", new_password.to_owned()),
    ];
    for (label, needle) in &needles {
        let hits = columns_containing(&f.pool, needle).await;
        assert!(
            hits.is_empty(),
            "the {label} is stored in a text/json column: {hits:?}"
        );
    }

    // The scan is not vacuous: a planted copy is found in a plain text column,
    // and in `operations.response_body` when it is filled the way the removed
    // idempotency store filled it.
    sqlx::query("CREATE TABLE a8_scan_probe (a text, b text)")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO a8_scan_probe VALUES ($1, '')")
        .bind(&change_token)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        columns_containing(&f.pool, &change_token).await,
        vec!["a8_scan_probe.a".to_owned()]
    );
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, response_status, \
         response_body, expires_at) VALUES ($1, $2, sha256($2::bytea), 'done', 202, \
         jsonb_build_object('token', $3::text), clock_timestamp() + interval '1 day')",
    )
    .bind(u.id)
    .bind(format!("a8-scan-probe-{}", Uuid::new_v4().simple()))
    .bind(&recovery_token)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        columns_containing(&f.pool, &recovery_token).await,
        vec!["operations.response_body".to_owned()],
        "the scan must see a token planted in `operations.response_body`"
    );
    sqlx::query("DELETE FROM operations WHERE actor_id = $1 AND key LIKE 'a8-scan-probe-%'")
        .bind(u.id)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE a8_scan_probe")
        .execute(&f.pool)
        .await
        .unwrap();
    let hits = columns_containing(&f.pool, &recovery_token).await;
    assert!(hits.is_empty(), "the probe rows are gone: {hits:?}");

    // The TOTP seed has no text column at all: its only row is
    // `stored_secrets.ciphertext`, a `bytea` the scan deliberately does not
    // inspect, and `totp_secrets` holds only `secret_id`. A plaintext seed in a
    // text column would have shown up above; there is no such column.
    let totp_text_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'totp_secrets' \
         AND data_type IN ('text', 'character varying', 'json', 'jsonb')",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert!(
        totp_text_columns.is_empty(),
        "totp_secrets has no text column: {totp_text_columns:?}"
    );

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Single use under concurrency and supersession (Minor 3)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_second_change_request_supersedes_the_first_and_exactly_one_concurrent_redemption_wins() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;

    // Supersession: the first link is dead once a second request exists.
    let u = user(&f.pool, "supersede").await;
    let first_new = format!("first-{}@example.com", Uuid::new_v4().simple());
    let second_new = format!("second-{}@example.com", Uuid::new_v4().simple());
    let first_token = request_change_token(&f, &u, &first_new).await;
    let second_token = request_change_token(&f, &u, &second_new).await;
    let stale = confirm_change(&f, &u.session, &first_token).await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED, "{}", stale.text());
    let fresh = confirm_change(&f, &u.session, &second_token).await;
    assert_eq!(fresh.status, StatusCode::NO_CONTENT, "{}", fresh.text());
    let stored: String = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(stored, second_new);

    // Concurrent redemption of one change token: exactly one 204.
    let c = user(&f.pool, "concurrent-change").await;
    let c_new = format!("concurrent-{}@example.com", Uuid::new_v4().simple());
    let c_token = request_change_token(&f, &c, &c_new).await;
    let (a, b) = tokio::join!(
        confirm_change(&f, &c.session, &c_token),
        confirm_change(&f, &c.session, &c_token),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED],
        "exactly one redemption wins: {} {}",
        a.text(),
        b.text()
    );

    // Concurrent redemption of one recovery token: exactly one 204.
    let r = user(&f.pool, "concurrent-recovery").await;
    let r_token = request_recovery_token(&f, &r.email).await;
    let (a, b) = tokio::join!(
        confirm_recovery(&f, &r.email, &r_token, "concurrent-password-value"),
        confirm_recovery(&f, &r.email, &r_token, "concurrent-password-value"),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED],
        "exactly one recovery redemption wins: {} {}",
        a.text(),
        b.text()
    );

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Account linking (U33's rule, used by 8B's OIDC callback)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn linking_an_external_identity_requires_a_verified_matching_address() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let a = user(&f.pool, "linked").await;
    let b = user(&f.pool, "unverified-account").await;
    sqlx::query("UPDATE accounts SET email_verified_at = NULL WHERE id = $1")
        .bind(b.id)
        .execute(&f.pool)
        .await
        .unwrap();

    let issuer = "https://idp.example".to_string();
    let matching = ExternalIdentity {
        issuer: issuer.clone(),
        subject: "subject-a".into(),
        email: Some(a.email.clone()),
        email_verified: true,
    };
    assert_eq!(
        link_external_identity(&f.pool, &matching).await.unwrap(),
        Some(a.id)
    );
    // A repeated callback resolves to the same account and writes no second row.
    assert_eq!(
        link_external_identity(&f.pool, &matching).await.unwrap(),
        Some(a.id)
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM external_identities WHERE issuer = $1")
            .bind(&issuer)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);

    // An unverified provider claim never links, even to the right address.
    for (label, identity) in [
        (
            "unverified",
            ExternalIdentity {
                issuer: issuer.clone(),
                subject: "subject-unverified".into(),
                email: Some(a.email.clone()),
                email_verified: false,
            },
        ),
        (
            "absent",
            ExternalIdentity {
                issuer: issuer.clone(),
                subject: "subject-absent".into(),
                email: None,
                email_verified: true,
            },
        ),
        (
            "mismatch",
            ExternalIdentity {
                issuer: issuer.clone(),
                subject: "subject-mismatch".into(),
                email: Some(format!("nobody-{}@example.com", Uuid::new_v4().simple())),
                email_verified: true,
            },
        ),
        (
            "account-not-verified",
            ExternalIdentity {
                issuer: issuer.clone(),
                subject: "subject-b".into(),
                email: Some(b.email.clone()),
                email_verified: true,
            },
        ),
    ] {
        assert_eq!(
            link_external_identity(&f.pool, &identity).await.unwrap(),
            None,
            "{label} must not link"
        );
    }
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM external_identities WHERE issuer = $1")
            .bind(&issuer)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "only the verified matching link was written");

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Schema shape
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_data_key_reference_is_the_key_lanes_deterministic_id_and_is_not_a_row_foreign_key() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let u = user(&f.pool, "keyed").await;
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1,$2,$3)")
        .bind(org)
        .bind(tag("keyed-org"))
        .bind("Keyed organization")
        .execute(&f.pool)
        .await
        .unwrap();

    // The contract is one algorithm, not a random id per writer. Pin it with a
    // test vector both lanes must satisfy, then check the database's own
    // function agrees with it for the real subject.
    assert_eq!(
        derived_data_key("account", Uuid::nil()),
        Uuid::parse_str("1ceb9657-767e-57c7-8761-8f47e3bb7126").unwrap()
    );
    assert_eq!(
        derived_data_key("organization", Uuid::nil()),
        Uuid::parse_str("d115e2d0-1551-5d38-a8ba-d2047adc4a7c").unwrap()
    );
    let sql_account: Uuid = sqlx::query_scalar("SELECT identity_account_data_key($1)")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(sql_account, derived_data_key("account", u.id));
    let account_key = sql_account;
    let org_key = derived_data_key("organization", org);

    // The key lane creates the owner key as an operator-custody file and
    // returns this id. The reference is a plain uuid: there is no
    // `secret_data_keys` row for an owner key (and a shredded key has no row),
    // so the writer must succeed on a fresh account.
    assert_eq!(
        bind_account_data_key(&f.pool, u.id).await.unwrap(),
        Some(account_key)
    );
    assert_eq!(
        bind_organization_data_key(&f.pool, org).await.unwrap(),
        Some(org_key)
    );
    let stored_account: Uuid = sqlx::query_scalar("SELECT data_key_id FROM accounts WHERE id = $1")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let stored_org: Uuid =
        sqlx::query_scalar("SELECT data_key_id FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        stored_account, account_key,
        "the account carries the key the key lane derives"
    );
    assert_eq!(stored_org, org_key);
    let key_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_data_keys WHERE id = $1")
        .bind(account_key)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        key_rows, 0,
        "an owner key is custody material, not a secret_data_keys row"
    );

    // The interface is a plain uuid with no foreign key: this is the b1 lane's
    // contract (the key is a file, and a shred leaves no row to point at). A
    // row FK here would make provisioning fail and would invite "delete the
    // row" as a fake shred.
    let foreign_keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage k \
           USING (constraint_name, table_schema, table_name) \
         WHERE tc.constraint_type = 'FOREIGN KEY' \
           AND tc.table_name IN ('accounts', 'organizations') \
           AND k.column_name = 'data_key_id'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(foreign_keys, 0, "the reference is not a row foreign key");

    // A reference that is not the derived value is refused by the schema, not
    // merely by the writer: this is the guard the review's probe removed.
    let other = user(&f.pool, "unkeyed").await;
    assert!(
        sqlx::query("UPDATE accounts SET data_key_id = $2 WHERE id = $1")
            .bind(other.id)
            .bind(Uuid::new_v4())
            .execute(&f.pool)
            .await
            .is_err(),
        "a free-form data-key reference is refused"
    );
    assert!(
        sqlx::query("UPDATE accounts SET data_key_id = $2 WHERE id = $1")
            .bind(other.id)
            .bind(account_key)
            .execute(&f.pool)
            .await
            .is_err(),
        "two accounts cannot share one reference"
    );
    // NULL stays valid: a pre-key row has no key yet.
    let unbound: Option<Uuid> =
        sqlx::query_scalar("SELECT data_key_id FROM accounts WHERE id = $1")
            .bind(other.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(unbound, None, "NULL means pre-key, not destroyed");

    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1)")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(
        exists,
        "the pseudonymous row stays, which is the tombstone promise"
    );

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn authenticator_seeds_are_stored_by_reference_and_recovery_codes_are_hashed() {
    let db = Db::new().await;
    let f = fixture(&db.url).await;
    let u = user(&f.pool, "factor").await;

    // A TOTP seed is a `stored_secrets` row; `totp_secrets` holds only its id.
    // No plaintext seed column exists, so no API could return one from here.
    let key = derived_data_key("account", u.id);
    assert_eq!(
        bind_account_data_key(&f.pool, u.id).await.unwrap(),
        Some(key)
    );
    // `secrets::seal_owned` gives the seed its own random data key and wraps it
    // under the account's owner key, recording the owner on that row.
    let dek = Uuid::new_v4();
    let secret = Uuid::new_v4();
    insert_owned_data_key(&f.pool, dek, key, "account", u.id).await;
    insert_stored_secret(&f.pool, secret, dek, &format!("totp/{}", u.id)).await;
    sqlx::query("INSERT INTO totp_secrets (account_id, secret_id) VALUES ($1,$2)")
        .bind(u.id)
        .bind(secret)
        .execute(&f.pool)
        .await
        .unwrap();
    let seed_key: Uuid = sqlx::query_scalar(
        "SELECT k.owner_key_id FROM stored_secrets s \
         JOIN secret_data_keys k ON k.id = s.data_key_id \
         JOIN totp_secrets t ON t.secret_id = s.id WHERE t.account_id = $1",
    )
    .bind(u.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let account_key: Uuid = sqlx::query_scalar("SELECT data_key_id FROM accounts WHERE id = $1")
        .bind(u.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(seed_key, account_key, "the seed is under the account key");

    // Revert: drop the constraint trigger; the next insert succeeds, which is
    // exactly the probe the review used to read one account's seed under
    // another account's key.
    let v = user(&f.pool, "foreign-key").await;
    let v_key = derived_data_key("account", v.id);
    assert_eq!(
        bind_account_data_key(&f.pool, v.id).await.unwrap(),
        Some(v_key)
    );
    // A seed wrapped under another account's owner key is refused for v: this
    // is the binding trigger, not the `secret_id` UNIQUE constraint.
    let foreign_dek = Uuid::new_v4();
    let foreign_secret = Uuid::new_v4();
    insert_owned_data_key(&f.pool, foreign_dek, key, "account", u.id).await;
    insert_stored_secret(
        &f.pool,
        foreign_secret,
        foreign_dek,
        &format!("totp/foreign/{}", v.id),
    )
    .await;
    assert!(
        sqlx::query("INSERT INTO totp_secrets (account_id, secret_id) VALUES ($1,$2)")
            .bind(v.id)
            .bind(foreign_secret)
            .execute(&f.pool)
            .await
            .is_err(),
        "an account cannot claim a seed wrapped under another account's key"
    );
    let v_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM totp_secrets WHERE account_id = $1")
        .bind(v.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(v_rows, 0, "the refused link wrote nothing");

    // Naming the account is not enough: the key row must actually be wrapped
    // by that account's owner key, not another account's.
    let misowned_dek = Uuid::new_v4();
    let misowned_secret = Uuid::new_v4();
    insert_owned_data_key(&f.pool, misowned_dek, key, "account", v.id).await;
    insert_stored_secret(
        &f.pool,
        misowned_secret,
        misowned_dek,
        &format!("totp/misowned/{}", v.id),
    )
    .await;
    assert!(
        sqlx::query("INSERT INTO totp_secrets (account_id, secret_id) VALUES ($1,$2)")
            .bind(v.id)
            .bind(misowned_secret)
            .execute(&f.pool)
            .await
            .is_err(),
        "a key row owned by v but wrapped under u's key is refused"
    );

    // A correctly wrapped seed is accepted, and neither side can move it.
    let v_dek = Uuid::new_v4();
    let v_secret = Uuid::new_v4();
    insert_owned_data_key(&f.pool, v_dek, v_key, "account", v.id).await;
    insert_stored_secret(&f.pool, v_secret, v_dek, &format!("totp/{}", v.id)).await;
    sqlx::query("INSERT INTO totp_secrets (account_id, secret_id) VALUES ($1,$2)")
        .bind(v.id)
        .bind(v_secret)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE secret_data_keys SET owner_key_id = $2 WHERE id = $1")
            .bind(v_dek)
            .bind(key)
            .execute(&f.pool)
            .await
            .is_err(),
        "a per-secret data key cannot be moved out from under its account"
    );
    assert!(
        sqlx::query("UPDATE stored_secrets SET data_key_id = $2 WHERE id = $1")
            .bind(v_secret)
            .bind(foreign_dek)
            .execute(&f.pool)
            .await
            .is_err(),
        "a stored seed cannot be repointed at another account's key row"
    );
    assert!(
        sqlx::query("UPDATE accounts SET data_key_id = NULL WHERE id = $1")
            .bind(v.id)
            .execute(&f.pool)
            .await
            .is_err(),
        "an account cannot drop the key a live seed depends on"
    );

    // An account with no provisioned key cannot have a seed at all: there is
    // no key that could shred it.
    let unkeyed = user(&f.pool, "unkeyed-factor").await;
    let unkeyed_dek = Uuid::new_v4();
    let unkeyed_secret = Uuid::new_v4();
    insert_owned_data_key(&f.pool, unkeyed_dek, key, "account", unkeyed.id).await;
    insert_stored_secret(
        &f.pool,
        unkeyed_secret,
        unkeyed_dek,
        &format!("totp/unkeyed/{}", unkeyed.id),
    )
    .await;
    assert!(
        sqlx::query("INSERT INTO totp_secrets (account_id, secret_id) VALUES ($1,$2)")
            .bind(unkeyed.id)
            .bind(unkeyed_secret)
            .execute(&f.pool)
            .await
            .is_err(),
        "a seed requires the account's data key to be provisioned"
    );

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'totp_secrets'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert!(columns.iter().any(|c| c == "secret_id"));
    assert!(
        !columns
            .iter()
            .any(|c| c == "seed" || c == "secret" || c == "totp_secret"),
        "no plaintext seed column exists: {columns:?}"
    );

    // Recovery codes are 32-byte digests, unique, and single-use by `used_at`.
    let hash = sha256("a recovery code value");
    sqlx::query("INSERT INTO recovery_codes (id, account_id, code_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind(u.id)
        .bind(&hash)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO recovery_codes (id, account_id, code_hash) VALUES ($1,$2,$3)")
            .bind(Uuid::new_v4())
            .bind(u.id)
            .bind(&hash)
            .execute(&f.pool)
            .await
            .is_err(),
        "a duplicate code digest is refused"
    );
    assert!(
        sqlx::query("INSERT INTO recovery_codes (id, account_id, code_hash) VALUES ($1,$2,$3)")
            .bind(Uuid::new_v4())
            .bind(u.id)
            .bind(vec![0u8; 31])
            .execute(&f.pool)
            .await
            .is_err(),
        "a short digest is refused"
    );

    // A passkey stores its credential id, public key, sign counter and RP id.
    sqlx::query(
        "INSERT INTO webauthn_credentials \
             (id, account_id, credential_id, public_key, sign_count, rp_id) \
         VALUES ($1,$2,$3,$4,0,'forge.example')",
    )
    .bind(Uuid::new_v4())
    .bind(u.id)
    .bind(vec![1u8; 16])
    .bind(vec![2u8; 32])
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO webauthn_credentials \
                 (id, account_id, credential_id, public_key, sign_count, rp_id) \
             VALUES ($1,$2,$3,$4,-1,'forge.example')",
        )
        .bind(Uuid::new_v4())
        .bind(u.id)
        .bind(vec![3u8; 16])
        .bind(vec![4u8; 32])
        .execute(&f.pool)
        .await
        .is_err(),
        "a negative sign counter is refused"
    );

    finish(f).await;
    db.drop_db().await;
}
