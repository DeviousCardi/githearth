//! M4.2 increment 7B acceptance: token subjects, principal confinement,
//! issuance/revocation and the legacy deprecation window (U26, U27).
//!
//! Requires a disposable PostgreSQL database via `GITHEARTH_TEST_DATABASE_URL`
//! and, for the SSH and Git HTTP cases, the system `git` and `ssh` binaries.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgPool};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn database_url() -> String {
    std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL")
}

fn temp_storage_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("githearth-tp-{label}-{}", Uuid::new_v4()))
}

/// A database this test starts empty, so a migration or deadline change cannot
/// disturb another test.
async fn fresh_database(label: &str) -> String {
    let url = database_url();
    let (origin, _) = url
        .rsplit_once('/')
        .expect("URL must end in a database name");
    let maint = PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    let name = format!(
        "githearth_tp_{label}_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
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

async fn api_pool(url: Option<String>, storage: Option<PathBuf>) -> (PgPool, Router, Config) {
    let config = Config {
        database_url: url.unwrap_or_else(database_url),
        database_max_connections: 6,
        database_timeout_seconds: 3,
        storage_root: storage.unwrap_or_else(|| temp_storage_root("api")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config.clone()).await.unwrap());
    (pool, app, config)
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
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

async fn login(app: &Router, username: &str, password: &str) -> String {
    let (status, value) = request(
        app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value["token"].as_str().unwrap().to_owned()
}

async fn fixture_account(pool: &PgPool, username: &str, password: &str) -> Uuid {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(hash)
    .fetch_one(pool)
    .await
    .unwrap()
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

async fn fixture_repo(pool: &PgPool, org_id: Uuid, name: &str, owner: Uuid) -> Uuid {
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind(name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    repo_id
}

async fn fixture_service_account(pool: &PgPool, org_id: Uuid, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO service_accounts (id,organization_id,name,created_by) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(org_id)
    .bind(tag("service"))
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn fixture_run(pool: &PgPool, org_id: Uuid, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO ci_runs (id,organization_id,triggered_by) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(org_id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn grant_repo(
    pool: &PgPool,
    org_id: Uuid,
    repo_id: Uuid,
    subject_kind: &str,
    subject_id: Uuid,
    role: &str,
    owner: Uuid,
) {
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) \
         VALUES ($1,$2,$3,$4,'repository',$5,$6,$7)",
    )
    .bind(Uuid::new_v4())
    .bind(role)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(org_id)
    .bind(repo_id)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
}

async fn create_scoped(
    app: &Router,
    session: &str,
    subject: Option<Value>,
    resource: Uuid,
    scopes: &[&str],
    audience: &[&str],
    expiry_seconds: i64,
) -> (Uuid, String, Value) {
    let mut body = json!({
        "name": tag("token"),
        "scopes": scopes,
        "resource_set": [{"type": "repository", "id": resource}],
        "audience": audience,
        "expires_in_seconds": expiry_seconds,
    });
    if let Some(subject) = subject {
        body["subject"] = subject;
    }
    let (status, value) = request(app, "POST", "/api/v1/user/tokens", Some(session), body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    assert!(value.get("token_hash").is_none());
    let id = value["id"].as_str().unwrap().parse().unwrap();
    let token = value["token"].as_str().unwrap().to_owned();
    (id, token, value)
}

fn repo_path(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}")
}

// ---------------------------------------------------------------------------
// Migration: existing rows and the audit chain
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn migration_preserves_existing_tokens_and_the_audit_chain() {
    let url = fresh_database("mig").await;
    let pool = PgPool::connect(&url).await.unwrap();
    // Everything before 0055: the schema M4.1 left behind. Filter by version,
    // not "all but the newest": 0056 arrived after this test was written, so
    // `len - 1` included 0055 and stopped meaning "before 0055".
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < 55)
                .cloned()
                .collect::<Vec<_>>(),
        ),
        ..MIGRATOR
    };
    previous.run(&pool).await.unwrap();

    let account = Uuid::new_v4();
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
        .bind(account)
        .bind(tag("legacy"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Legacy')")
        .bind(org)
        .bind(tag("org"))
        .execute(&pool)
        .await
        .unwrap();
    let plaintext = "gth_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let hash = Sha256::digest(plaintext.as_bytes()).to_vec();
    let token_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO access_tokens (id,account_id,name,token_hash,scopes,expires_at) \
         VALUES ($1,$2,'old token',$3,ARRAY['repo:read','repo:write'],NULL)",
    )
    .bind(token_id)
    .bind(account)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();
    for kind in ["organization.created", "repository.pushed"] {
        sqlx::query(
            "INSERT INTO audit_events (id,organization_id,actor_id,kind) VALUES ($1,$2,$3,$4)",
        )
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(account)
        .bind(kind)
        .execute(&pool)
        .await
        .unwrap();
    }
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();

    // The real upgrade.
    MIGRATOR.run(&pool).await.unwrap();

    let row: (String, bool, Vec<String>, Option<Uuid>, bool, bool) = sqlx::query_as(
        "SELECT subject_kind, legacy, scopes, account_id, legacy_deadline IS NOT NULL, \
                legacy_deadline = '2027-07-01T00:00:00Z'::timestamptz \
         FROM access_tokens WHERE id = $1",
    )
    .bind(token_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "account");
    assert!(row.1, "an existing token stays legacy");
    assert_eq!(row.2, vec!["repo:read".to_owned(), "repo:write".to_owned()]);
    assert_eq!(row.3, Some(account));
    assert!(row.4, "legacy tokens get the pinned deadline");
    assert!(
        row.5,
        "the deadline is the first instant of the UTC day after 2027-06-30"
    );

    // The token still authenticates with the same authority.
    let ((who, _), _) = githearth_server::authenticate_access_token(&pool, hash)
        .await
        .unwrap();
    assert_eq!(who.id, account);
    assert!(!who.read_only, "repo:write is still write authority");
    assert!(
        who.confinement.is_none(),
        "legacy tokens keep account-wide reach"
    );

    // Every old audit row is an account actor with itself as the account.
    let kinds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE actor_kind = 'account' AND actor_account_id = actor_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kinds, before);

    let mut conn = pool.acquire().await.unwrap();
    let verification = githearth_server::audit::verify(&mut conn, Some(org))
        .await
        .unwrap();
    assert!(verification.ok);
    assert_eq!(verification.rows, before as u64);
    drop(conn);

    let actor: (String, Option<Uuid>) =
        sqlx::query_as("SELECT actor_kind, actor_account_id FROM audit_events WHERE actor_id = $1")
            .bind(account)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(actor, ("account".to_owned(), Some(account)));
    pool.close().await;
}

/// The rehash must walk a chain in `seq` order, not `created_at` order, and must
/// never renumber. `created_at` defaults to the transaction start time while
/// `seq` is assigned under the chain lock at commit order, so two concurrent
/// transactions invert them: here the transaction that started first commits
/// second. An earlier revision of 0055 rehashed in `created_at` order, which
/// permuted `seq`, broke `audit::verify` and left every `outbox_events
/// (chain, seq)` pointing at a different event.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn migration_rehashes_in_seq_order_without_permuting_seq_or_outbox() {
    let url = fresh_database("mig_order").await;
    let pool = PgPool::connect(&url).await.unwrap();
    // Everything before 0055, by version: the rehash under test is 0055.
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < 55)
                .cloned()
                .collect::<Vec<_>>(),
        ),
        ..MIGRATOR
    };
    previous.run(&pool).await.unwrap();
    // Prove the fixture really stops before 0055, so applying MIGRATOR below is
    // what rehashes the V3 chain into V4 rather than a no-op.
    let applied: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 54, "the fixture must stop before 0055");

    let account = Uuid::new_v4();
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
        .bind(account)
        .bind(tag("order"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Order')")
        .bind(org)
        .bind(tag("org"))
        .execute(&pool)
        .await
        .unwrap();

    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut conn_a = pool.acquire().await.unwrap();
    let mut conn_b = pool.acquire().await.unwrap();
    // B starts first, so its `now()` is earlier, but A takes the chain lock and
    // inserts first. B's insert then waits for A's commit and receives seq 2.
    let mut tx_b = conn_b.begin().await.unwrap();
    let mut tx_a = conn_a.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO audit_events (id, organization_id, actor_id, kind) \
         VALUES ($1,$2,$3,'order.a_inserted_first')",
    )
    .bind(first)
    .bind(org)
    .bind(account)
    .execute(&mut *tx_a)
    .await
    .unwrap();
    let commit_a = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx_a.commit().await
    };
    let insert_b = sqlx::query(
        "INSERT INTO audit_events (id, organization_id, actor_id, kind) \
         VALUES ($1,$2,$3,'order.b_created_first')",
    )
    .bind(second)
    .bind(org)
    .bind(account)
    .execute(&mut *tx_b);
    let (committed, inserted) = tokio::join!(commit_a, insert_b);
    committed.unwrap();
    inserted.unwrap();
    tx_b.commit().await.unwrap();
    drop(conn_a);
    drop(conn_b);

    // The two orders really do differ, or the test proves nothing.
    let inverted: bool = sqlx::query_scalar(
        "SELECT (SELECT created_at FROM audit_events WHERE id = $1) \
              < (SELECT created_at FROM audit_events WHERE id = $2)",
    )
    .bind(second)
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        inverted,
        "the concurrent transactions must invert created_at and seq"
    );

    // Every audited write also writes its outbox row.
    for id in [first, second] {
        sqlx::query(
            "INSERT INTO outbox_events (id, organization_id, kind, payload) \
             VALUES ($1,$2,'order.event','{}'::jsonb)",
        )
        .bind(id)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    }
    let seq_before: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT id, seq FROM audit_events ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    let outbox_before: Vec<(Uuid, Uuid, i64)> =
        sqlx::query_as("SELECT id, chain, seq FROM outbox_events ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seq_before, vec![(first, 1), (second, 2)]);
    let hashes_before: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT hash FROM audit_events ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();

    MIGRATOR.run(&pool).await.unwrap();

    // The chain verifies under the same link check `audit::verify` mirrors.
    let mut conn = pool.acquire().await.unwrap();
    let verification = githearth_server::audit::verify(&mut conn, Some(org))
        .await
        .unwrap();
    assert!(
        verification.ok,
        "chain does not verify: ok={} rows={} first_bad_seq={:?}",
        verification.ok, verification.rows, verification.first_bad_seq
    );
    assert_eq!(verification.rows, 2);
    drop(conn);

    let seq_after: Vec<(Uuid, i64)> =
        sqlx::query_as("SELECT id, seq FROM audit_events ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seq_after, seq_before, "the migration must not permute seq");
    let hashes_after: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT hash FROM audit_events ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_ne!(
        hashes_after, hashes_before,
        "0055 must rehash the V3 chain into the V4 encoding"
    );
    let outbox_after: Vec<(Uuid, Uuid, i64)> =
        sqlx::query_as("SELECT id, chain, seq FROM outbox_events ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        outbox_after, outbox_before,
        "(chain, seq) must still resolve to the same event"
    );
    let aligned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events e JOIN outbox_events o ON o.id = e.id \
         WHERE o.chain = $1 AND o.seq = e.seq",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(aligned, 2);
    pool.close().await;
}

/// The token subject CHECK must make "two subjects" and "no subject"
/// unrepresentable, independent of the API's own validation.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn token_subjects_cannot_be_two_or_none() {
    let url = fresh_database("subject").await;
    let pool = PgPool::connect(&url).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let account = fixture_account(&pool, &tag("subject"), "unused-password").await;
    let (org, _) = fixture_organization(&pool, account).await;
    let service = fixture_service_account(&pool, org, account).await;
    let run = fixture_run(&pool, org, account).await;

    let insert = |id: Uuid, kind: &'static str, account_id, service_id, run_id| {
        let pool = pool.clone();
        async move {
            let mut token_hash = Uuid::new_v4().as_bytes().to_vec();
            token_hash.extend_from_slice(Uuid::new_v4().as_bytes());
            sqlx::query(
                "INSERT INTO access_tokens \
                 (id, account_id, service_account_id, run_id, subject_kind, name, token_hash, \
                  scopes, resource_set, expires_at, legacy) \
                 VALUES ($1,$2,$3,$4,$5,'subject test',$6,ARRAY['repo:read'],'[]'::jsonb, \
                         clock_timestamp() + interval '1 hour', false)",
            )
            .bind(id)
            .bind(account_id)
            .bind(service_id)
            .bind(run_id)
            .bind(kind)
            .bind(token_hash)
            .execute(&pool)
            .await
        }
    };

    for (kind, account_id, service_id, run_id) in [
        ("account", Some(account), None, None),
        ("service_account", None, Some(service), None),
        ("run", None, None, Some(run)),
    ] {
        insert(Uuid::new_v4(), kind, account_id, service_id, run_id)
            .await
            .unwrap_or_else(|e| panic!("valid {kind} subject rejected: {e}"));
    }
    for (kind, account_id, service_id, run_id, why) in [
        (
            "run",
            Some(account),
            None,
            Some(run),
            "a run token must not also name an account",
        ),
        (
            "run",
            None,
            Some(service),
            Some(run),
            "a run token must not also name a service account",
        ),
        (
            "account",
            Some(account),
            None,
            Some(run),
            "an account token must not also name a run",
        ),
        (
            "account",
            None,
            None,
            None,
            "a token must name some subject",
        ),
        (
            "service_account",
            None,
            None,
            None,
            "a service-account token must name its service account",
        ),
    ] {
        let result = insert(Uuid::new_v4(), kind, account_id, service_id, run_id).await;
        assert!(result.is_err(), "accepted a malformed token subject: {why}");
    }
    pool.close().await;
}

/// An audit row names exactly one principal: the account itself, a service
/// account with no account, or a run with its triggering account. It also must
/// name a principal that exists, which the dropped accounts foreign key used to
/// guarantee.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn audit_actors_cannot_be_two_subjects_or_none() {
    let url = fresh_database("actor").await;
    let pool = PgPool::connect(&url).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let account = fixture_account(&pool, &tag("actor"), "unused-password").await;
    let other = fixture_account(&pool, &tag("other"), "unused-password").await;
    let (org, _) = fixture_organization(&pool, account).await;
    let service = fixture_service_account(&pool, org, account).await;
    let run = fixture_run(&pool, org, account).await;

    // The three valid shapes are accepted.
    for (kind, actor, actor_account) in [
        ("account", account, None),
        ("service_account", service, None),
        ("run", run, Some(account)),
    ] {
        sqlx::query(
            "INSERT INTO audit_events \
             (id, organization_id, actor_id, actor_kind, actor_account_id, kind) \
             VALUES ($1,$2,$3,$4,$5,'actor.valid')",
        )
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(actor)
        .bind(kind)
        .bind(actor_account)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("valid {kind} actor rejected: {e}"));
    }
    let filled: bool = sqlx::query_scalar(
        "SELECT actor_account_id = actor_id FROM audit_events \
         WHERE actor_id = $1 AND actor_kind = 'account'",
    )
    .bind(account)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(filled, "an account actor is its own account");

    // Malformed shapes and missing referents are refused.
    let cases = [
        (run, "run", None, "a run must name its triggering account"),
        (
            service,
            "service_account",
            Some(account),
            "a service account has no account",
        ),
        (
            account,
            "account",
            Some(other),
            "an account actor is its own account, not another",
        ),
        (
            Uuid::new_v4(),
            "account",
            None,
            "the account actor must exist",
        ),
        (
            Uuid::new_v4(),
            "service_account",
            None,
            "the service account actor must exist",
        ),
        (
            Uuid::new_v4(),
            "run",
            Some(account),
            "the run actor must exist",
        ),
    ];
    for (actor, kind, actor_account, why) in cases {
        let result = sqlx::query(
            "INSERT INTO audit_events \
             (id, organization_id, actor_id, actor_kind, actor_account_id, kind) \
             VALUES ($1,$2,$3,$4,$5,'actor.invalid')",
        )
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(actor)
        .bind(kind)
        .bind(actor_account)
        .execute(&pool)
        .await;
        assert!(result.is_err(), "accepted a malformed actor: {why}");
    }
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Issuance, one-time delivery and immediate revocation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn issuance_returns_the_token_once_and_revocation_is_immediate() {
    let (pool, app, _config) = api_pool(None, None).await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let (org, org_slug) = fixture_organization(&pool, owner).await;
    let repo = tag("repo");
    let repo_id = fixture_repo(&pool, org, &repo, owner).await;

    let (id, token, created) =
        create_scoped(&app, &session, None, repo_id, &["repo:read"], &["api"], 900).await;
    assert_eq!(created["subject_kind"], "account");
    assert_eq!(created["legacy"], false);
    assert_eq!(created["audience"], json!(["api"]));
    assert_eq!(created["resource_set"][0]["id"], repo_id.to_string());

    // The plaintext exists only in the response: the row stores its hash.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM access_tokens WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, Sha256::digest(token.as_bytes()).to_vec());
    let any_plaintext: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM access_tokens WHERE encode(token_hash,'escape') = $1)",
    )
    .bind(&token)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!any_plaintext);

    // It reads inside its set and is refused outside it.
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let other = tag("other");
    fixture_repo(&pool, org, &other, owner).await;
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &other),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // A write with a read scope is still refused.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories"),
            Some(&token),
            json!({"name": tag("blocked"), "description": "", "visibility": "private"}),
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Listing never returns the plaintext.
    let (status, listed) = request(
        &app,
        "GET",
        "/api/v1/user/tokens",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id.to_string())
        .unwrap();
    assert!(item.get("token").is_none());
    assert_eq!(item["subject_kind"], "account");

    // Revocation is immediate.
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/v1/user/tokens/{id}"),
            Some(&session),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Expiry is equally immediate.
    let (_id2, token2, _) =
        create_scoped(&app, &session, None, repo_id, &["repo:read"], &["api"], 900).await;
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token2),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE access_tokens SET expires_at = clock_timestamp() - interval '1 second' WHERE token_hash = $1")
        .bind(Sha256::digest(token2.as_bytes()).to_vec())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token2),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Subject confinement on the API
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn every_subject_is_confined_on_the_api() {
    let (pool, app, _config) = api_pool(None, None).await;
    let username = tag("owner");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let (org, org_slug) = fixture_organization(&pool, owner).await;
    let repo = tag("inside");
    let inside = fixture_repo(&pool, org, &repo, owner).await;
    let outside_name = tag("outside");
    fixture_repo(&pool, org, &outside_name, owner).await;

    // --- account subject -------------------------------------------------
    let (_id, account_token, _) =
        create_scoped(&app, &session, None, inside, &["repo:read"], &["api"], 900).await;
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&account_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &outside_name),
            Some(&account_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // --- service account subject ----------------------------------------
    let service = fixture_service_account(&pool, org, owner).await;
    grant_repo(
        &pool,
        org,
        inside,
        "service_account",
        service,
        "read",
        owner,
    )
    .await;
    let (_id, service_token, created) = create_scoped(
        &app,
        &session,
        Some(json!({"kind": "service_account", "id": service})),
        inside,
        &["repo:read"],
        &["api"],
        900,
    )
    .await;
    assert_eq!(created["subject_kind"], "service_account");
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&service_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &outside_name),
            Some(&service_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A package scope cannot read a repository.
    let (_id, wrong_scope, _) = create_scoped(
        &app,
        &session,
        Some(json!({"kind": "service_account", "id": service})),
        inside,
        &["package:read"],
        &["api"],
        900,
    )
    .await;
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&wrong_scope),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A Git-only token is not an API credential.
    let (_id, git_only, _) = create_scoped(
        &app,
        &session,
        Some(json!({"kind": "service_account", "id": service})),
        inside,
        &["repo:read"],
        &["git"],
        900,
    )
    .await;
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&git_only),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // The service account's token event has its own audit actor.
    let actor: (String, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT actor_kind, actor_id, actor_account_id FROM audit_events \
         WHERE kind = 'token.created' AND actor_kind = 'service_account' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor.0, "service_account");
    assert_eq!(actor.1, service);
    assert_eq!(actor.2, None);

    // --- run subject -----------------------------------------------------
    let run = fixture_run(&pool, org, owner).await;
    let (_id, run_token, created) = create_scoped(
        &app,
        &session,
        Some(json!({"kind": "run", "id": run})),
        inside,
        &["repo:read"],
        &["api"],
        900,
    )
    .await;
    assert_eq!(created["subject_kind"], "run");
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&run_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &outside_name),
            Some(&run_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // The run's audit actor resolves to the run and its triggering account.
    let actor: (String, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT actor_kind, actor_id, actor_account_id FROM audit_events \
         WHERE actor_kind = 'run' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, ("run".to_owned(), run, Some(owner)));

    // Ending the run refuses the token at the next admission.
    sqlx::query(
        "UPDATE ci_runs SET status = 'cancelled', ended_at = clock_timestamp() WHERE id = $1",
    )
    .bind(run)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&run_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Legacy deprecation (U27)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn legacy_token_works_until_its_stated_end_then_is_refused() {
    let url = fresh_database("legacy").await;
    let (pool, app, config) = api_pool(Some(url), None).await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let (org, org_slug) = fixture_organization(&pool, owner).await;
    let repo = tag("repo");
    fixture_repo(&pool, org, &repo, owner).await;

    // A legacy request: no resource set.
    let (status, value) = request(
        &app,
        "POST",
        "/api/v1/user/tokens",
        Some(&session),
        json!({"name": tag("legacy"), "scopes": ["repo:read"], "expires_in_days": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    assert_eq!(value["legacy"], true);
    let token = value["token"].as_str().unwrap().to_owned();
    let id: Uuid = value["id"].as_str().unwrap().parse().unwrap();

    // The date the operator reads is the date enforced: the stored cutoff is
    // exactly the first instant of the next UTC day (not a session-zone
    // midnight, and not a day early).
    let exact: bool = sqlx::query_scalar(
        "SELECT legacy_deadline = '2027-07-01T00:00:00Z'::timestamptz \
         FROM access_tokens WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        exact,
        "the default 2027-06-30 must be enforced at 2027-07-01T00:00:00Z"
    );

    // Inside the window it still works exactly as before.
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );

    // `doctor` is where an operator reads the date and the remaining count. It
    // must read this fresh database, not the scratch base database.
    let checks = githearth_server::doctor::report(&config).await;
    let line = checks
        .iter()
        .find(|c| c.name == "tokens")
        .expect("doctor reports legacy tokens");
    assert!(
        line.detail.contains("2027-07-01T00:00:00Z"),
        "doctor must print the enforced cutoff: {}",
        line.detail
    );
    assert!(
        line.detail.contains("1 legacy"),
        "doctor must count the token in this database: {}",
        line.detail
    );

    // Narrowing is monotone: a restart with a later configured date must not
    // re-extend a token that was already shortened.
    sqlx::query("UPDATE access_tokens SET legacy_deadline = '2026-12-01T00:00:00Z' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    drop(state);
    let still_short: bool = sqlx::query_scalar(
        "SELECT legacy_deadline = '2026-12-01T00:00:00Z'::timestamptz \
         FROM access_tokens WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        still_short,
        "start-up may shorten the deadline but must never extend it"
    );

    // After the cutoff the token is refused. Moving the stored instant into the
    // past exercises the same `legacy_deadline <= now()` predicate that a clock
    // past 2027-07-01T00:00:00Z does.
    sqlx::query("UPDATE access_tokens SET legacy_deadline = clock_timestamp() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "GET",
            &repo_path(&org_slug, &repo),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // A legacy row cannot exist without a deadline; NULL used to mean "never
    // expires".
    assert!(
        sqlx::query("UPDATE access_tokens SET legacy_deadline = NULL WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err(),
        "the schema must refuse a legacy token with no deadline"
    );

    // The configuration cannot extend the announced window, and it rejects an
    // impossible calendar date instead of swallowing the UPDATE error.
    let mut extended = config.clone();
    extended.legacy_token_deprecation_date = "2027-07-01".to_owned();
    assert!(
        extended.validate().is_err(),
        "2027-07-01 extends the window"
    );
    let mut impossible = config.clone();
    impossible.legacy_token_deprecation_date = "2027-02-30".to_owned();
    assert!(impossible.validate().is_err(), "2027-02-30 is not a date");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Git HTTP: account, service account and run tokens
// ---------------------------------------------------------------------------

fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_SSL_NO_VERIFY", "1")
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

async fn seed_bare(repo_path: &str, work_dir: &str) {
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work_dir,
        "main:refs/heads/main",
    ]))
    .await;
}

async fn clone(port: u16, org: &str, repo: &str, token: &str) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!("githearth-tp-clone-{}", Uuid::new_v4()));
    let url = format!("http://x-access-token:{token}@127.0.0.1:{port}/{org}/{repo}.git");
    let out = git_run(git_cmd(&["clone", &url, dir.to_str().unwrap()])).await;
    let _ = std::fs::remove_dir_all(&dir);
    out
}

struct GitFixture {
    pool: PgPool,
    app: Router,
    port: u16,
    org: String,
    inside: String,
    outside: String,
    owner: Uuid,
    session: String,
    inside_id: Uuid,
}

async fn git_fixture() -> GitFixture {
    let storage = temp_storage_root("git");
    std::fs::create_dir_all(&storage).unwrap();
    let config = Config {
        database_url: database_url(),
        database_max_connections: 6,
        database_timeout_seconds: 3,
        storage_root: storage.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let username = tag("owner");
    let owner = fixture_account(&pool, &username, "testpass").await;
    let (org_id, org) = fixture_organization(&pool, owner).await;
    let inside = tag("inside");
    let outside = tag("outside");
    let inside_id = fixture_repo(&pool, org_id, &inside, owner).await;
    let outside_id = fixture_repo(&pool, org_id, &outside, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    for (id, name) in [(inside_id, inside.as_str()), (outside_id, outside.as_str())] {
        let repo_path = state
            .storage
            .create(
                id,
                Uuid::new_v4(),
                githearth_git::layout::ObjectFormat::Sha1,
            )
            .unwrap();
        let work = std::env::temp_dir().join(format!("githearth-tp-seed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&work).unwrap();
        let w = work.to_str().unwrap();
        git_ok(git_cmd(&["-C", w, "init", "-b", "main"])).await;
        std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
        git_ok(git_cmd(&["-C", w, "add", "README.md"])).await;
        git_ok(git_cmd(&[
            "-C",
            w,
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ]))
        .await;
        seed_bare(repo_path.to_str().unwrap(), w).await;
        let _ = std::fs::remove_dir_all(&work);
        let _ = name;
    }

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let session = login(&app, &username, "testpass").await;

    GitFixture {
        pool,
        app,
        port,
        org,
        inside,
        outside,
        owner,
        session,
        inside_id,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn every_subject_is_confined_on_git_http() {
    let fx = git_fixture().await;

    // Account subject.
    let (_id, account_token, _) = create_scoped(
        &fx.app,
        &fx.session,
        None,
        fx.inside_id,
        &["repo:read"],
        &["git"],
        900,
    )
    .await;
    assert!(
        clone(fx.port, &fx.org, &fx.inside, &account_token)
            .await
            .status
            .success()
    );
    assert!(
        !clone(fx.port, &fx.org, &fx.outside, &account_token)
            .await
            .status
            .success()
    );
    // An API-only token is not a Git credential.
    let (_id, api_only, _) = create_scoped(
        &fx.app,
        &fx.session,
        None,
        fx.inside_id,
        &["repo:read"],
        &["api"],
        900,
    )
    .await;
    assert!(
        !clone(fx.port, &fx.org, &fx.inside, &api_only)
            .await
            .status
            .success()
    );

    // Service account subject: read grant on the inside repository only.
    let service = fixture_service_account(
        &fx.pool,
        fixture_org_id(&fx.pool, fx.inside_id).await,
        fx.owner,
    )
    .await;
    grant_repo(
        &fx.pool,
        fixture_org_id(&fx.pool, fx.inside_id).await,
        fx.inside_id,
        "service_account",
        service,
        "read",
        fx.owner,
    )
    .await;
    let (_id, service_token, _) = create_scoped(
        &fx.app,
        &fx.session,
        Some(json!({"kind": "service_account", "id": service})),
        fx.inside_id,
        &["repo:read"],
        &["git"],
        900,
    )
    .await;
    assert!(
        clone(fx.port, &fx.org, &fx.inside, &service_token)
            .await
            .status
            .success()
    );
    assert!(
        !clone(fx.port, &fx.org, &fx.outside, &service_token)
            .await
            .status
            .success()
    );

    // Run subject, then the run ends.
    let run = fixture_run(
        &fx.pool,
        fixture_org_id(&fx.pool, fx.inside_id).await,
        fx.owner,
    )
    .await;
    let (_id, run_token, _) = create_scoped(
        &fx.app,
        &fx.session,
        Some(json!({"kind": "run", "id": run})),
        fx.inside_id,
        &["repo:read"],
        &["git"],
        900,
    )
    .await;
    assert!(
        clone(fx.port, &fx.org, &fx.inside, &run_token)
            .await
            .status
            .success()
    );
    assert!(
        !clone(fx.port, &fx.org, &fx.outside, &run_token)
            .await
            .status
            .success()
    );
    sqlx::query(
        "UPDATE ci_runs SET status = 'succeeded', ended_at = clock_timestamp() WHERE id = $1",
    )
    .bind(run)
    .execute(&fx.pool)
    .await
    .unwrap();
    assert!(
        !clone(fx.port, &fx.org, &fx.inside, &run_token)
            .await
            .status
            .success()
    );

    // Revoked is immediate on Git too.
    let (id, revoked, _) = create_scoped(
        &fx.app,
        &fx.session,
        None,
        fx.inside_id,
        &["repo:read"],
        &["git"],
        900,
    )
    .await;
    assert!(
        clone(fx.port, &fx.org, &fx.inside, &revoked)
            .await
            .status
            .success()
    );
    assert_eq!(
        request(
            &fx.app,
            "DELETE",
            &format!("/api/v1/user/tokens/{id}"),
            Some(&fx.session),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert!(
        !clone(fx.port, &fx.org, &fx.inside, &revoked)
            .await
            .status
            .success()
    );

    fx.pool.close().await;
}

async fn fixture_org_id(pool: &PgPool, repo: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id = $1")
        .bind(repo)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// SSH: a service account's own key is confined to its grants
// ---------------------------------------------------------------------------

fn has_ssh() -> bool {
    Command::new("ssh").arg("-V").output().is_ok()
}

fn ssh_keygen(path: &Path) {
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fingerprint(path: &Path) -> String {
    let out = Command::new("ssh-keygen")
        .args(["-lf"])
        .arg(path)
        .arg("-E")
        .arg("sha256")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

fn git_ssh_env(key: &Path, port: u16) -> String {
    format!(
        "ssh -i {} -p {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR",
        key.display(),
        port
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn ssh_service_account_key_cannot_exceed_its_grants() {
    if !has_ssh() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }
    let url = fresh_database("ssh").await;
    let storage = temp_storage_root("ssh");
    std::fs::create_dir_all(&storage).unwrap();
    let host_key = storage.join("ssh_host_key");
    githearth_server::ssh::generate_host_key(&host_key).unwrap();
    let key_dir = std::env::temp_dir().join(format!("githearth-tp-ssh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen(&client_key);
    let fp = fingerprint(&client_key);

    let config = Config {
        database_url: url,
        storage_root: storage.clone(),
        ssh_enabled: true,
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let username = tag("owner");
    let owner = fixture_account(&pool, &username, "unused").await;
    let (org_id, org) = fixture_organization(&pool, owner).await;
    let inside = tag("inside");
    let outside = tag("outside");
    let inside_id = fixture_repo(&pool, org_id, &inside, owner).await;
    let outside_id = fixture_repo(&pool, org_id, &outside, owner).await;
    let service = fixture_service_account(&pool, org_id, owner).await;
    grant_repo(
        &pool,
        org_id,
        inside_id,
        "service_account",
        service,
        "read",
        owner,
    )
    .await;
    // The key belongs to the service account, not an account.
    sqlx::query(
        "INSERT INTO ssh_keys (id,subject_kind,service_account_id,name,algorithm,public_key,fingerprint) \
         VALUES ($1,'service_account',$2,'svc key','ssh-ed25519',$3,$4)",
    )
    .bind(Uuid::new_v4())
    .bind(service)
    .bind(std::fs::read_to_string(client_key.with_extension("pub")).unwrap())
    .bind(&fp)
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();
    for (id, name) in [(inside_id, inside.as_str()), (outside_id, outside.as_str())] {
        let repo_path = state
            .storage
            .create(
                id,
                Uuid::new_v4(),
                githearth_git::layout::ObjectFormat::Sha1,
            )
            .unwrap();
        let work = std::env::temp_dir().join(format!("githearth-tp-ssh-seed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&work).unwrap();
        let w = work.to_str().unwrap();
        git_ok(git_cmd(&["-C", w, "init", "-b", "main"])).await;
        std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
        git_ok(git_cmd(&["-C", w, "add", "README.md"])).await;
        git_ok(git_cmd(&[
            "-C",
            w,
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ]))
        .await;
        seed_bare(repo_path.to_str().unwrap(), w).await;
        let _ = std::fs::remove_dir_all(&work);
        let _ = name;
    }

    let authenticate = githearth_server::ssh_keys::authenticator(pool.clone());
    let (_handle, port_rx) = githearth_server::ssh::spawn(state, authenticate);
    let port = port_rx.await.unwrap().port();
    let ssh_env = git_ssh_env(&client_key, port);

    let inside_dir = std::env::temp_dir().join(format!("githearth-tp-ssh-in-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&[
        "clone",
        &format!("ssh://git@127.0.0.1:{port}/{org}/{inside}.git"),
        inside_dir.to_str().unwrap(),
    ]);
    cmd.env("GIT_SSH_COMMAND", &ssh_env);
    git_ok(cmd).await;

    let outside_dir = std::env::temp_dir().join(format!("githearth-tp-ssh-out-{}", Uuid::new_v4()));
    let mut cmd = git_cmd(&[
        "clone",
        &format!("ssh://git@127.0.0.1:{port}/{org}/{outside}.git"),
        outside_dir.to_str().unwrap(),
    ]);
    cmd.env("GIT_SSH_COMMAND", &ssh_env);
    assert!(!git_run(cmd).await.status.success());

    // A read grant is not write authority: a push is refused.
    let mut cmd = git_cmd(&["-C", inside_dir.to_str().unwrap(), "push", "origin", "main"]);
    cmd.env("GIT_SSH_COMMAND", &ssh_env);
    std::fs::write(inside_dir.join("second.txt"), "second\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        inside_dir.to_str().unwrap(),
        "add",
        "second.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        inside_dir.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "second",
    ]))
    .await;
    assert!(!git_run(cmd).await.status.success());

    let _ = std::fs::remove_dir_all(&inside_dir);
    let _ = std::fs::remove_dir_all(&outside_dir);
    let _ = std::fs::remove_dir_all(&key_dir);
    pool.close().await;
}
