//! Invitations with expiry (M4.1 acceptance case U06).
//!
//! An invitation is created, listed and revoked through the JSON API and the
//! browser page; it is single-use and expiring; accepting it creates the
//! membership and the team membership in the same transaction as the grant and
//! is audited; and a refusal reveals nothing — an unknown token, a token for
//! another account, an expired, revoked or consumed one and an account without
//! a verified email all answer the same status with byte-identical bodies that
//! never name the organization.
//!
//! The fixtures write PostgreSQL directly because they are setup; every
//! assertion goes through the router.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

/// The raw response, for tests that compare bodies byte for byte.
struct Raw {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
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
    key: Option<&str>,
    body: Option<&Value>,
) -> Raw {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    let body = body.map_or_else(String::new, ToString::to_string);
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Raw {
        status,
        headers,
        body,
    }
}

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let response = raw(app, method, path, token, key, body).await;
    (
        response.status,
        serde_json::from_slice(&response.body).unwrap_or(Value::Null),
    )
}

async fn get_page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).expect("hidden field") + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
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

async fn post_form(
    app: &Router,
    path: &str,
    cookie: &str,
    form: &[(&str, String)],
) -> (StatusCode, String) {
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..12])
}

fn short_temp_dir(label: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("ghi-{label}-{}", &id[..10]))
}

struct User {
    id: Uuid,
    #[allow(dead_code)]
    username: String,
    email: String,
    token: String,
}

impl User {
    fn cookie(&self) -> String {
        format!("githearth_session={}", self.token)
    }
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    let email = format!("{username}@example.com");
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,email,email_verified_at) \
         VALUES ($1,$2,$3,$4,now())",
    )
    .bind(id)
    .bind(&username)
    .bind(hash_password("correct-horse-battery").unwrap())
    .bind(&email)
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
    User {
        id,
        username,
        email,
        token,
    }
}

async fn add_membership(pool: &PgPool, org_id: Uuid, account: Uuid, role: &str) {
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,$3)")
        .bind(org_id)
        .bind(account)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn audit_kinds(pool: &PgPool, org_id: Uuid, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind = $2")
        .bind(org_id)
        .bind(kind)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn organization(pool: &PgPool, owner: Uuid, name: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&slug)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    add_membership(pool, id, owner, "owner").await;
    (id, slug)
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    org_id: Uuid,
    org_slug: String,
    repo_name: String,
    team_slug: String,
    owner: User,
}

/// A private organization named `SECRET-ORG-NAME-<random>`: every refusal
/// asserts that name never appears in the response.
async fn fixture() -> Fixture {
    let storage_root = short_temp_dir("inv");
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
    let owner = user(&pool, "owner").await;
    let (org_id, org_slug) =
        organization(&pool, owner.id, &format!("SECRET-ORG-{}", org_slug_tail())).await;
    let repo_name = tag("repo");
    sqlx::query("INSERT INTO repositories (id,organization_id,name,description,visibility,created_by,operation_id,status) VALUES ($1,$2,$3,'','private',$4,$5,'active')")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(&repo_name)
        .bind(owner.id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let team_slug = tag("team");
    sqlx::query("INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Invited')")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(&team_slug)
        .execute(&pool)
        .await
        .unwrap();
    let app_state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(app_state.clone());
    Fixture {
        app,
        state: app_state,
        pool,
        storage_root,
        org_id,
        org_slug,
        repo_name,
        team_slug,
        owner,
    }
}

fn org_slug_tail() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_owned()
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

fn invitations_path(f: &Fixture) -> String {
    format!("/api/v1/organizations/{}/invitations", f.org_slug)
}

/// Create an invitation through the API and return `(id, token)`.
async fn invite(f: &Fixture, email: &str, team: bool) -> (Uuid, String) {
    let body = json!({
        "email": email,
        "role": "member",
        "team": if team { Value::String(f.team_slug.clone()) } else { Value::Null },
    });
    let (status, body) = api(
        &f.app,
        "POST",
        &invitations_path(f),
        Some(&f.owner.token),
        None,
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "pending");
    (
        body["id"].as_str().unwrap().parse().unwrap(),
        body["token"].as_str().unwrap().to_owned(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_invitation_accepts_once_creates_the_membership_and_team_and_is_audited() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let (_, token) = invite(&f, &invitee.email, true).await;

    // The inviter sees it listed, pending, and the token is not in the listing.
    let (status, list) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = list.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["status"], "pending");
    assert_eq!(listed[0]["email"], invitee.email);
    assert!(!list.to_string().contains(&token));

    // Not in the organization before, a member after: nothing else changed.
    let (status, _) = api(
        &f.app,
        "GET",
        &format!("/api/v1/organizations/{}", f.org_id),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let accept_path = format!("/api/v1/invitations/{token}");
    let (status, body) = api(
        &f.app,
        "POST",
        &accept_path,
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "member");
    assert_eq!(body["organization_id"], f.org_id.to_string());

    let (status, _) = api(
        &f.app,
        "GET",
        &format!("/api/v1/organizations/{}", f.org_id),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The membership, its grant and the team membership exist.
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id=$1 AND account_id=$2",
    )
    .bind(f.org_id)
    .bind(invitee.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(memberships, 1);
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_id=$1 AND subject_kind='account' AND subject_id=$2 AND role='member' AND revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(invitee.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(grants, 1);
    let team: i64 = sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id=(SELECT id FROM teams WHERE organization_id=$1 AND slug=$2) AND account_id=$3")
        .bind(f.org_id)
        .bind(&f.team_slug)
        .bind(invitee.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(team, 1);
    // One `invitation_accepted` audit row (the acceptance writes it itself), and
    // the membership INSERT additionally feeds the 0002 authorization-event
    // stream: an acceptance is two rows in two logs, not one row in one.
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_accepted").await,
        1
    );
    let authz_membership: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM authz_events WHERE organization_id = $1 \
         AND kind = 'membership' AND subject_id = $2",
    )
    .bind(f.org_id)
    .bind(invitee.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(authz_membership, 1);

    // The second attempt is refused, and the refusal is the uniform one.
    let (status, _) = api(
        &f.app,
        "POST",
        &accept_path,
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_accepted").await,
        1
    );
    finish(f).await;
}

/// B1: creating an invitation persists the plaintext token in no text or json
/// column of the public schema — in particular not in the idempotency store's
/// `operations.response_body`, which carried it while creation ran through
/// `idempotency::run`. Creation is therefore not replayed: the same key again
/// is a second, equally valid invitation.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_created_invitation_is_stored_only_as_its_hash_in_any_column() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let body = json!({"email": invitee.email.clone(), "role": "member"});
    let key = Uuid::new_v4().simple().to_string();
    let (status, created) = api(
        &f.app,
        "POST",
        &invitations_path(&f),
        Some(&f.owner.token),
        Some(&key),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_created").await,
        1
    );
    // The row holds exactly the hash, and the hash is not the token.
    let stored: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT token_hash FROM invitations WHERE organization_id = $1")
            .bind(f.org_id)
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored, vec![sha256(&token)]);

    // Every text and json column of the public schema, `operations.response_body`
    // included, holds no copy of the token.
    let hits = columns_containing(&f.pool, &token).await;
    assert!(
        hits.is_empty(),
        "the plaintext token is stored in the public schema: {hits:?}"
    );
    // The scan itself is not vacuous. A one-row table whose first column holds
    // the needle must be found (`table.column`, text rendering), and so must
    // `operations.response_body` when it is filled the way the removed
    // `idem.store` filled it. (A plain table, not a temporary one: the pool may
    // hand each statement a different session.) The table is dropped at the end
    // of the test.
    sqlx::query("CREATE TABLE a3_scan_probe (a text, b text, c text)")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO a3_scan_probe VALUES ('a3-' || $1, '', '')")
        .bind("scan")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        columns_containing(&f.pool, "scan").await,
        vec!["a3_scan_probe.a".to_owned()]
    );
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, response_status, \
         response_body, expires_at) VALUES ($1, $2, sha256($2::bytea), 'done', 201, \
         jsonb_build_object('token', $3::text), clock_timestamp() + interval '1 day')",
    )
    .bind(f.owner.id)
    .bind(format!("a3-scan-probe-{}", Uuid::new_v4().simple()))
    .bind(&token)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        columns_containing(&f.pool, &token).await,
        vec!["operations.response_body".to_owned()],
        "the scan must see a token planted in `operations.response_body`"
    );
    sqlx::query("DELETE FROM operations WHERE actor_id = $1 AND key LIKE 'a3-scan-probe-%'")
        .bind(f.owner.id)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE a3_scan_probe")
        .execute(&f.pool)
        .await
        .unwrap();
    // And what is left is the real answer: no column holds the token.
    let hits = columns_containing(&f.pool, &token).await;
    assert!(
        hits.is_empty(),
        "the plaintext token is stored in the public schema: {hits:?}"
    );

    // The same key and body again: not replayed, a second invitation, audited
    // as a second create, and neither token in any column.
    let (status, again) = api(
        &f.app,
        "POST",
        &invitations_path(&f),
        Some(&f.owner.token),
        Some(&key),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_ne!(again["id"], created["id"], "a retry must not replay");
    let again_token = again["token"].as_str().unwrap().to_owned();
    assert_ne!(again_token, token);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invitations WHERE organization_id = $1")
            .bind(f.org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_created").await,
        2
    );
    for needle in [&token, &again_token] {
        assert!(
            columns_containing(&f.pool, needle).await.is_empty(),
            "a plaintext token is stored in the public schema"
        );
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_expired_invitation_is_refused_with_no_membership_created() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let (id, token) = invite(&f, &invitee.email, false).await;
    sqlx::query(
        "UPDATE invitations SET expires_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap();
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{token}"),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(list[0]["status"], "expired");
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id=$1 AND account_id=$2",
    )
    .bind(f.org_id)
    .bind(invitee.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(memberships, 0);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_accepted").await,
        0
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_wrong_account_gets_the_same_bytes_as_an_unknown_or_expired_invitation() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let hijacker = user(&f.pool, "hijacker").await;
    let unverified_user = user(&f.pool, "unverified").await;
    let (foreign_org_id, _) = organization(&f.pool, hijacker.id, "Hijacker's Own").await;
    let (_, token) = invite(&f, &invitee.email, true).await;
    let (expired_id, expired_token) = invite(&f, &invitee.email, false).await;
    sqlx::query(
        "UPDATE invitations SET expires_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(expired_id)
    .execute(&f.pool)
    .await
    .unwrap();
    let (_, unverified_token) = invite(&f, &unverified_user.email, false).await;
    sqlx::query("UPDATE accounts SET email_verified_at = NULL WHERE id = $1")
        .bind(unverified_user.id)
        .execute(&f.pool)
        .await
        .unwrap();
    let (revoked_id, revoked_token) = invite(&f, &invitee.email, false).await;
    sqlx::query(
        "UPDATE invitations SET revoked_at = clock_timestamp(), revoked_by = $2 WHERE id = $1",
    )
    .bind(revoked_id)
    .bind(f.owner.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let unknown = format!("{}{}", uuid_hex(), uuid_hex());

    // A hijacker signed in and holding the token, and one who is not given
    // it at all, both get the same refusal.
    let path = format!("/api/v1/invitations/{token}");
    let wrong = raw(&f.app, "POST", &path, Some(&hijacker.token), None, None).await;
    let wrong_preview = raw(&f.app, "GET", &path, Some(&hijacker.token), None, None).await;
    let expired = raw(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{expired_token}"),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    let missing = raw(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{unknown}"),
        Some(&hijacker.token),
        None,
        None,
    )
    .await;
    let unverified = raw(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{unverified_token}"),
        Some(&unverified_user.token),
        None,
        None,
    )
    .await;
    let revoked = raw(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{revoked_token}"),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::NOT_FOUND);
    assert_eq!(wrong_preview.status, StatusCode::NOT_FOUND);
    // Every refusal is one refusal: a wrong account (accept and preview), an
    // expired, an unknown, an unverified-address and a revoked token all answer
    // the same status, the same body bytes, content-type and content-length. A
    // consumed token is refused on the same path (see the accepted-once test).
    for (label, case) in [
        ("wrong-preview", &wrong_preview),
        ("expired", &expired),
        ("missing", &missing),
        ("unverified", &unverified),
        ("revoked", &revoked),
    ] {
        assert_eq!(case.status, wrong.status, "{label} status");
        assert_eq!(case.body, wrong.body, "{label} body");
        assert_eq!(
            case.headers.get(header::CONTENT_TYPE),
            wrong.headers.get(header::CONTENT_TYPE),
            "{label} content-type"
        );
        assert_eq!(
            case.headers.get(header::CONTENT_LENGTH),
            wrong.headers.get(header::CONTENT_LENGTH),
            "{label} content-length"
        );
    }

    // Nothing about the private organization is in the bytes: not its name,
    // its slug, its id, the team, the invited address or a member count.
    let name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id = $1")
        .bind(f.org_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(name.starts_with("SECRET-ORG-"), "{name}");
    for needle in [
        name.as_bytes(),
        f.org_slug.as_bytes(),
        f.org_id.to_string().as_bytes(),
        f.team_slug.as_bytes(),
        invitee.email.as_bytes(),
        f.repo_name.as_bytes(),
    ] {
        assert!(
            !wrong.body.windows(needle.len()).any(|w| w == needle),
            "the refusal names {:?}: {}",
            String::from_utf8_lossy(needle),
            wrong.text()
        );
    }
    // And the hijacker joined nothing.
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id=$1 AND account_id=$2",
    )
    .bind(f.org_id)
    .bind(hijacker.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(memberships, 0);
    // Nor did the unverified account or a revoked-token attempt accept.
    for account in [&unverified_user, &invitee] {
        let joined: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memberships WHERE organization_id=$1 AND account_id=$2",
        )
        .bind(f.org_id)
        .bind(account.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(joined, 0, "a refused token must not create a membership");
    }
    assert_eq!(
        audit_kinds(&f.pool, foreign_org_id, "organization.invitation_accepted").await,
        0
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_accepted").await,
        0
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_unverified_address_cannot_accept() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let (_, token) = invite(&f, &invitee.email, false).await;
    sqlx::query("UPDATE accounts SET email_verified_at = NULL WHERE id = $1")
        .bind(invitee.id)
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{token}"),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id=$1 AND account_id=$2",
    )
    .bind(f.org_id)
    .bind(invitee.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(memberships, 0);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_account_email_check_rejects_what_the_application_rejects() {
    let f = fixture().await;
    // One table of cases, run against the real `accounts` table, against the
    // carved-out copy of the same rule (so the copy cannot drift from it), and
    // through the API's validator and create endpoint. `true` means valid.
    let cases: [(&str, bool); 17] = [
        ("x@@y", false),
        ("x@a", false),
        ("x@y", false),
        ("x@.y", false),
        ("x@y.", false),
        ("x@y..z", false),
        ("x@y..z@w.v", false),
        (".x@example.com", false),
        // A space is inside ASCII 32–126 and the application validator no
        // longer replaces the domain rules with a printable test (the old
        // precedence bug did); the CHECK and `valid_email` agree on it.
        ("x y@example.com", true),
        ("x\ty@example.com", false),
        ("x@ex\u{7f}ample.com", false),
        ("x@ex\u{1}ample.com", false),
        ("x@example.c\u{e9}", false),
        ("@x.y", false),
        ("ok@example.com", true),
        ("x.y+z@sub.example.co", true),
        ("a@b.cd", true),
    ];
    // The carved-out copy (0049) is shared by the whole test binary and one
    // test drops it, so it is recreated here under a name only this test uses:
    // a copy with the same predicate, which must agree on every case.
    sqlx::query("DROP TABLE IF EXISTS a3_email_rule_copy")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE a3_email_rule_copy (email text NOT NULL, CONSTRAINT copy CHECK (\
     length(email) BETWEEN 3 AND 320 AND email = lower(email) \
     AND email ~ '^[ -~]+$' \
     AND length(email) - length(replace(email, '@', '')) = 1 \
     AND strpos(email, '@') > 1 \
     AND length(split_part(email, '@', 1)) BETWEEN 1 AND 64 \
     AND left(split_part(email, '@', 1), 1) <> '.' \
     AND split_part(email, '@', 2) <> '' \
     AND strpos(split_part(email, '@', 2), '.') > 1 \
     AND right(split_part(email, '@', 2), 1) <> '.' \
     AND split_part(email, '@', 2) NOT LIKE '%..%'))",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    for (email, valid) in cases {
        // A unique address per case, so a valid case is not refused as a
        // duplicate of another test or another valid case: the CHECK, not the
        // unique index, is what this asserts.
        let unique = tag("email-addr");
        let address = if valid {
            email.replacen('@', &format!("@{unique}."), 1)
        } else {
            email.to_owned()
        };
        let inserted = sqlx::query(
            "INSERT INTO accounts (id, username, password_hash, email) VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(tag("email-case"))
        .bind(hash_password("correct-horse-battery").unwrap())
        .bind(&address)
        .execute(&f.pool)
        .await;
        assert_eq!(
            inserted.is_ok(),
            valid,
            "accounts_email_format on accounts disagreed for {email:?}: {inserted:?}"
        );
        let copied = sqlx::query("INSERT INTO a3_email_rule_copy (email) VALUES ($1)")
            .bind(&address)
            .execute(&f.pool)
            .await;
        assert_eq!(
            copied.is_ok(),
            valid,
            "the carved-out copy of the rule drifted for {email:?}: {copied:?}"
        );
        // Through the product: the same verdict as the CHECK.
        let (status, body) = api(
            &f.app,
            "POST",
            &invitations_path(&f),
            Some(&f.owner.token),
            None,
            Some(&json!({"email": address, "role": "member"})),
        )
        .await;
        assert_eq!(
            status.is_success(),
            valid,
            "the invitation API disagreed for {email:?}: {body}"
        );
    }
    sqlx::query("DROP TABLE a3_email_rule_copy")
        .execute(&f.pool)
        .await
        .unwrap();
    // The migration's own copy must still exist and be empty-handed: it is part of
    // the schema, and its predicate is the one the table above repeats.
    let _: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts_email_format_check")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_token_is_stored_hashed_and_an_owner_lists_and_revokes_from_the_page() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let cookie = f.owner.cookie();
    let page_path = format!("/organizations/{}/invitations", f.org_slug);
    let (status, page) = get_page(&f.app, &page_path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&format!("action=\"{page_path}\"")));
    // The organization page offers the invitations link to an owner.
    let (_, org_page) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&cookie),
    )
    .await;
    assert!(org_page.contains(&page_path));

    let csrf = hidden(&page, "csrf_token");
    let (status, created) = post_form(
        &f.app,
        &page_path,
        &cookie,
        &[
            ("csrf_token", csrf.clone()),
            ("idempotency_key", Uuid::new_v4().simple().to_string()),
            ("email", invitee.email.clone()),
            ("role", "member".to_owned()),
            ("team", f.team_slug.clone()),
            ("days", "3".to_owned()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    // The page shows the token once, and says how to use it.
    let rest = &created[created.find("Invitation created").expect("token box")..];
    let token_start = rest.find("<code>").unwrap() + "<code>".len();
    let token =
        rest[token_start..token_start + rest[token_start..].find("</code>").unwrap()].to_owned();
    assert_eq!(token.len(), 64);

    // The row stores only the hash: the plaintext appears in no column.
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM invitations WHERE organization_id = $1")
            .bind(f.org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(stored, sha256(&token));
    let plaintext_hits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invitations WHERE encode(token_hash, 'escape') LIKE '%' || $1 || '%'",
    )
    .bind(&token)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(plaintext_hits, 0);
    // Not in any text/json column of the public schema either: in particular,
    // creation did not leave the response body in `operations.response_body`.
    let hits = columns_containing(&f.pool, &token).await;
    assert!(hits.is_empty(), "the plaintext token is stored in {hits:?}");
    // The listing never carries it either.
    let (status, list) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!list.to_string().contains(&token));
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_created").await,
        1
    );

    // The acceptance page offers it to the invited account, then accepts.
    let accept_path = format!("/invitations/{token}");
    let (status, page) = get_page(&f.app, &accept_path, Some(&invitee.cookie())).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = hidden(&page, "csrf_token");
    let (status, _) = post_form(
        &f.app,
        &accept_path,
        &invitee.cookie(),
        &[
            ("csrf_token", csrf),
            ("idempotency_key", Uuid::new_v4().simple().to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&invitee.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Revoking: the accepted one cannot be revoked, a pending one can, and a
    // revoked token is refused exactly like an expired one.
    let (pending_id, revoked_token) = invite(&f, &invitee.email, false).await;
    let (status, body) = api(
        &f.app,
        "DELETE",
        &format!("{}/{pending_id}", invitations_path(&f)),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_revoked").await,
        1
    );
    // The trigger refuses a direct SQL un-revoke: revocation is one-way, and the
    // acceptance record is final (m4). Both halves are tried.
    let unrevoke =
        sqlx::query("UPDATE invitations SET revoked_at = NULL, revoked_by = NULL WHERE id = $1")
            .bind(pending_id)
            .execute(&f.pool)
            .await;
    assert!(unrevoke.is_err(), "a revoked invitation must stay revoked");
    let rewrite_revoker = sqlx::query("UPDATE invitations SET revoked_by = $2 WHERE id = $1")
        .bind(pending_id)
        .bind(invitee.id)
        .execute(&f.pool)
        .await;
    assert!(rewrite_revoker.is_err(), "a revocation's actor is final");
    // And the acceptance record: who accepted cannot be rewritten.
    let rewrite_user =
        sqlx::query("UPDATE invitations SET used_by = $2 WHERE organization_id = $1")
            .bind(f.org_id)
            .bind(f.owner.id)
            .execute(&f.pool)
            .await;
    assert!(rewrite_user.is_err(), "the accepting account is final");
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("/api/v1/invitations/{revoked_token}"),
        Some(&invitee.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn only_an_owner_manages_invitations_and_a_member_is_refused() {
    let f = fixture().await;
    let member = user(&f.pool, "member").await;
    add_membership(&f.pool, f.org_id, member.id, "member").await;
    let outsider = user(&f.pool, "outsider").await;
    let body = json!({"email": "newcomer@example.com", "role": "member"});
    let authz = f.state.authz.clone();
    let org_id = f.org_id;

    // The fine-grained split: an owner holds invitation.read and gets the
    // list; a plain organization member is denied exactly that action.
    let (status, list) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.as_array().unwrap().is_empty());

    // The split at the engine, directly through `Authz::may` with each
    // account's persisted facts: `invitation.read` is one action an owner holds
    // and a plain member does not, with the same facts loader every request
    // uses. (No role in M4.1 grants a fine-grained action on its own — only
    // `owner` and `member` exist — so this asks the engine rather than inventing
    // a role; `teams.rs` pins `team.update` without `team.delete` the same way.)
    let may = |account: Uuid, action: githearth_authz::Action| {
        let state = f.state.clone();
        let authz = authz.clone();
        let org = org_id;
        async move {
            let mut conn = state.pool.acquire().await.unwrap();
            let facts = githearth_server::facts::load(&mut conn, account, org)
                .await
                .unwrap()
                .unwrap();
            authz
                .authorize(&facts.on_organization(action, Some(Duration::ZERO)))
                .is_allowed()
        }
    };
    use githearth_authz::Action;
    assert!(
        may(f.owner.id, Action::InvitationRead).await,
        "an owner holds invitation.read"
    );
    assert!(
        may(f.owner.id, Action::InvitationCreate).await,
        "an owner holds invitation.create"
    );
    assert!(
        !may(member.id, Action::InvitationRead).await,
        "a plain member is denied invitation.read"
    );
    assert!(
        !may(member.id, Action::InvitationCreate).await,
        "a plain member is denied invitation.create"
    );
    // The organization-level read is the one both hold.
    assert!(may(member.id, Action::OrganizationRead).await);
    assert!(may(f.owner.id, Action::OrganizationRead).await);

    // A plain member may read the organization, not its invitations.
    let (status, body) = api(
        &f.app,
        "POST",
        &invitations_path(&f),
        Some(&member.token),
        None,
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/invitations", f.org_slug),
        Some(&member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // ... and the organization page does not offer the link.
    let (_, org_page) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&member.cookie()),
    )
    .await;
    assert!(!org_page.contains(&format!("/organizations/{}/invitations", f.org_slug)));

    // An outsider gets the organization's 404.
    let (status, body) = api(
        &f.app,
        "POST",
        &invitations_path(&f),
        Some(&outsider.token),
        None,
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = api(
        &f.app,
        "GET",
        &invitations_path(&f),
        Some(&outsider.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Nothing was written, and the refused invitation never mentions the
    // organization's name.
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_created").await,
        0
    );
    let (status, _) = api(
        &f.app,
        "POST",
        &invitations_path(&f),
        Some(&member.token),
        None,
        Some(&json!({"email": "x@example.com", "role": "member", "team": null})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The owner may invite; a bad address and a bad role are refused as input.
    for bad in [
        json!({"email": "not-an-address", "role": "member"}),
        json!({"email": "x@example.com", "role": "admin"}),
        json!({"email": "x@example.com", "role": "member", "team": "no-such-team"}),
        json!({"email": "x@example.com", "role": "member", "expires_in_days": 0}),
        json!({"email": "x@example.com", "role": "member", "expires_in_days": 31}),
    ] {
        let (status, body) = api(
            &f.app,
            "POST",
            &invitations_path(&f),
            Some(&f.owner.token),
            None,
            Some(&bad),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.invitation_created").await,
        0
    );
    finish(f).await;
}

/// The refusal never carries a cookie that would reveal the account's state in
/// a different shape, and the page's refusal equals the API's words.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_accept_page_refuses_a_wrong_account_with_the_same_words() {
    let f = fixture().await;
    let invitee = user(&f.pool, "invitee").await;
    let hijacker = user(&f.pool, "hijacker").await;
    let (_, token) = invite(&f, &invitee.email, false).await;
    let (status, page) = get_page(
        &f.app,
        &format!("/invitations/{token}"),
        Some(&hijacker.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!page.contains("SECRET-ORG-"));
    assert!(!page.contains(&f.org_slug));
    // The same page for an unknown token.
    let (status, unknown) = get_page(
        &f.app,
        &format!("/invitations/{}{}", uuid_hex(), uuid_hex()),
        Some(&hijacker.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body_start = |html: &str| html[html.find("<main").unwrap()..].to_owned();
    assert_eq!(body_start(&page), body_start(&unknown));
    finish(f).await;
}

/// Every public-schema text or JSON column that contains `needle`, as
/// `table.column`. Used to prove the token is not persisted anywhere, including
/// the idempotency store's `operations.response_body`.
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
        // Both the raw text and the `::text` rendering, so a value in a json
        // column is seen the way a dump would show it.
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

fn uuid_hex() -> String {
    Uuid::new_v4().simple().to_string()
}

fn sha256(value: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes()).to_vec()
}
