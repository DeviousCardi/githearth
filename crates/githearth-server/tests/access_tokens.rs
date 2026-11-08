//! Personal access tokens: API lifecycle, authentication, scope enforcement,
//! recent-auth gating, and Git Smart HTTP with a token.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// HTTP + fixture helpers
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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
    assert_eq!(status, StatusCode::OK);
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

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(id)
        .bind(tag("org"))
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
    id
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

/// Create a token through the session API; returns its plaintext.
async fn create_token(
    app: &Router,
    session: &str,
    scopes: &[&str],
    expires_in_days: Option<i64>,
) -> (Uuid, String) {
    let mut body = json!({"name": tag("token"), "scopes": scopes});
    if let Some(days) = expires_in_days {
        body["expires_in_days"] = json!(days);
    }
    let (status, value) = request(app, "POST", "/api/v1/user/tokens", Some(session), body).await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    let id: Uuid = value["id"].as_str().unwrap().parse().unwrap();
    let token = value["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("gth_"));
    assert_eq!(token.len(), 64 + 4);
    (id, token)
}

async fn api_pool() -> (PgPool, Router) {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-tok-{}", Uuid::new_v4())),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    (pool, app)
}

// ---------------------------------------------------------------------------
// API tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn token_create_list_revoke() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let (id, plaintext) = create_token(&app, &session, &["repo:read"], None).await;

    // A token authenticates the API as its account.
    let (status, me) = request(&app, "GET", "/api/v1/me", Some(&plaintext), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"], owner.to_string());

    // The listing omits any hash and any plaintext.
    let (status, listed) = request(
        &app,
        "GET",
        "/api/v1/user/tokens",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = listed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], id.to_string());
    assert!(list[0].get("token").is_none());
    assert!(list[0].get("token_hash").is_none());
    assert_eq!(list[0]["scopes"], json!(["repo:read"]));

    // Revoke, then the token stops authenticating.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/user/tokens/{id}"),
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(&plaintext), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // Both outbox events name the token, and never carry the secret (issue #67).
    let events: Vec<(String, Value)> = sqlx::query_as(
        "SELECT kind, payload FROM outbox_events WHERE payload->>'token_id' = $1 ORDER BY created_at",
    )
    .bind(id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["token.created", "token.revoked"]);
    for (_, payload) in &events {
        assert_eq!(payload["actor_id"], owner.to_string());
        assert!(!payload.to_string().contains(&plaintext));
    }

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn expired_token_is_unauthorized() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let (id, plaintext) = create_token(&app, &session, &["repo:write"], None).await;
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(&plaintext), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("UPDATE access_tokens SET expires_at = clock_timestamp() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", "/api/v1/me", Some(&plaintext), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_token_can_read_but_not_write() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let org_id = fixture_organization(&pool, owner).await;
    let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    fixture_repo(&pool, org_id, &repo_name, owner).await;

    let (_id, token) = create_token(&app, &session, &["repo:read"], None).await;

    // Reads succeed.
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Writes are refused with 403.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        json!({"name": tag("blocked"), "description": "", "visibility": "private"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Creating an organization is a write too.
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/organizations",
            Some(&token),
            json!({"slug": tag("blocked-org"), "name": "Blocked"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_boundary_fails_closed_when_lookup_errors() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let (_id, token) = create_token(&app, &session, &["repo:read"], None).await;

    // Break the middleware lookup: closing the pool makes
    // `authenticate_access_token` error, so the boundary must refuse the write
    // instead of falling through to the handler.
    pool.close().await;

    let slug = tag("blocked-org");
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": slug, "name": "Blocked"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    // Reconnect to prove the write was never applied.
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-tok-{}", Uuid::new_v4())),
        ..Config::default()
    };
    let check = connect(&config).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE slug = $1")
        .bind(&slug)
        .fetch_one(&check)
        .await
        .unwrap();
    assert_eq!(count, 0);
    check.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_token_blocked_from_pulls_reviews_and_protection() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let org_id = fixture_organization(&pool, owner).await;
    let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    fixture_repo(&pool, org_id, &repo_name, owner).await;

    let (_id, token) = create_token(&app, &session, &["repo:read"], None).await;
    let pulls_path = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/pulls");

    // Opening a pull request is a write the layer must refuse for a read token.
    let (status, body) = request(
        &app,
        "POST",
        &pulls_path,
        Some(&token),
        json!({"title": "blocked", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Submitting a review (approval) is a write too.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("{pulls_path}/1/reviews"),
            Some(&token),
            json!({"verdict": "approved", "commit_id": "0123456789abcdef0123456789abcdef01234567"}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // Creating a branch protection rule is a write too.
    assert_eq!(
        request(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/protection"),
            Some(&token),
            json!({"pattern": "main"}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn token_cannot_manage_tokens() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let (_id, token) = create_token(&app, &session, &["repo:write"], None).await;

    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/user/tokens",
            Some(&token),
            json!({"name": "nested", "scopes": ["repo:read"]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/user/tokens",
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/v1/user/tokens/{}", Uuid::new_v4()),
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn token_fails_recent_auth_gated_action() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    let owner = fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;
    let org_id = fixture_organization(&pool, owner).await;
    let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    fixture_repo(&pool, org_id, &repo_name, owner).await;

    let (_id, token) = create_token(&app, &session, &["repo:write"], None).await;

    // Archiving is a sensitive action that also requires repo:write, so a
    // repo:write token passes scope but still fails fresh-auth.
    let archive = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/archive");
    assert_eq!(
        request(&app, "POST", &archive, Some(&token), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // A fresh session succeeds: it satisfies both scope and fresh-auth.
    assert_eq!(
        request(&app, "POST", &archive, Some(&session), Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn write_token_is_looked_up_once_per_request() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let (token_id, token) = create_token(&app, &session, &["repo:write"], None).await;

    // Revoke the token from a trigger that fires on the first `last_used_at`
    // refresh. The middleware's single lookup refreshes it and, via this
    // trigger, revokes it; a second lookup in the handler would then observe
    // `revoked_at` and refuse the write. The write succeeding proves the
    // handler reused the middleware's lookup instead of querying again.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION revoke_on_use() RETURNS trigger AS $$
         BEGIN
           UPDATE access_tokens SET revoked_at = clock_timestamp() WHERE id = NEW.id;
           RETURN NEW;
         END $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER revoke_on_use_{} \
         AFTER UPDATE OF last_used_at ON access_tokens \
         FOR EACH ROW WHEN (NEW.id = '{token_id}') EXECUTE FUNCTION revoke_on_use()",
        token_id.simple()
    ))
    .execute(&pool)
    .await
    .unwrap();

    // The trigger has not fired yet: the token starts with no last_used_at.
    let initial: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM access_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!initial);

    // A write with the repo:write token still succeeds.
    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": tag("org"), "name": "Once"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Exactly one lookup: that lookup refreshed `last_used_at`, which fired the
    // trigger and revoked the token. A second lookup would have seen the
    // revocation and refused the write, so the successful write means the
    // handler never queried the token again.
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM access_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked, "token should be revoked by the trigger");

    // The test database is shared: leave no trigger behind.
    sqlx::query(&format!(
        "DROP TRIGGER revoke_on_use_{} ON access_tokens",
        token_id.simple()
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_token_write_is_still_refused() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let (_id, token) = create_token(&app, &session, &["repo:read"], None).await;

    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&token),
        json!({"slug": tag("blocked-org"), "name": "Blocked"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, json!({"error": "read-only token"}));

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn session_write_is_unchanged() {
    let (pool, app) = api_pool().await;
    let username = tag("user");
    fixture_account(&pool, &username, "correct horse battery staple").await;
    let session = login(&app, &username, "correct horse battery staple").await;

    let (status, body) = request(
        &app,
        "POST",
        "/api/v1/organizations",
        Some(&session),
        json!({"slug": tag("org"), "name": "Session Org"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Git Smart HTTP tests
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
        Ok(Ok(Err(e))) => panic!("git command failed to launch: {e}"),
        Ok(Err(_)) => panic!("spawn_blocking join error"),
        Err(_) => panic!("git command timed out after 60 s"),
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

struct GitFx {
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    repo_name: String,
    port: u16,
    read_token: String,
    write_token: String,
}

async fn git_setup() -> GitFx {
    let storage_root = std::env::temp_dir().join(format!("githearth-tok-git-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let username = tag("owner");
    let owner = fixture_account(&pool, &username, "testpass").await;
    let org_id = fixture_organization(&pool, owner).await;
    let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    let repo_id = fixture_repo(&pool, org_id, &repo_name, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let repo_path = state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let work = std::env::temp_dir().join(format!("githearth-tok-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]))
    .await;
    seed_bare(repo_path.to_str().unwrap(), tw).await;
    let _ = std::fs::remove_dir_all(&work);

    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let session = login(&app, &username, "testpass").await;
    let (_rid, read_token) = create_token(&app, &session, &["repo:read"], None).await;
    let (_wid, write_token) = create_token(&app, &session, &["repo:write"], None).await;

    GitFx {
        pool,
        storage_root,
        org_slug,
        repo_name,
        port,
        read_token,
        write_token,
    }
}

fn token_url(port: u16, org: &str, repo: &str, token: &str) -> String {
    format!("http://x-access-token:{token}@127.0.0.1:{port}/{org}/{repo}.git")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn git_clone_with_token_and_push_fails_for_read_token() {
    let fx = git_setup().await;

    // Clone works with a read token.
    let clone_dir = std::env::temp_dir().join(format!("git-tok-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let read_url = token_url(fx.port, &fx.org_slug, &fx.repo_name, &fx.read_token);
    git_ok(git_cmd(&["clone", "--", &read_url, cd])).await;
    assert_eq!(
        std::fs::read_to_string(clone_dir.join("README.md")).unwrap(),
        "# Hello\n"
    );

    // Add a commit and try to push it with the read token: refused.
    std::fs::write(clone_dir.join("FEATURE.md"), "# feature\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "feature"])).await;
    git_ok(git_cmd(&["-C", cd, "add", "FEATURE.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "feature",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;
    assert!(!out.status.success(), "read-only push must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("permission denied: ref.push"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The same push succeeds with a write token.
    let write_remote = token_url(fx.port, &fx.org_slug, &fx.repo_name, &fx.write_token);
    git_ok(git_cmd(&[
        "-C",
        cd,
        "remote",
        "set-url",
        "origin",
        &write_remote,
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;

    let _ = std::fs::remove_dir_all(&clone_dir);
    let _ = std::fs::remove_dir_all(&fx.storage_root);
    fx.pool.close().await;
}

// ---------------------------------------------------------------------------
// Write boundary: a repo:write token can open a pull request
// ---------------------------------------------------------------------------

/// A repository with `main` (target) and `feature` (source) branches, plus a
/// session, a read token and a write token for its owner. Refs are seeded
/// directly into storage (no HTTP transport needed for the API tests).
struct PrFixture {
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    repo_name: String,
    write_token: String,
    app: Router,
}

async fn pr_fixture() -> PrFixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-tok-pr-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let username = tag("owner");
    let owner = fixture_account(&pool, &username, "testpass").await;
    let org_id = fixture_organization(&pool, owner).await;
    let org_slug: String = sqlx::query_scalar("SELECT slug FROM organizations WHERE id = $1")
        .bind(org_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    let repo_id = fixture_repo(&pool, org_id, &repo_name, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let repo_path = state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let work = std::env::temp_dir().join(format!("githearth-tok-pr-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]))
    .await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "-b", "feature"])).await;
    std::fs::write(work.join("FEATURE.md"), "# feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "FEATURE.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "feature",
    ]))
    .await;

    // Direct storage pushes bypass GitHearth, so skip its ref-transaction hooks.
    let repo_path_str = repo_path.to_str().unwrap();
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path_str,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        tw,
        "main:refs/heads/main",
        "feature:refs/heads/feature",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);

    let app = router(state);
    let session = login(&app, &username, "testpass").await;
    let (_wid, write_token) = create_token(&app, &session, &["repo:write"], None).await;

    PrFixture {
        pool,
        storage_root,
        org_slug,
        repo_name,
        write_token,
        app,
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn write_token_can_open_pull_request() {
    let fx = pr_fixture().await;
    let pulls_path = format!(
        "/api/v1/organizations/{}/repositories/{}/pulls",
        fx.org_slug, fx.repo_name
    );

    let (status, body) = request(
        &fx.app,
        "POST",
        &pulls_path,
        Some(&fx.write_token),
        json!({"title": "open", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["source_branch"], "feature");

    let _ = std::fs::remove_dir_all(&fx.storage_root);
    fx.pool.close().await;
}
