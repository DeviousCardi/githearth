//! Tamper-evident audit log contract tests. Opt in with --ignored.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Connection, PgPool};
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

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

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn fixture_account(pool: &PgPool, username: &str, is_admin: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',$3) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(is_admin)
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

async fn fixture_org(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
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

async fn fixture_membership(pool: &PgPool, org: Uuid, account: Uuid, role: &str) {
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,$3)")
        .bind(org)
        .bind(account)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
}

async fn insert_event(pool: &PgPool, org: Option<Uuid>, actor: Uuid, kind: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_events (id,organization_id,actor_id,kind) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(org)
        .bind(actor)
        .bind(kind)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn setup() -> (Router, PgPool, PathBuf, Uuid, String, Uuid) {
    let storage_root = std::env::temp_dir().join(format!("githearth-audit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 32,
        database_timeout_seconds: 5,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("owner"), false).await;
    let (org_id, org_slug) = fixture_org(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, storage_root, owner, org_slug, org_id)
}

fn cleanup(storage_root: &PathBuf) {
    let _ = std::fs::remove_dir_all(storage_root);
}

async fn verify_chain(pool: &PgPool, chain: Option<Uuid>) -> (u64, bool, Option<i64>) {
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    let v = githearth_server::audit::verify(&mut tx, chain)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (v.rows, v.ok, v.first_bad_seq)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn chain_starts_with_zero_prev_and_links_sequentially() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;
    let mut events = Vec::new();
    for i in 0..5 {
        events.push(insert_event(&pool, Some(org_id), owner, &format!("kind.{i}")).await);
    }
    // First row's prev_hash is 32 zero bytes and seq is 1.
    let first: (i64, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT seq, prev_hash, hash FROM audit_events WHERE organization_id=$1 ORDER BY seq LIMIT 1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first.0, 1);
    assert_eq!(first.1, vec![0u8; 32]);
    // Every row's prev_hash equals the previous row's hash.
    let rows: Vec<(i64, Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT seq, prev_hash, hash FROM audit_events WHERE organization_id=$1 ORDER BY seq",
    )
    .bind(org_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 5);
    for w in rows.windows(2) {
        assert_eq!(w[1].0, w[0].0 + 1, "seq gap");
        assert_eq!(w[1].1, w[0].2, "prev_hash must equal previous hash");
    }
    let (rows_n, ok, first_bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows_n, ok, first_bad), (5, true, None));
    assert_eq!(events.len(), 5);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_writers_produce_gap_free_valid_chain() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;
    let mut handles = Vec::new();
    for i in 0..20 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "INSERT INTO audit_events (id,organization_id,actor_id,kind) VALUES ($1,$2,$3,$4)",
            )
            .bind(Uuid::new_v4())
            .bind(org_id)
            .bind(owner)
            .bind(format!("concurrent.{i}"))
            .execute(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM audit_events WHERE organization_id=$1 ORDER BY seq")
            .bind(org_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        seqs,
        (1..=20).collect::<Vec<_>>(),
        "gap-free strictly increasing seq"
    );
    let (rows_n, ok, first_bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows_n, ok, first_bad), (20, true, None));
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_organizations_have_independent_chains() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;
    let other = fixture_org(&pool, owner).await.0;
    insert_event(&pool, Some(org_id), owner, "a").await;
    insert_event(&pool, Some(org_id), owner, "b").await;
    insert_event(&pool, Some(other), owner, "a").await;
    // Each organization starts its own chain at seq 1.
    let first: i64 = sqlx::query_scalar(
        "SELECT seq FROM audit_events WHERE organization_id=$1 ORDER BY seq LIMIT 1",
    )
    .bind(other)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first, 1);
    let (_, ok1, _) = verify_chain(&pool, Some(org_id)).await;
    let (_, ok2, _) = verify_chain(&pool, Some(other)).await;
    assert!(ok1 && ok2);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn update_delete_truncate_are_refused() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;
    let id = insert_event(&pool, Some(org_id), owner, "doomed").await;
    let err = sqlx::query("UPDATE audit_events SET kind='edited' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&err), "UPDATE is refused");
    let err = sqlx::query("DELETE FROM audit_events WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&err), "DELETE is refused");
    // The outbox FK would refuse a bare TRUNCATE; CASCADE lets the TRUNCATE
    // trigger be the thing that raises 23514.
    let err = sqlx::query("TRUNCATE audit_events CASCADE")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&err), "TRUNCATE is refused");
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tampering_is_detected_with_first_bad_seq() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;
    for i in 0..5 {
        insert_event(&pool, Some(org_id), owner, &format!("tamper.{i}")).await;
    }
    let (rows_n, ok, _bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows_n, ok), (5, true));
    // Tamper with seq 3 as superuser, bypassing the append-only UPDATE trigger.
    // SET LOCAL session_replication_role=replica skips triggers only inside this
    // transaction, so no other test (or pooled connection) ever sees them
    // disabled. The ALTER TABLE ... DISABLE TRIGGER it replaces is table-wide
    // and races concurrent tests sharing the database.
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET hash = decode(repeat('11',32),'hex') WHERE organization_id=$1 AND seq=3")
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(conn);
    let (rows_n, ok, first_bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!(rows_n, 5);
    assert!(!ok);
    assert_eq!(first_bad, Some(3));
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn backfilled_rows_verify() {
    // Recreate the pre-migration state: apply migrations 0001..0006, insert
    // chain-agnostic rows, then apply 0007 which backfills them.
    let url = fresh_database("githearth_audit_backfill").await;
    let config = Config {
        database_url: url,
        database_timeout_seconds: 5,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    let migrations: Vec<_> = MIGRATOR.iter().collect();
    for m in migrations.iter().filter(|m| m.version < 7) {
        sqlx::raw_sql(&m.sql).execute(&pool).await.unwrap();
    }
    let owner = fixture_account(&pool, &tag("owner"), false).await;
    let (org_id, _slug) = fixture_org(&pool, owner).await;
    // Pre-0007 rows carry no chain columns.
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO audit_events (id,organization_id,actor_id,kind) VALUES ($1,$2,$3,$4)",
        )
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(owner)
        .bind(format!("pre.{i}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO audit_events (id,organization_id,actor_id,kind) VALUES ($1,NULL,$2,'site.event')")
        .bind(Uuid::new_v4())
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    // Apply migration 0007 (adds columns, trigger, backfill).
    let m7 = migrations.iter().find(|m| m.version == 7).unwrap();
    sqlx::raw_sql(&m7.sql).execute(&pool).await.unwrap();
    // Apply 0019 (adds client_ip and the v2 trigger) so the schema matches what
    // verify() expects; v2 hashes byte-identically to 0007 when client_ip is
    // NULL, so these backfilled rows keep their 0007-computed hashes.
    let m19 = migrations.iter().find(|m| m.version == 19).unwrap();
    sqlx::raw_sql(&m19.sql).execute(&pool).await.unwrap();
    // Apply 0022 (adds the IP key/digest columns and the v3 trigger) so the
    // schema matches what verify() selects; the backfilled rows have no digest,
    // so they keep their 0019-computed hashes.
    let m22 = migrations.iter().find(|m| m.version == 22).unwrap();
    sqlx::raw_sql(&m22.sql).execute(&pool).await.unwrap();
    let (org_rows, org_ok, bad1) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!(
        (org_rows, org_ok, bad1),
        (3, true, None),
        "backfilled org rows verify"
    );
    let (site_rows, site_ok, bad2) = verify_chain(&pool, None).await;
    assert_eq!(
        (site_rows, site_ok, bad2),
        (1, true, None),
        "backfilled site row verifies"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn api_paging_and_permission_errors() {
    let (app, pool, storage_root, owner, slug, org_id) = setup().await;
    let member = fixture_account(&pool, &tag("member"), false).await;
    fixture_membership(&pool, org_id, member, "member").await;
    let stranger = fixture_account(&pool, &tag("stranger"), false).await;

    for i in 0..25 {
        insert_event(&pool, Some(org_id), owner, &format!("event.{i}")).await;
    }
    let owner_token = fixture_session(&pool, owner).await;

    // A NUL in the organization path is invalid input, not a database error.
    for path in [
        "/api/v1/organizations/%00/audit",
        "/api/v1/organizations/%00/audit/verify",
    ] {
        let (status, _) = request(&app, "GET", path, Some(&owner_token), Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }

    // Paging: newest last, default limit, next cursor.
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit"),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 25);
    // Only 25 events, so no next cursor.
    assert!(body["next"].is_null());
    // Items are newest-last.
    let first_seq = items.first().unwrap()["seq"].as_i64().unwrap();
    let last_seq = items.last().unwrap()["seq"].as_i64().unwrap();
    assert!(first_seq < last_seq);
    assert!(items[0]["hash"].is_string());

    // limit=3 pages through everything, next points at the last item's seq.
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit?limit=3"),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
    let next = body["next"].as_i64().unwrap();
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit?limit=200&after={next}"),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 22);

    // Bad limit.
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit?limit=201"),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // verify endpoint for owner.
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit/verify"),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["rows"], 25);
    assert!(body["first_bad_seq"].is_null());

    // member can read the org but not administer: 403.
    let member_token = fixture_session(&pool, member).await;
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit"),
        Some(&member_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit/verify"),
        Some(&member_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // non-member never learns existence: 404.
    let stranger_token = fixture_session(&pool, stranger).await;
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit"),
        Some(&stranger_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // unknown organization: 404.
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{}-nope/audit", tag("missing")),
        Some(&owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // unauthenticated: 401.
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn verify_endpoint_never_false_alarms_under_concurrent_writes() {
    let (app, pool, storage_root, owner, slug, org_id) = setup().await;
    let token = fixture_session(&pool, owner).await;

    // Writers stream new events while the verify endpoint runs repeatedly. The
    // endpoint must always report ok: it reads the head first and bounds the
    // stream by it at REPEATABLE READ, so a concurrent writer cannot make a
    // freshly-committed row look like a tamper (the chain row lock serializes
    // writers, so rows always appear gap-free committed in seq order).
    let mut writers = Vec::new();
    for i in 0..8 {
        let pool = pool.clone();
        writers.push(tokio::spawn(async move {
            for j in 0..25 {
                insert_event(&pool, Some(org_id), owner, &format!("race.{i}.{j}")).await;
            }
        }));
    }

    for _ in 0..20 {
        let (status, body) = request(
            &app,
            "GET",
            &format!("/api/v1/organizations/{slug}/audit/verify"),
            Some(&token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], json!(true));
        assert!(body["first_bad_seq"].is_null());
    }

    for h in writers {
        h.await.unwrap();
    }
    // After all writers finish, the chain is complete and still verifies.
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{slug}/audit/verify"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["rows"], json!(200));
    assert!(body["first_bad_seq"].is_null());

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn verify_first_insert_never_false_alarms() {
    let (_, pool, storage_root, owner, _slug, org_id) = setup().await;

    // An organization with no events yet has no audit_chain head. While the
    // first event is being inserted, verify (at READ COMMITTED) must keep
    // reporting ok: the empty-chain bound (seq <= 0) excludes the in-flight
    // first row instead of streaming it against a None head.
    let pool2 = pool.clone();
    let writer = tokio::spawn(async move {
        for j in 0..10 {
            insert_event(&pool2, Some(org_id), owner, &format!("first.{j}")).await;
        }
    });

    for _ in 0..50 {
        let (_, ok, first_bad) = verify_chain(&pool, Some(org_id)).await;
        assert!(
            ok,
            "verify must not report tampering while the first event lands: {first_bad:?}"
        );
    }

    writer.await.unwrap();
    // The chain is complete and still verifies.
    let (rows_n, ok, first_bad) = verify_chain(&pool, Some(org_id)).await;
    assert_eq!((rows_n, ok, first_bad), (10, true, None));

    cleanup(&storage_root);
    pool.close().await;
}

fn violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.code().as_deref() == Some("23514"))
}
/// A database this test always starts empty, dropped and recreated per run.
async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL to a disposable database");
    let (origin, _) = url
        .rsplit_once('/')
        .expect("URL must end in a database name");
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
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
