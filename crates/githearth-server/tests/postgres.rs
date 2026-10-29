//! Real PostgreSQL contract test; explicitly opt in with --ignored.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_authz::{Action, BuiltinRole, Engine, Resource, Role, Scope, Subject};
use githearth_server::{AppState, MIGRATOR, bootstrap_admin, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Connection, PgPool, Row};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

type Facts = githearth_server::facts::Facts;

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
async fn login(app: &Router, username: &str, password: &str) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await
}
async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A database this test always starts empty: dropped and recreated per run on
/// the same server, so it cannot collide with the other tests' fixture data.
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

/// A unique storage root in the system temp dir so AppState setup never
/// writes under the source tree.
fn temp_storage_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("githearth-postgres-{}", Uuid::new_v4()))
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_foundation_contract() {
    let config = Config {
        database_url: fresh_database("githearth_t1a_foundation").await,
        database_timeout_seconds: 1,
        storage_root: temp_storage_root(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tables, 0, "test refuses a nonempty database");
    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    let app = router(state);
    assert_eq!(
        request(&app, "GET", "/health/live", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    MIGRATOR.run(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    assert_eq!(
        count(&pool, "_sqlx_migrations").await,
        MIGRATOR.iter().count() as i64
    );
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE _sqlx_migrations SET success=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("UPDATE _sqlx_migrations SET success=true")
        .execute(&pool)
        .await
        .unwrap();

    let originals: Vec<(i64, Vec<u8>)> =
        sqlx::query("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("version"),
                    row.get::<Vec<u8>, _>("checksum"),
                )
            })
            .collect();
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1")
        .bind(vec![0u8; 48])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for (version, checksum) in originals {
        sqlx::query("UPDATE _sqlx_migrations SET checksum=$1 WHERE version=$2")
            .bind(checksum)
            .bind(version)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("ALTER TABLE sessions RENAME TO missing_sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("ALTER TABLE missing_sessions RENAME TO sessions")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    sqlx::query("INSERT INTO _sqlx_migrations (version,description,success,checksum,execution_time) VALUES (999,'unknown fixture',true,$1,0)")
        .bind(vec![0u8; 48]).execute(&pool).await.unwrap();
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=999")
        .execute(&pool)
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        bootstrap_admin(&pool, "first-admin", "correct horse battery staple".into()),
        bootstrap_admin(&pool, "first-admin", "correct horse battery staple".into()),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let admin = first.or(second).unwrap();
    assert!(
        bootstrap_admin(&pool, "replacement", "another secure password".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(count(&pool, "accounts").await, 1);
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id=$1")
        .bind(admin)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("correct horse"));
    assert_eq!(
        login(&app, "first-admin", "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&app, "missing-user", "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, token) = login(&app, "first-admin", "correct horse battery staple").await;
    assert_eq!(status, StatusCode::OK);
    let token = token["token"].as_str().unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions WHERE account_id=$1")
        .bind(admin)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token.as_bytes());
    let (status, me) = request(&app, "GET", "/api/v1/me", Some(token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"], admin.to_string());
    assert!(me.get("password_hash").is_none());
    assert_eq!(
        request(&app, "GET", "/api/v1/me", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    for malformed in [
        "",
        "short",
        "not-a-hex-token",
        &"g".repeat(64),
        &"0".repeat(64),
    ] {
        assert_eq!(
            request(&app, "GET", "/api/v1/me", Some(malformed), Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    // Account provisioning is an operator fixture; there is no public registration endpoint.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,'other-user',$2)")
        .bind(other)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let (status, other_token) = login(&app, "other-user", "correct horse battery staple").await;
    assert_eq!(status, StatusCode::OK);
    let other_token = other_token["token"].as_str().unwrap();
    let (status, organization) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(other_token),
        json!({"slug":"private-team","name":"Private Team"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(organization["role"], "owner");
    let path = format!(
        "/api/v1/organizations/{}",
        organization["id"].as_str().unwrap()
    );
    assert_eq!(
        request(&app, "GET", &path, Some(other_token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    // Administrator does not imply tenant membership.
    assert_eq!(
        request(&app, "GET", &path, Some(token), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations",
            Some(token),
            Value::Null
        )
        .await
        .1,
        json!([])
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations?limit=1",
            Some(other_token),
            Value::Null
        )
        .await
        .1
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/organizations?limit=101",
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(other_token),
        json!({"slug":"private-team","name":"Duplicate"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let owner: Uuid = sqlx::query("SELECT account_id FROM memberships WHERE organization_id=$1")
        .bind(
            organization["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("account_id");
    assert_eq!(owner, other);

    for payload in [
        json!({"slug":"","name":"Invalid"}),
        json!({"slug":"UPPERCASE","name":"Invalid"}),
        json!({"slug":"bad/slash","name":"Invalid"}),
        json!({"slug":"-leading","name":"Invalid"}),
        json!({"slug":"trailing-","name":"Invalid"}),
        json!({"slug":"double--hyphen","name":"Invalid"}),
        json!({"slug":"a".repeat(65),"name":"Invalid"}),
        json!({"slug":"valid-slug","name":"   "}),
        json!({"slug":"valid-slug","name":"x".repeat(201)}),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/organizations",
                Some(other_token),
                payload
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(count(&pool, "organizations").await, 1);

    let mut expected = vec![organization["id"].as_str().unwrap().to_owned()];
    for slug in ["cursor-second", "cursor-third"] {
        let (status, org) = request(
            &app,
            "POST",
            "/api/v1/organizations",
            Some(other_token),
            json!({"slug":slug,"name":slug}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        expected.push(org["id"].as_str().unwrap().to_owned());
    }
    let (status, admin_org) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(token),
        json!({"slug":"admin-private","name":"Admin private"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        request(
            &app,
            "GET",
            &format!(
                "/api/v1/organizations/{}",
                admin_org["id"].as_str().unwrap()
            ),
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    expected.sort();
    let mut seen = Vec::new();
    let mut cursor = String::new();
    for _ in 0..=expected.len() {
        let path = format!("/api/v1/organizations?limit=1{cursor}");
        let (status, page) = request(&app, "GET", &path, Some(other_token), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let page = page.as_array().unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        let id = page[0]["id"].as_str().unwrap().to_owned();
        cursor = format!("&after={id}");
        seen.push(id);
    }
    assert_eq!(
        seen, expected,
        "cursor pages must have no omissions, duplicates, or foreign organizations"
    );

    let before = [
        count(&pool, "organizations").await,
        count(&pool, "memberships").await,
        count(&pool, "audit_events").await,
        count(&pool, "outbox_events").await,
    ];
    // Fail the final insert, proving all earlier inserts roll back together.
    sqlx::query("ALTER TABLE outbox_events ADD CONSTRAINT forced_failure CHECK (kind <> 'organization.created') NOT VALID").execute(&pool).await.unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/organizations",
            Some(other_token),
            json!({"slug":"must-rollback","name":"Rollback"})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let after = [
        count(&pool, "organizations").await,
        count(&pool, "memberships").await,
        count(&pool, "audit_events").await,
        count(&pool, "outbox_events").await,
    ];
    assert_eq!(before, after);
    sqlx::query("ALTER TABLE outbox_events DROP CONSTRAINT forced_failure")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/logout",
            Some(other_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(other_token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE sessions SET expires_at=clock_timestamp() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/login",
            None,
            json!({"username":"first-admin","password":"x".repeat(600_000)})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        request(&app, "POST", "/api/v1/register", None, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // Closed pool represents DB dependency outage, without taking down the test runner's service.
    pool.close().await;
    assert_eq!(
        request(&app, "GET", "/health/ready", None, Value::Null)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&app, "GET", "/health/live", None, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}

// --- persisted authorization facts (T1a) -----------------------------------
//
// Tests share one database: every test tags its own rows with a unique
// suffix so the suite passes in any order and in parallel, and rows left
// behind by earlier runs cannot collide.

async fn postgres_facts_pool() -> (PgPool, Config) {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL to a disposable database"),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        storage_root: temp_storage_root(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    (pool, config)
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.code().as_deref() == Some("23514"))
}

async fn fixture_account(pool: &PgPool, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A bearer-equivalent session without the password path. The stored hash
/// must equal the SHA-256 of the token bytes.
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

async fn fixture_membership(pool: &PgPool, organization: Uuid, account: Uuid, role: &str) {
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(account)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
}

/// An organization owned by `owner`: membership and owner grant included.
async fn fixture_organization(pool: &PgPool, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    fixture_membership(pool, id, owner, "owner").await;
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn fixture_team(pool: &PgPool, organization: Uuid, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id,organization_id,parent_id,slug,name) VALUES ($1,$2,$3,$4,'Fixture Team')")
        .bind(id)
        .bind(organization)
        .bind(parent)
        .bind(tag("team"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn fixture_grant(
    pool: &PgPool,
    role: &str,
    subject_kind: &'static str,
    subject: Uuid,
    organization: Uuid,
    granted_by: Uuid,
) {
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,$3,$4,'organization',$5,$6)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(subject_kind)
        .bind(subject)
        .bind(organization)
        .bind(granted_by)
        .execute(pool)
        .await
        .unwrap();
}

async fn fixture_join_team(pool: &PgPool, team: Uuid, account: Uuid) {
    sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
        .bind(team)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
}

async fn facts_snapshot(
    pool: &PgPool,
    account: Uuid,
    organization: Uuid,
) -> Option<githearth_server::facts::Facts> {
    let mut conn = pool.acquire().await.unwrap();
    githearth_server::facts::load(&mut conn, account, organization)
        .await
        .unwrap()
}

fn readable(engine: &Engine, facts: &Facts, action: Action) -> bool {
    engine
        .authorize(&facts.request(action, Resource::Organization, None))
        .is_allowed()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_teams_trigger_contract() {
    let (pool, _) = postgres_facts_pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let organization = fixture_organization(&pool, owner).await;

    // Nesting accepted up to eight levels deep.
    let mut parent: Option<Uuid> = None;
    for level in 1..=8 {
        let id = fixture_team(&pool, organization, parent).await;
        sqlx::query("UPDATE teams SET slug=$1 WHERE id=$2")
            .bind(format!("team-{level}"))
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        parent = Some(id);
    }
    // Depth nine refused.
    let error = sqlx::query("INSERT INTO teams (id,organization_id,parent_id,slug,name) VALUES ($1,$2,$3,$4,'Ninth Team')")
        .bind(Uuid::new_v4())
        .bind(organization)
        .bind(parent)
        .bind(tag("ninth"))
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        violation(&error),
        "team nesting beyond eight levels is refused"
    );
    // A cycle through an UPDATE of parent_id is refused.
    let first = fixture_team(&pool, organization, None).await;
    let second = fixture_team(&pool, organization, Some(first)).await;
    let error = sqlx::query("UPDATE teams SET parent_id=$1 WHERE id=$2")
        .bind(second)
        .bind(first)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&error), "team parent cycle is refused");
    // A parent team from another organization is refused.
    let other_owner = fixture_account(&pool, &tag("other-owner")).await;
    let other_organization = fixture_organization(&pool, other_owner).await;
    let error = sqlx::query("INSERT INTO teams (id,organization_id,parent_id,slug,name) VALUES ($1,$2,$3,$4,'Foreign Team')")
        .bind(Uuid::new_v4())
        .bind(other_organization)
        .bind(parent)
        .bind(tag("foreign"))
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&error), "cross-organization parent is refused");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_team_members_trigger_contract() {
    let (pool, _) = postgres_facts_pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let organization = fixture_organization(&pool, owner).await;
    let member = fixture_account(&pool, &tag("member")).await;
    fixture_membership(&pool, organization, member, "member").await;
    let team = fixture_team(&pool, organization, None).await;
    fixture_join_team(&pool, team, member).await;
    // An account without a membership in the team's organization is refused.
    let outsider = fixture_account(&pool, &tag("outsider")).await;
    let error = sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
        .bind(team)
        .bind(outsider)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        violation(&error),
        "team member without a membership is refused"
    );
    // A membership in another organization does not qualify either.
    let other_owner = fixture_account(&pool, &tag("other-owner")).await;
    let other_organization = fixture_organization(&pool, other_owner).await;
    let other_member = fixture_account(&pool, &tag("other-member")).await;
    fixture_membership(&pool, other_organization, other_member, "member").await;
    let error = sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
        .bind(team)
        .bind(other_member)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        violation(&error),
        "a member of another organization is refused"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_grants_trigger_contract() {
    let (pool, _) = postgres_facts_pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let organization = fixture_organization(&pool, owner).await;
    let member = fixture_account(&pool, &tag("member")).await;
    fixture_membership(&pool, organization, member, "member").await;
    let team = fixture_team(&pool, organization, None).await;

    // Role must be a built-in role.
    let error = sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'mayor','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(member)
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&error), "grant with an unknown role is refused");
    // An account subject must be a member of the scope organization.
    let outsider = fixture_account(&pool, &tag("outsider")).await;
    let error = sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$4)")
        .bind(Uuid::new_v4())
        .bind(outsider)
        .bind(organization)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        violation(&error),
        "grant to a non-member account is refused"
    );
    // A team subject must belong to the scope organization.
    let other_owner = fixture_account(&pool, &tag("owner2")).await;
    let other_organization = fixture_organization(&pool, other_owner).await;
    let other_team = fixture_team(&pool, other_organization, None).await;
    let error = sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','team',$2,'organization',$3,$4)")
        .bind(Uuid::new_v4())
        .bind(other_team)
        .bind(organization)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(violation(&error), "grant to a foreign team is refused");
    // Expiry must be in the future.
    let error = sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by,expires_at) VALUES ($1,'read','team',$2,'organization',$3,$4,clock_timestamp() - interval '1 minute')")
        .bind(Uuid::new_v4())
        .bind(team)
        .bind(organization)
        .bind(member)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        violation(&error),
        "grant expired before creation is refused"
    );
    let grant = Uuid::new_v4();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by,expires_at) VALUES ($1,'read','team',$2,'organization',$3,$4,clock_timestamp() + interval '1 day')")
        .bind(grant)
        .bind(team)
        .bind(organization)
        .bind(member)
        .execute(&pool)
        .await
        .expect("a future expiry is accepted");

    // Grants are granted or revoked, never edited.
    for edit in [
        "UPDATE grants SET role='write' WHERE id=$1",
        "UPDATE grants SET subject_id=$1 WHERE id=$1",
        "UPDATE grants SET scope_id=$1 WHERE id=$1",
        "UPDATE grants SET expires_at=NULL WHERE id=$1",
    ] {
        let error = sqlx::query(edit)
            .bind(grant)
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(violation(&error), "grants are never edited");
    }
    // One revocation accepted, a second refused.
    sqlx::query("UPDATE grants SET revoked_at=clock_timestamp(), revoked_by=$2 WHERE id=$1")
        .bind(grant)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("the first revocation is accepted");
    let error =
        sqlx::query("UPDATE grants SET revoked_at=clock_timestamp(), revoked_by=$2 WHERE id=$1")
            .bind(grant)
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(violation(&error), "a second revocation is refused");
    // Grants are never deleted, only revoked: the delete raises at commit.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM grants WHERE id=$1")
        .bind(grant)
        .execute(&mut *tx)
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(violation(&error), "grant deletes are refused");

    // Deleting the scope organization cascades its grants.
    let doomed = fixture_organization(&pool, member).await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("deleting the organization cascades its grants");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM grants WHERE scope_id=$1")
        .bind(doomed)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "cascaded grants do not survive the organization");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_facts_load_contract() {
    let (pool, _) = postgres_facts_pool().await;
    let engine = Engine::new().unwrap();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let organization = fixture_organization(&pool, owner).await;
    let member = fixture_account(&pool, &tag("member")).await;
    fixture_membership(&pool, organization, member, "member").await;
    let parent = fixture_team(&pool, organization, None).await;
    let child = fixture_team(&pool, organization, Some(parent)).await;
    fixture_join_team(&pool, child, member).await;
    // The grant sits on the parent team the account reaches only through
    // the ancestor chain.
    fixture_grant(&pool, "member", "team", parent, organization, member).await;

    let facts = facts_snapshot(&pool, member, organization)
        .await
        .expect("the organization exists");
    let loaded: std::collections::HashSet<Uuid> = facts.teams.iter().map(|t| t.id).collect();
    assert_eq!(loaded, std::collections::HashSet::from([parent, child]));
    assert_eq!(facts.account.teams, vec![child]);
    assert_eq!(facts.account.organizations, vec![organization]);
    assert!(
        facts
            .grants
            .iter()
            .any(|g| g.role == Role::Builtin(BuiltinRole::Member)
                && g.subject == Subject::Team(parent)
                && g.scope == Scope::Organization(organization))
    );
    // A grant through a nested ancestor team applies.
    assert!(readable(&engine, &facts, Action::OrganizationRead));

    // An expired grant leaves the facts without any edit.
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by,expires_at) VALUES ($1,'triage','account',$2,'organization',$3,$2,clock_timestamp() + interval '80 milliseconds')")
        .bind(Uuid::new_v4())
        .bind(member)
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let facts = facts_snapshot(&pool, member, organization).await.unwrap();
    assert!(
        !facts
            .grants
            .iter()
            .any(|g| g.subject == Subject::Account(member)
                && g.role == Role::Builtin(BuiltinRole::Triage))
    );
    assert!(readable(&engine, &facts, Action::OrganizationRead));

    // A revoked grant leaves the facts too.
    let revoked = Uuid::new_v4();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'admin','account',$2,'organization',$3,$2)")
        .bind(revoked)
        .bind(member)
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE grants SET revoked_at=clock_timestamp(), revoked_by=$2 WHERE id=$1")
        .bind(revoked)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let facts = facts_snapshot(&pool, member, organization).await.unwrap();
    assert!(
        !facts
            .grants
            .iter()
            .any(|g| g.subject == Subject::Account(member)
                && g.role == Role::Builtin(BuiltinRole::Admin))
    );
    assert!(readable(&engine, &facts, Action::OrganizationRead));

    // An account that is not a member carries no usable facts.
    let outsider = fixture_account(&pool, &tag("outsider")).await;
    let facts = facts_snapshot(&pool, outsider, organization).await.unwrap();
    assert!(facts.account.organizations.is_empty());
    assert!(facts.teams.is_empty());
    assert!(!readable(&engine, &facts, Action::OrganizationRead));

    // A missing organization is None.
    assert!(
        facts_snapshot(&pool, member, Uuid::new_v4())
            .await
            .is_none()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_read_organization_fact_contract() {
    let (pool, config) = postgres_facts_pool().await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    let owner = fixture_account(&pool, &tag("owner")).await;
    let token = fixture_session(&pool, owner).await;
    let slug = tag("created");
    let (status, organization) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": slug, "name": "Facts Organization"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let path = format!(
        "/api/v1/organizations/{}",
        organization["id"].as_str().unwrap()
    );
    let (status, body) = request(&app, "GET", &path, Some(&token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], "owner");
    // A suspended organization reads as not found and drops out of the listing.
    sqlx::query("UPDATE organizations SET suspended=true WHERE slug=$1")
        .bind(&slug)
        .execute(&pool)
        .await
        .unwrap();
    let (status, listed) = request(
        &app,
        "GET",
        "/api/v1/organizations",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["slug"] == slug.as_str())
    );
    assert_eq!(
        request(&app, "GET", &path, Some(&token), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE organizations SET suspended=false WHERE slug=$1")
        .bind(&slug)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    // A suspended account's sessions stop working, whatever its grants.
    sqlx::query("UPDATE accounts SET suspended=true WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &path, Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // A non-member reads as not found.
    let stranger = fixture_account(&pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&pool, stranger).await;
    assert_eq!(
        request(&app, "GET", &path, Some(&stranger_token), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_create_organization_backfills_grants() {
    let (pool, config) = postgres_facts_pool().await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    let owner = fixture_account(&pool, &tag("owner")).await;
    let token = fixture_session(&pool, owner).await;
    let (status, organization) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": tag("backfill"), "name": "Granted Organization"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id: Uuid = organization["id"].as_str().unwrap().parse().unwrap();
    // Every membership row ends up backed by a live grant of the same role.
    let missing: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships m LEFT JOIN grants g \
         ON g.subject_kind='account' AND g.subject_id=m.account_id \
         AND g.scope_id=m.organization_id AND g.role=m.role AND g.revoked_at IS NULL \
         WHERE m.organization_id=$1 AND g.id IS NULL",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(missing, 0);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM authz_events WHERE organization_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(events > 0, "the created grant joined the change feed");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn postgres_authz_events_commit_order_contract() {
    let (pool, _) = postgres_facts_pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let organization = fixture_organization(&pool, owner).await;
    let mut conn_a = pool.acquire().await.unwrap();
    let mut conn_b = pool.acquire().await.unwrap();
    let mut tx_a = conn_a.begin().await.unwrap();
    let mut tx_b = conn_b.begin().await.unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    // A inserts first but commits last: sequence numbers follow commit order,
    // never insertion order.
    sqlx::query(
        "INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Commit First')",
    )
    .bind(first)
    .bind(organization)
    .bind(tag("commit-a"))
    .execute(&mut *tx_a)
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    sqlx::query(
        "INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Commit Second')",
    )
    .bind(second)
    .bind(organization)
    .bind(tag("commit-b"))
    .execute(&mut *tx_b)
    .await
    .unwrap();
    tx_b.commit().await.unwrap();
    tx_a.commit().await.unwrap();
    drop(conn_a);
    drop(conn_b);
    let rows: Vec<(Uuid, Option<i64>)> = sqlx::query_as(
        "SELECT subject_id, seq FROM authz_events WHERE kind='team' AND (subject_id=$1 OR subject_id=$2)",
    )
    .bind(first)
    .bind(second)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    let seq = |id: Uuid| {
        rows.iter()
            .find(|row| row.0 == id)
            .unwrap()
            .1
            .expect("committed events carry a sequence number")
    };
    let first_seq = seq(first);
    let second_seq = seq(second);
    assert!(
        second_seq < first_seq,
        "sequence numbers follow commit order: {first_seq} must exceed {second_seq}"
    );
    // A committed reader never sees an event without its sequence number.
    let nulls: i64 = sqlx::query_scalar("SELECT count(*) FROM authz_events WHERE seq IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(nulls, 0);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn suspended_account_cannot_sign_in_or_create_organizations() {
    let config = Config {
        database_url: fresh_database("githearth_suspended_login").await,
        database_timeout_seconds: 1,
        storage_root: temp_storage_root(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    let password = "correct horse battery staple";
    let admin = bootstrap_admin(&pool, "suspended-admin", password.into())
        .await
        .unwrap();
    let (status, token) = login(&app, "suspended-admin", password).await;
    assert_eq!(status, StatusCode::OK);
    let token = token["token"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE accounts SET suspended=true WHERE id=$1")
        .bind(admin)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        login(&app, "suspended-admin", password).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": "spam", "name": "Spam"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count(&pool, "organizations").await, 0);
    pool.close().await;
}
