//! The published budgets at their enforcement points (M4.2, U30/U30b).
//!
//! Unit tests in `limits.rs` prove the counters; these prove the wiring: the
//! `/api/v1` middleware charges every route, organization creation refuses
//! over its cap, and sign-in refuses per address before hashing. Each test
//! fails if its call is removed.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, Connection, PgPool, postgres::PgConnectOptions};
use std::path::PathBuf;
use std::str::FromStr;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

// ---------------------------------------------------------------------------
// Per-test database
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
        let name = format!("githearth_limits_{}", Uuid::new_v4().simple());
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
    pool: PgPool,
    storage_root: PathBuf,
}

async fn fixture(url: &str, adjust: impl FnOnce(&mut Config)) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-limits-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let mut config = Config {
        database_url: url.to_owned(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        public_url: "https://forge.example".into(),
        ..Config::default()
    };
    adjust(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = githearth_server::AppState::new(pool.clone(), config)
        .await
        .unwrap();
    let app = router(state);
    Fixture {
        app,
        pool,
        storage_root,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

struct User {
    id: Uuid,
    token: String,
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(githearth_server::hash_password("correct-horse-battery").unwrap())
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
    User { id, token }
}

async fn organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,$2)")
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
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,'owner','account',$2,'organization',$3,$2)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    (id, slug)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// M5: the published per-organization budget is a rate on every `/api/v1`
/// route, not only on repository creation. The fourth request is refused and
/// another organization still has its own budget.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_api_budget_is_charged_per_organization_on_every_route() {
    let db = Db::new().await;
    let f = fixture(&db.url, |config| {
        config.api_requests_per_organization = 3;
        config.api_requests_per_account = 100;
    })
    .await;
    let u = user(&f.pool, "budget").await;
    let (_first_id, first) = organization(&f.pool, u.id).await;
    let (_second_id, second) = organization(&f.pool, u.id).await;

    let path = format!("/api/v1/organizations/{first}/repositories");
    for request in 1..=3 {
        let (status, body, text) = api(&f.app, "GET", &path, Some(&u.token), None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "request {request} inside the organization budget: {body} {text}"
        );
    }
    let (status, _body, _text) = api(&f.app, "GET", &path, Some(&u.token), None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the fourth request is over the published 300/min organization budget"
    );

    let other = format!("/api/v1/organizations/{second}/repositories");
    let (status, body, text) = api(&f.app, "GET", &other, Some(&u.token), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "another organization is not charged for the first one's requests: {body} {text}"
    );

    finish(f).await;
    db.drop_db().await;
}

/// The per-account budget is charged too, and applies to a route with no
/// organization in its path (`/api/v1/me`).
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_api_budget_is_charged_per_account_without_an_organization_path() {
    let db = Db::new().await;
    let f = fixture(&db.url, |config| {
        config.api_requests_per_account = 2;
        config.api_requests_per_organization = 100;
    })
    .await;
    let u = user(&f.pool, "account-budget").await;
    for request in 1..=2 {
        let (status, body, text) = api(&f.app, "GET", "/api/v1/me", Some(&u.token), None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "request {request} inside the account budget: {body} {text}"
        );
    }
    let (status, _body, _text) = api(&f.app, "GET", "/api/v1/me", Some(&u.token), None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the third request is over the published 60/min account budget"
    );
    finish(f).await;
    db.drop_db().await;
}

/// The organization-creation cap is enforced at the endpoint: one account
/// cannot multiply its Git, job and email share by creating organizations.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn organization_creation_is_capped_at_the_endpoint() {
    let db = Db::new().await;
    let f = fixture(&db.url, |config| {
        config.organization_creation_per_account = 1;
        config.api_requests_per_account = 100;
    })
    .await;
    let u = user(&f.pool, "creator").await;
    let body = json!({"slug": tag("created"), "name": "Created"});
    let (status, body, text) = api(
        &f.app,
        "POST",
        "/api/v1/organizations",
        Some(&u.token),
        Some(&body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the first organization is created: {body} {text}"
    );

    let body = json!({"slug": tag("created"), "name": "Created"});
    let (status, _body, _text) = api(
        &f.app,
        "POST",
        "/api/v1/organizations",
        Some(&u.token),
        Some(&body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the second organization is refused by the per-account cap"
    );
    finish(f).await;
    db.drop_db().await;
}

/// The anonymous login budget is charged before hashing: the third attempt
/// from one address is refused even though nothing was authenticated.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sign_in_is_refused_over_the_anonymous_login_budget() {
    let db = Db::new().await;
    let f = fixture(&db.url, |config| {
        config.anonymous_login_per_address = 2;
        config.anonymous_login_global = 100;
    })
    .await;
    let body = json!({"username": "nobody", "password": "wrong-password"});
    for attempt in 1..=2 {
        let (status, _body, text) = api(&f.app, "POST", "/api/v1/login", None, Some(&body)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} reaches password verification: {text}"
        );
    }
    let (status, _body, _text) = api(&f.app, "POST", "/api/v1/login", None, Some(&body)).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the third attempt is refused by the per-address login budget"
    );
    finish(f).await;
    db.drop_db().await;
}

/// The organization budget is charged only to a member. Otherwise a stranger
/// who knows an organization's slug could exhaust that tenant's 300/min with
/// requests the handler refuses anyway.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_stranger_cannot_spend_an_organizations_api_budget() {
    let db = Db::new().await;
    let f = fixture(&db.url, |config| {
        config.api_requests_per_organization = 1;
        config.api_requests_per_account = 100;
    })
    .await;
    let owner = user(&f.pool, "owner").await;
    let stranger = user(&f.pool, "stranger").await;
    let (_org_id, slug) = organization(&f.pool, owner.id).await;
    let path = format!("/api/v1/organizations/{slug}/repositories");

    for _ in 0..3 {
        let (status, _body, _text) = api(&f.app, "GET", &path, Some(&stranger.token), None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a non-member gets the ordinary not-found, not a rate limit"
        );
    }

    let (status, body, text) = api(&f.app, "GET", &path, Some(&owner.token), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the member's own first request still has the organization's budget: {body} {text}"
    );
    finish(f).await;
    db.drop_db().await;
}
