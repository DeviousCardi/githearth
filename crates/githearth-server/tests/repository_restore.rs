//! Repository restore from trash (M4.1 U23c).
//!
//! Acceptance goes through the JSON API, the browser page and Git over HTTP
//! and SSH. Direct PostgreSQL and filesystem writes are fixtures and race
//! orchestration only: no refusal or success below is produced by bypassing a
//! surface.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{
    AppState, MIGRATOR, config::Config, connect, hash_password, reconcile, router, ssh,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures and transport helpers
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

struct Env {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    port: u16,
    owner: Uuid,
    owner_name: String,
    org_id: Uuid,
    org_slug: String,
    owner_token: String,
}

impl Env {
    fn api_path(&self, suffix: &str) -> String {
        format!(
            "/api/v1/organizations/{}/repositories{}",
            self.org_slug, suffix
        )
    }

    fn web_path(&self, name: &str) -> String {
        format!("/organizations/{}/repositories/{}", self.org_slug, name)
    }

    fn git_url(&self, name: &str) -> String {
        format!(
            "http://{}:testpass@127.0.0.1:{}/{}/{}.git",
            self.owner_name, self.port, self.org_slug, name
        )
    }

    /// The unauthenticated Git HTTP `info/refs` route, for status assertions.
    fn git_refs_path(&self, name: &str) -> String {
        format!(
            "/{}/{}.git/info/refs?service=git-upload-pack",
            self.org_slug, name
        )
    }
}

async fn fixture_account(pool: &PgPool, username: &str, admin: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,$3,$4) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(hash_password("testpass").unwrap())
    .bind(admin)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture_membership(pool: &PgPool, org: Uuid, account: Uuid) {
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
}

async fn fixture_org_grant(pool: &PgPool, org: Uuid, account: Uuid, role: &str) {
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,$2,'account',$3,'organization',$4,$3)",
    )
    .bind(Uuid::new_v4())
    .bind(role)
    .bind(account)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}

async fn fixture_session(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')",
    )
    .bind(&token)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    token
}

fn test_config(db_url: &str, storage_root: &Path, ssh: bool) -> Config {
    Config {
        database_url: db_url.to_owned(),
        database_max_connections: 8,
        database_timeout_seconds: 3,
        storage_root: storage_root.to_path_buf(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        git_timeout_seconds: 120,
        ssh_enabled: ssh,
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: storage_root.join("ssh_host_key"),
        // The race and purge tests backdate one entry by two hours; keep the
        // configured window at the minimum so that entry is genuinely expired.
        trash_retention_seconds: 3600,
        ..Config::default()
    }
}

async fn setup_with(ssh_enabled: bool) -> Env {
    let storage_root = std::env::temp_dir().join(format!("githearth-restore-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let canonical = std::fs::canonicalize(&storage_root).unwrap();
    let db_url =
        std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
    let config = test_config(&db_url, &canonical, ssh_enabled);
    if ssh_enabled {
        ssh::generate_host_key(&config.ssh_host_key_path).unwrap();
    }
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner_name = tag("owner");
    let owner = fixture_account(&pool, &owner_name, false).await;
    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Restore Org')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    fixture_org_grant(&pool, org_id, owner, "owner").await;
    let owner_token = fixture_session(&pool, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = app.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    Env {
        app,
        state,
        pool,
        storage_root: canonical,
        port,
        owner,
        owner_name,
        org_id,
        org_slug,
        owner_token,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage_root);
    }
}

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let body = body.map_or_else(String::new, |value| value.to_string());
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
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

/// The status of an unauthenticated request, for surfaces where the Git
/// client would stop at "could not read Username" on a `401` instead of
/// reporting the status code.
async fn anonymous_status(app: &Router, path: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn post_form(
    app: &Router,
    path: &str,
    cookie: &str,
    fields: &[(&str, &str)],
) -> (StatusCode, Option<String>) {
    let body = fields
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
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, location)
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
    let rest = &html[html.find(&marker).expect("hidden field") + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

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
        Ok(Err(_)) => panic!("git task panicked"),
        Err(_) => panic!("git timed out"),
    }
}

async fn git_ok(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn git_fail(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(!out.status.success(), "git unexpectedly succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

async fn create_repo(env: &Env, name: &str) -> Uuid {
    let (status, body) = api(
        &env.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", env.org_slug),
        Some(&env.owner_token),
        None,
        Some(json!({"name": name})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn delete_repo(env: &Env, name: &str) {
    let (status, body) = api(
        &env.app,
        "DELETE",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

async fn restore_repo(
    env: &Env,
    name: &str,
    token: &str,
    key: Option<&str>,
) -> (StatusCode, Value) {
    api(
        &env.app,
        "POST",
        &env.api_path(&format!("/{name}/restore")),
        Some(token),
        key,
        None,
    )
    .await
}

/// Seed two branches in the bare repo the API created, bypassing receive-pack
/// (which is not what this suite tests). Returns `(main, feature)`.
async fn seed_history(env: &Env, repo_id: Uuid) -> (String, String) {
    let work = std::env::temp_dir().join(format!("restore-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "init",
        "-b",
        "main",
    ]))
    .await;
    std::fs::write(work.join("README.md"), "first\n").unwrap();
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
        "first",
    ]))
    .await;
    let main = git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "rev-parse",
        "HEAD",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "checkout",
        "-b",
        "feature",
    ]))
    .await;
    std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "add",
        "feature.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "feature",
    ]))
    .await;
    let feature = git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "rev-parse",
        "HEAD",
    ]))
    .await;

    let bare = env.state.storage.repository_path(repo_id);
    git_ok(git_cmd(&[
        "--git-dir",
        bare.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work.to_str().unwrap(),
        "+refs/heads/*:refs/heads/*",
    ]))
    .await;
    git_ok(git_cmd(&[
        "--git-dir",
        bare.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);
    (main, feature)
}

async fn clone_head(env: &Env, name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("restore-clone-{}", Uuid::new_v4()));
    git_ok(git_cmd(&[
        "clone",
        "--quiet",
        &env.git_url(name),
        dir.to_str().unwrap(),
    ]))
    .await;
    let head = git_ok(git_cmd(&["-C", dir.to_str().unwrap(), "rev-parse", "HEAD"])).await;
    let count = git_ok(git_cmd(&[
        "-C",
        dir.to_str().unwrap(),
        "rev-list",
        "--count",
        "HEAD",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(count, "1", "clone should have main's single commit");
    head
}

async fn create_issue(env: &Env, name: &str, title: &str) -> Value {
    let (status, body) = api(
        &env.app,
        "POST",
        &env.api_path(&format!("/{name}/issues")),
        Some(&env.owner_token),
        None,
        Some(json!({"title": title, "body": "kept across restore"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

async fn create_pull(env: &Env, name: &str, title: &str) -> Value {
    let (status, body) = api(
        &env.app,
        "POST",
        &env.api_path(&format!("/{name}/pulls")),
        Some(&env.owner_token),
        None,
        Some(json!({
            "title": title,
            "source_branch": "feature",
            "target_branch": "main"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

/// Rename a trash entry to an old timestamp so the configured retention sweep
/// treats it as expired without waiting an hour.
fn backdate_trash(env: &Env, repo_id: Uuid) {
    let entry = env
        .state
        .storage
        .find_trash(repo_id)
        .unwrap()
        .expect("trash entry");
    let old = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs())
    .saturating_sub(7200);
    let target = entry
        .path
        .parent()
        .unwrap()
        .join(format!("{}.{}.git", repo_id.simple(), old));
    std::fs::rename(&entry.path, &target).unwrap();
}

async fn reconcile_once(env: &Env) -> reconcile::Summary {
    let mut attempts = HashMap::new();
    reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap()
}

async fn row_status(env: &Env, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM repositories WHERE id=$1")
        .bind(id)
        .fetch_one(&env.pool)
        .await
        .unwrap()
}

async fn set_status(env: &Env, id: Uuid, status: &str) {
    sqlx::query("UPDATE repositories SET status=$1 WHERE id=$2")
        .bind(status)
        .bind(id)
        .execute(&env.pool)
        .await
        .unwrap();
}

async fn set_visibility(env: &Env, id: Uuid, visibility: &str) {
    sqlx::query("UPDATE repositories SET visibility=$1 WHERE id=$2")
        .bind(visibility)
        .bind(id)
        .execute(&env.pool)
        .await
        .unwrap();
}

async fn redirect_rows(env: &Env, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM repository_redirects WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(env.org_id)
    .bind(name)
    .fetch_one(&env.pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// U23c clauses
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn migration_only_adds_the_restore_transitions() {
    let env = setup_with(false).await;
    let name = tag("transitions");
    let id = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    // deleted -> active is still forbidden; deleted -> restoring is the only
    // way out of deleted.
    let err = sqlx::query("UPDATE repositories SET status='active' WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .unwrap_err();
    assert_eq!(
        err.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );

    sqlx::query("UPDATE repositories SET status='restoring' WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .expect("deleted -> restoring");

    // A restoring row may only complete or go back to deleted.
    let err = sqlx::query("UPDATE repositories SET status='deleting' WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .unwrap_err();
    assert_eq!(
        err.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );

    sqlx::query("UPDATE repositories SET status='deleted' WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .expect("restoring -> deleted");
    sqlx::query("UPDATE repositories SET status='restoring' WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .expect("deleted -> restoring again");
    sqlx::query("UPDATE repositories SET status='active', deleted_at=NULL WHERE id=$1")
        .bind(id)
        .execute(&env.pool)
        .await
        .expect("restoring -> active");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_keeps_id_history_issues_and_pull_requests() {
    let env = setup_with(false).await;
    let name = tag("keep");
    let id = create_repo(&env, &name).await;
    let (main, _feature) = seed_history(&env, id).await;
    assert_eq!(clone_head(&env, &name).await, main);

    let issue = create_issue(&env, &name, "issue survives delete").await;
    let pull = create_pull(&env, &name, "pull survives delete").await;

    delete_repo(&env, &name).await;
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");

    let key = tag("restore-key");
    let (status, restored) = restore_repo(&env, &name, &env.owner_token, Some(&key)).await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["id"], json!(id), "restore changed the id");

    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    // The same commit is reachable through Git after the directory returned.
    assert_eq!(clone_head(&env, &name).await, main);

    let (_, issues) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}/issues?state=all&limit=100")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert!(
        issues["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == issue["id"]),
        "issue lost: {issues}"
    );

    let (_, pulls) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}/pulls?state=all&limit=100")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert!(
        pulls["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == pull["id"]),
        "pull request lost: {pulls}"
    );

    // A repeated restore with the same key replays the stored response; a new
    // unkeyed restore is refused because the row is no longer deleted.
    let (status, replay) = restore_repo(&env, &name, &env.owner_token, Some(&key)).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["id"], json!(id));
    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "repository is not deleted");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn revoked_grant_stays_revoked_and_active_grant_still_works() {
    let env = setup_with(false).await;
    let name = tag("grants");
    let id = create_repo(&env, &name).await;

    let reader_name = tag("reader");
    let reader = fixture_account(&env.pool, &reader_name, false).await;
    fixture_membership(&env.pool, env.org_id, reader).await;
    fixture_org_grant(&env.pool, env.org_id, reader, "read").await;
    let reader_token = fixture_session(&env.pool, reader).await;

    let revoked_name = tag("revoked");
    let revoked = fixture_account(&env.pool, &revoked_name, false).await;
    fixture_membership(&env.pool, env.org_id, revoked).await;
    let revoked_token = fixture_session(&env.pool, revoked).await;

    let deleted_name = tag("deleted-grant");
    let deleted = fixture_account(&env.pool, &deleted_name, false).await;
    fixture_membership(&env.pool, env.org_id, deleted).await;
    let deleted_token = fixture_session(&env.pool, deleted).await;

    // One repository grant is revoked before the delete; another is active and
    // is revoked by the delete itself. Both must stay revoked after restore.
    let (status, grant) = api(
        &env.app,
        "POST",
        &env.api_path(&format!("/{name}/access")),
        Some(&env.owner_token),
        None,
        Some(json!({
            "subject_kind": "account",
            "subject_id": revoked,
            "role": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{grant}");
    let grant_id = grant["id"].as_str().unwrap();
    let (status, _) = api(
        &env.app,
        "DELETE",
        &env.api_path(&format!("/{name}/access/{grant_id}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, active_grant) = api(
        &env.app,
        "POST",
        &env.api_path(&format!("/{name}/access")),
        Some(&env.owner_token),
        None,
        Some(json!({
            "subject_kind": "account",
            "subject_id": deleted,
            "role": "read"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{active_grant}");

    // The organization read grant is not repository-scoped, so the delete does
    // not revoke it; it must still work after the restore.
    assert_eq!(
        api(
            &env.app,
            "GET",
            &env.api_path(&format!("/{name}")),
            Some(&reader_token),
            None,
            None
        )
        .await
        .0,
        StatusCode::OK
    );

    delete_repo(&env, &name).await;
    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    assert_eq!(
        api(
            &env.app,
            "GET",
            &env.api_path(&format!("/{name}")),
            Some(&reader_token),
            None,
            None
        )
        .await
        .0,
        StatusCode::OK,
        "an active organization grant must survive the restore"
    );
    assert_eq!(
        api(
            &env.app,
            "GET",
            &env.api_path(&format!("/{name}")),
            Some(&revoked_token),
            None,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "a revoked repository grant was resurrected"
    );
    assert_eq!(
        api(
            &env.app,
            "GET",
            &env.api_path(&format!("/{name}")),
            Some(&deleted_token),
            None,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "a grant revoked by the delete was resurrected"
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_owner_refused_and_site_admin_allowed() {
    let env = setup_with(false).await;
    let name = tag("authority");
    let id = create_repo(&env, &name).await;

    // A repository admin is not an organization owner: it can read the repo
    // (so the refusal is a 403, not a masked 404) but cannot restore it.
    let admin_name = tag("repo-admin");
    let admin = fixture_account(&env.pool, &admin_name, false).await;
    fixture_membership(&env.pool, env.org_id, admin).await;
    fixture_org_grant(&env.pool, env.org_id, admin, "admin").await;
    let admin_token = fixture_session(&env.pool, admin).await;

    let site_admin_name = tag("site-admin");
    let site_admin = fixture_account(&env.pool, &site_admin_name, true).await;
    let site_admin_token = fixture_session(&env.pool, site_admin).await;

    delete_repo(&env, &name).await;

    let (status, body) = restore_repo(&env, &name, &admin_token, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "not permitted");

    let (status, body) = restore_repo(&env, &name, &site_admin_token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    // The audit row names the site administrator who performed the restore.
    let (status, audit) = api(
        &env.app,
        "GET",
        &format!("/api/v1/organizations/{}/audit?limit=200", env.org_slug),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let restored = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "repository.restored")
        .expect("repository.restored audit event");
    assert_eq!(restored["actor_id"], json!(site_admin));
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cli_restore_uses_the_same_authorization() {
    let env = setup_with(false).await;
    let name = tag("cli");
    let id = create_repo(&env, &name).await;

    let reader_name = tag("cli-reader");
    let reader = fixture_account(&env.pool, &reader_name, false).await;
    fixture_membership(&env.pool, env.org_id, reader).await;
    fixture_org_grant(&env.pool, env.org_id, reader, "read").await;

    delete_repo(&env, &name).await;

    let run = |actor: String| {
        let org = env.org_slug.clone();
        let name = name.clone();
        let storage = env.storage_root.clone();
        let db = std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap();
        tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_githearth-server"))
                .args([
                    "repository",
                    "restore",
                    "--organization",
                    &org,
                    "--name",
                    &name,
                    "--actor",
                    &actor,
                ])
                .env("GITHEARTH_DATABASE_URL", db)
                .env("GITHEARTH_STORAGE_ROOT", storage)
                .output()
                .unwrap()
        })
    };

    let refused = run(reader_name.clone()).await.unwrap();
    assert!(!refused.status.success(), "non-owner CLI restore succeeded");
    let stderr = String::from_utf8_lossy(&refused.stderr).to_lowercase();
    assert!(
        stderr.contains("not permitted"),
        "CLI refusal should match the API 403: {stderr}"
    );

    let restored = run(env.owner_name.clone()).await.unwrap();
    assert!(
        restored.status.success(),
        "owner CLI restore failed: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(
        String::from_utf8_lossy(&restored.stdout).contains(&id.to_string()),
        "CLI did not report the restored id"
    );

    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_after_purge_is_refused() {
    let env = setup_with(false).await;
    let name = tag("purged");
    let id = create_repo(&env, &name).await;

    delete_repo(&env, &name).await;
    backdate_trash(&env, id);
    let summary = reconcile_once(&env).await;
    assert_eq!(summary.trash_purged, 1, "{summary:?}");

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "repository is no longer in trash");

    let (status, _) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_onto_taken_name_is_refused_and_row_stays_deleted() {
    let env = setup_with(false).await;
    let name = tag("collision");
    let original = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    let replacement = create_repo(&env, &name).await;
    assert_ne!(replacement, original);
    // A live replacement that reused the name must win the path for everyone,
    // including an anonymous visitor; it does not inherit the deleted row's 410.
    sqlx::query("UPDATE repositories SET visibility='public' WHERE id=$1")
        .bind(replacement)
        .execute(&env.pool)
        .await
        .unwrap();
    let (status, _) = page(&env.app, &env.web_path(&name), None).await;
    assert_eq!(status, StatusCode::OK, "live replacement inherited 410");

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "repository name already taken");

    // The path still belongs to the replacement.
    let (_, live) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(live["id"], json!(replacement));

    // Free the name, then the original row is still restorable: the refused
    // restore changed neither the row nor its trash entry.
    let (status, _) = api(
        &env.app,
        "PATCH",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        Some(json!({"name": tag("moved")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(original));
    let (_, live) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(live["id"], json!(original));
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn old_path_is_410_on_api_web_and_git_http_then_resolves() {
    let env = setup_with(false).await;
    let name = tag("path");
    let id = create_repo(&env, &name).await;
    let (main, _feature) = seed_history(&env, id).await;
    delete_repo(&env, &name).await;

    // API and browser: 410, not 404 or a redirect.
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"], "repository is gone");

    let cookie = format!("githearth_session={}", env.owner_token);
    let (status, html) = page(&env.app, &env.web_path(&name), Some(&cookie)).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(html.contains("Repository gone"), "{html}");
    assert!(html.contains("Restore repository"), "{html}");

    // Git over HTTP: the info/refs response is 410.
    let stderr = git_fail(git_cmd(&["ls-remote", &env.git_url(&name)])).await;
    assert!(stderr.contains("410"), "{stderr}");

    // The browser form performs the same restore the API does.
    let csrf = hidden(&html, "csrf_token");
    let key = hidden(&html, "idempotency_key");
    let (status, location) = post_form(
        &env.app,
        &format!("{}/restore", env.web_path(&name)),
        &cookie,
        &[("csrf_token", &csrf), ("idempotency_key", &key)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{location:?}");
    assert_eq!(location.as_deref(), Some(env.web_path(&name).as_str()));

    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));

    let (status, html) = page(&env.app, &env.web_path(&name), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(&name),
        "restored page should name the repository"
    );

    // The Git HTTP path resolves again to the same history.
    let refs = git_ok(git_cmd(&["ls-remote", &env.git_url(&name)])).await;
    assert!(refs.contains(&main), "{refs}");
    assert_eq!(clone_head(&env, &name).await, main);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_move_leaves_the_row_retryable() {
    let env = setup_with(false).await;
    let name = tag("retry");
    let id = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    // A non-empty directory at the restore target makes the rename fail.
    let target = env.state.storage.repository_path(id);
    std::fs::create_dir_all(target.join("blocker")).unwrap();
    std::fs::write(target.join("blocker/file"), "blocked").unwrap();

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "failed move should be a storage error: {body}"
    );
    // The revert is what this test pins: without it the row would stay
    // `restoring` and the retry below would silently resume instead of
    // starting from `deleted`.
    assert_eq!(
        row_status(&env, id).await,
        "deleted",
        "a failed move must put the row back in `deleted`"
    );
    assert!(
        env.state.storage.find_trash(id).unwrap().is_some(),
        "the failed move must leave the trash entry for a retry"
    );

    std::fs::remove_dir_all(&target).unwrap();

    // The row is already `deleted`, so this pass has nothing to do: the grace
    // period has not elapsed and no trash entry is expired. A retry would
    // legitimately resume a `restoring` row, which is why the status assertion
    // above — not this pass — is what proves the revert ran.
    let summary = reconcile_once(&env).await;
    assert_eq!(summary.restored, 0, "{summary:?}");
    assert_eq!(summary.trash_purged, 0, "{summary:?}");

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::OK, "the row was not retryable: {body}");
    assert_eq!(body["id"], json!(id));
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn purge_racing_a_restore_does_not_destroy_a_restoring_repository() {
    let env = setup_with(false).await;
    let name = tag("race");
    let id = create_repo(&env, &name).await;
    let (main, _feature) = seed_history(&env, id).await;
    delete_repo(&env, &name).await;
    backdate_trash(&env, id);

    // Hold the repository lock, queue the purge first, then start a restore.
    // The restore commits `restoring` before it waits for the same lock, so
    // when the purge finally runs it must re-read the row under the lock and
    // skip the entry. Without that lock the purge would remove the entry
    // between the restore's lookup and its rename.
    let guard = env.state.storage.repository_lock(id).lock_owned().await;
    let state = env.state.clone();
    let purger = tokio::spawn(async move {
        let mut attempts = HashMap::new();
        reconcile::reconcile_once(&state, &mut attempts)
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let app = env.app.clone();
    let token = env.owner_token.clone();
    let path = env.api_path(&format!("/{name}/restore"));
    let restorer =
        tokio::spawn(async move { api(&app, "POST", &path, Some(&token), None, None).await });

    // Wait until the restore owns the `restoring` state, then let both go.
    let mut saw_restoring = false;
    for _ in 0..100 {
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM repositories WHERE id=$1")
                .bind(id)
                .fetch_optional(&env.pool)
                .await
                .unwrap();
        if status.as_deref() == Some("restoring") {
            saw_restoring = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw_restoring, "restore never entered restoring");
    drop(guard);

    let summary = purger.await.unwrap();
    assert_eq!(summary.errors, 0, "{summary:?}");
    assert_eq!(
        summary.trash_purged, 0,
        "the purge removed a restoring repository: {summary:?}"
    );

    let (status, body) = restorer.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(id));
    assert_eq!(clone_head(&env, &name).await, main);
}

// ---------------------------------------------------------------------------
// Disclosure: `410` is not an oracle
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn gone_is_not_an_existence_oracle_for_a_deleted_private_repository() {
    let env = setup_with(false).await;
    let name = tag("oracle");
    let id = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    // A plain organization member with no grant on this private repository.
    let member_name = tag("oracle-member");
    let member = fixture_account(&env.pool, &member_name, false).await;
    fixture_membership(&env.pool, env.org_id, member).await;
    let member_token = fixture_session(&env.pool, member).await;

    // API: the same 404 body as a name that never existed.
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&member_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let missing = tag("never-existed");
    let (missing_status, missing_body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{missing}")),
        Some(&member_token),
        None,
        None,
    )
    .await;
    assert_eq!(missing_status, status);
    assert_eq!(missing_body, body);

    // Web, anonymous: the ordinary sign-in challenge, not `410`.
    let (status, _) = page(&env.app, &env.web_path(&name), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "anonymous web leaked 410");
    let (status, _) = page(&env.app, &env.web_path(&missing), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Web, signed in but unable to read: the ordinary not-found page.
    let cookie = format!("githearth_session={member_token}");
    let (status, _) = page(&env.app, &env.web_path(&name), Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "member web leaked 410");

    // Git HTTP, anonymous: 401, exactly like a private live repository.
    let status = anonymous_status(&env.app, &env.git_refs_path(&name)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "anonymous Git HTTP leaked the deleted path"
    );

    // Git HTTP, signed in without a grant: 404.
    let url = format!(
        "http://{member_name}:testpass@127.0.0.1:{}/{}/{}.git",
        env.port, env.org_slug, name
    );
    let stderr = git_fail(git_cmd(&["ls-remote", &url])).await;
    assert!(
        stderr.contains("not found") && !stderr.to_lowercase().contains("gone"),
        "member Git HTTP leaked the deleted path: {stderr}"
    );

    // The row is untouched by the refused lookups.
    assert_eq!(row_status(&env, id).await, "deleted");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn gone_stays_410_for_public_grant_and_site_admin_viewers() {
    let env = setup_with(false).await;

    // 1. Public deleted: everyone who could read it before still gets 410.
    let public_name = tag("gone-public");
    let public_id = create_repo(&env, &public_name).await;
    set_visibility(&env, public_id, "public").await;
    delete_repo(&env, &public_name).await;
    let (status, _) = page(&env.app, &env.web_path(&public_name), None).await;
    assert_eq!(status, StatusCode::GONE, "public deleted web lost its 410");
    let status = anonymous_status(&env.app, &env.git_refs_path(&public_name)).await;
    assert_eq!(
        status,
        StatusCode::GONE,
        "public deleted Git HTTP lost its 410"
    );

    // 2. Private deleted, a live grant: the grant holder may read it, so 410.
    let granted_name = tag("gone-granted");
    let granted_id = create_repo(&env, &granted_name).await;
    let reader_name = tag("granted-reader");
    let reader = fixture_account(&env.pool, &reader_name, false).await;
    fixture_membership(&env.pool, env.org_id, reader).await;
    fixture_org_grant(&env.pool, env.org_id, reader, "read").await;
    let reader_token = fixture_session(&env.pool, reader).await;
    delete_repo(&env, &granted_name).await;
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{granted_name}")),
        Some(&reader_token),
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::GONE,
        "grant holder lost the 410: {body}"
    );
    assert_eq!(body["error"], "repository is gone");

    // 3. Private deleted, a non-member site administrator: it may restore the
    //    row, so the 410 page (with the form) is the right answer.
    let admin_name = tag("gone-site-admin");
    let admin = fixture_account(&env.pool, &admin_name, true).await;
    let admin_token = fixture_session(&env.pool, admin).await;
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{granted_name}")),
        Some(&admin_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "site admin lost the 410: {body}");
    let cookie = format!("githearth_session={admin_token}");
    let (status, html) = page(&env.app, &env.web_path(&granted_name), Some(&cookie)).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(html.contains("Restore repository"), "{html}");

    assert_eq!(row_status(&env, granted_id).await, "deleted");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn private_replacement_does_not_inherit_the_deleted_rows_410() {
    let env = setup_with(false).await;
    let name = tag("private-replacement");
    let original = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    // The replacement stays private (the default): it wins the path for a
    // caller who can see it, and anonymous visitors get the sign-in challenge
    // rather than the old row's `410`.
    let replacement = create_repo(&env, &name).await;
    assert_ne!(replacement, original);

    let (status, _) = page(&env.app, &env.web_path(&name), None).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "anonymous visitor got the deleted row's 410 instead of the login challenge"
    );
    let status = anonymous_status(&env.app, &env.git_refs_path(&name)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "anonymous Git HTTP got the deleted row's 410"
    );

    // The owner sees the live replacement, not the deleted row.
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(replacement));
}

// ---------------------------------------------------------------------------
// Redirects, restore and delete
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_stale_rename_redirect_does_not_survive_a_restore() {
    let env = setup_with(false).await;
    let name = tag("redirect");
    let original = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;

    // A second repository reuses the deleted name, then is renamed away,
    // leaving `name -> replacement` behind.
    let _replacement = create_repo(&env, &name).await;
    let moved = tag("redirect-moved");
    let (status, _) = api(
        &env.app,
        "PATCH",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        Some(json!({"name": moved})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(redirect_rows(&env, &name).await, 1, "fixture redirect");

    // Restoring the original retires the redirect in the same transaction.
    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(original));
    assert_eq!(
        redirect_rows(&env, &name).await,
        0,
        "the restore left a redirect claiming the restored name"
    );

    // Delete it again: the path is 410, not the renamed repository.
    delete_repo(&env, &name).await;
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"], "repository is gone");
    let stderr = git_fail(git_cmd(&["ls-remote", &env.git_url(&name)])).await;
    assert!(stderr.contains("410"), "{stderr}");
    assert_eq!(redirect_rows(&env, &name).await, 0);
}

// ---------------------------------------------------------------------------
// `restoring` is a gone state on every surface
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restoring_paths_are_410_on_api_web_and_git_http() {
    let env = setup_with(false).await;
    let name = tag("restoring");
    let id = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;
    set_status(&env, id, "restoring").await;

    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["error"], "repository is gone");

    let cookie = format!("githearth_session={}", env.owner_token);
    let (status, html) = page(&env.app, &env.web_path(&name), Some(&cookie)).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(html.contains("already in progress"), "{html}");

    let stderr = git_fail(git_cmd(&["ls-remote", &env.git_url(&name)])).await;
    assert!(stderr.contains("410"), "{stderr}");

    // A sub-page answers the same way.
    let (status, _) = page(
        &env.app,
        &format!("{}/issues", env.web_path(&name)),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

// ---------------------------------------------------------------------------
// Crash recovery for a `restoring` row
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reconcile_finishes_or_reverts_a_crashed_restore() {
    let env = setup_with(false).await;

    // A: crashed before the move — the trash entry is intact.
    let a_name = tag("crash-before");
    let a = create_repo(&env, &a_name).await;
    delete_repo(&env, &a_name).await;
    set_status(&env, a, "restoring").await;

    // B: crashed after the move — the directory is already back.
    let b_name = tag("crash-after");
    let b = create_repo(&env, &b_name).await;
    delete_repo(&env, &b_name).await;
    // Mark `restoring` before the manual move: while the row is `deleted` the
    // orphan sweep may treat a directory in `repositories/` as an orphan.
    set_status(&env, b, "restoring").await;
    let b_entry = env
        .state
        .storage
        .find_trash(b)
        .unwrap()
        .expect("trash entry");
    env.state.storage.restore(b, &b_entry.path).unwrap();

    // C: the purge won — the trash entry is gone, and the row must go back to
    // `deleted` with its original `deleted_at`.
    let c_name = tag("crash-purged");
    let c = create_repo(&env, &c_name).await;
    delete_repo(&env, &c_name).await;
    let c_deleted_at: Option<String> =
        sqlx::query_scalar("SELECT deleted_at::text FROM repositories WHERE id=$1")
            .bind(c)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    let c_entry = env
        .state
        .storage
        .find_trash(c)
        .unwrap()
        .expect("trash entry");
    std::fs::remove_dir_all(&c_entry.path).unwrap();
    set_status(&env, c, "restoring").await;

    // Drive the reconciler's per-repository step directly: other suites share
    // this database and run full passes over their own storage roots, so a
    // backdated row here would also be seen (and acted on) by them.
    assert_eq!(
        reconcile::recover_restore(&env.state, a).await.unwrap(),
        reconcile::RestoreOutcome::Finished
    );
    assert_eq!(
        reconcile::recover_restore(&env.state, b).await.unwrap(),
        reconcile::RestoreOutcome::Finished
    );
    assert_eq!(
        reconcile::recover_restore(&env.state, c).await.unwrap(),
        reconcile::RestoreOutcome::Reverted
    );

    assert_eq!(row_status(&env, a).await, "active");
    assert!(env.state.storage.has_repository(a));
    assert!(env.state.storage.find_trash(a).unwrap().is_none());

    assert_eq!(row_status(&env, b).await, "active");
    assert!(env.state.storage.has_repository(b));

    assert_eq!(row_status(&env, c).await, "deleted");
    let after_deleted_at: Option<String> =
        sqlx::query_scalar("SELECT deleted_at::text FROM repositories WHERE id=$1")
            .bind(c)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(
        after_deleted_at, c_deleted_at,
        "a reverted restore moved deleted_at forward"
    );

    // Each recovered restore is audited exactly once.
    let restored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind='repository.restored' AND organization_id=$1",
    )
    .bind(env.org_id)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(restored, 2);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn suspended_organization_refuses_restore_for_owner_and_site_admin() {
    let env = setup_with(false).await;
    let name = tag("suspended");
    create_repo(&env, &name).await;

    let admin_name = tag("suspended-site-admin");
    let admin = fixture_account(&env.pool, &admin_name, true).await;
    let admin_token = fixture_session(&env.pool, admin).await;

    delete_repo(&env, &name).await;
    sqlx::query("UPDATE organizations SET suspended=true WHERE id=$1")
        .bind(env.org_id)
        .execute(&env.pool)
        .await
        .unwrap();

    for (who, token) in [("owner", &env.owner_token), ("site admin", &admin_token)] {
        let (status, body) = restore_repo(&env, &name, token, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{who} restored inside a suspended organization: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// Cross-process lock (B1)
// ---------------------------------------------------------------------------

/// Child-process half of [`a_purge_waits_for_a_lock_held_by_another_process`].
///
/// It is an ordinary test so the harness can select it by name; when the
/// parent's environment is absent it does nothing. Readiness is signalled by
/// creating a file, not by stdout, so a closed pipe can never fail the child.
#[tokio::test]
#[ignore = "helper process; driven by a_purge_waits_for_a_lock_held_by_another_process"]
async fn repository_lock_holder_process() {
    let Ok(root) = std::env::var("GITHEARTH_B4_LOCK_ROOT") else {
        return;
    };
    let id: Uuid = std::env::var("GITHEARTH_B4_LOCK_ID")
        .expect("lock id")
        .parse()
        .unwrap();
    let ready = std::env::var("GITHEARTH_B4_LOCK_READY").expect("ready path");
    let storage = githearth_git::layout::Storage::open(&root).unwrap();
    let guard = storage.lock_repository(id).await.unwrap();
    std::fs::write(&ready, b"locked").unwrap();
    // Hold until the parent closes our stdin, then release.
    let mut line = String::new();
    let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line);
    drop(guard);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_purge_waits_for_a_lock_held_by_another_process() {
    use std::process::Stdio;

    let env = setup_with(false).await;
    let name = tag("cross-process");
    let id = create_repo(&env, &name).await;
    delete_repo(&env, &name).await;
    backdate_trash(&env, id);

    let ready = std::env::temp_dir().join(format!("githearth-b4-lock-{}", Uuid::new_v4()));

    // A genuine second process takes the repository lock.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository_lock_holder_process",
            "--ignored",
            "--test-threads=1",
        ])
        .env("GITHEARTH_B4_LOCK_ROOT", &env.storage_root)
        .env("GITHEARTH_B4_LOCK_ID", id.to_string())
        .env("GITHEARTH_B4_LOCK_READY", &ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn lock holder");

    // Wait until the child actually holds the lock.
    let mut held = false;
    for _ in 0..600 {
        if ready.exists() {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held, "the second process never took the lock");

    // Run the purge. With the inter-process lock it must block on the flock and
    // remove nothing while the other process holds it.
    let state = env.state.clone();
    let mut purger = tokio::spawn(async move {
        let mut attempts = HashMap::new();
        reconcile::reconcile_once(&state, &mut attempts)
            .await
            .unwrap()
    });
    let early = tokio::time::timeout(Duration::from_secs(5), &mut purger).await;
    assert!(
        early.is_err(),
        "the purge finished while another process held the lock: {:?}",
        early.map(|joined| joined.unwrap())
    );
    assert!(
        env.state.storage.find_trash(id).unwrap().is_some(),
        "the purge removed an entry another process held the lock for"
    );

    // Release: closing stdin makes the child exit and drop its guard.
    drop(child.stdin.take());
    let exited = tokio::task::spawn_blocking(move || child.wait().unwrap())
        .await
        .unwrap();
    assert!(exited.success(), "lock holder failed");
    let _ = std::fs::remove_file(&ready);

    let summary = tokio::time::timeout(Duration::from_secs(30), purger)
        .await
        .expect("the purge did not resume after the lock was released")
        .unwrap();
    assert_eq!(summary.errors, 0, "{summary:?}");
    assert_eq!(summary.trash_purged, 1, "{summary:?}");
    assert!(env.state.storage.find_trash(id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Cross-slice: the transfer slice's tombstone (M4)
// ---------------------------------------------------------------------------

/// Shape of the check that must run **after `m41/transfer` merges**: a name
/// transferred out of an organization leaves a tombstone; a deleted duplicate
/// at that name must neither take the name back (restore refused) nor delete
/// the tombstone, and the old path must not resolve to the moved repository.
///
/// It cannot run on this branch: `repository_redirects.transferred_at` is
/// added by the transfer slice, so it self-skips until that column exists.
#[tokio::test]
#[ignore = "cross-slice: needs m41/transfer's repository_redirects.transferred_at; run after that slice merges"]
async fn transferred_name_tombstone_survives_a_duplicate_restore() {
    let env = setup_with(false).await;
    let has_column: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns \
         WHERE table_name='repository_redirects' AND column_name='transferred_at')",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    if !has_column {
        eprintln!("SKIP: repository_redirects.transferred_at does not exist yet");
        return;
    }

    // A repository that moved to another organization: a tombstone at `name`
    // in this organization points at a row owned by the other one.
    let name = tag("transferred");
    let other_org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Other Org')")
        .bind(other_org)
        .bind(tag("other-org"))
        .execute(&env.pool)
        .await
        .unwrap();
    let moved = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,operation_id,created_by) \
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(moved)
    .bind(other_org)
    .bind(&name)
    .bind(Uuid::new_v4())
    .bind(env.owner)
    .execute(&env.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO repository_redirects (organization_id,name,repository_id,transferred_at) \
         VALUES ($1,$2,$3,now())",
    )
    .bind(env.org_id)
    .bind(&name)
    .bind(moved)
    .execute(&env.pool)
    .await
    .unwrap();

    // A deleted duplicate at the same name in the source organization.
    let duplicate = create_repo(&env, &name).await;
    assert_ne!(duplicate, moved);
    delete_repo(&env, &name).await;

    // Restore must be refused while the tombstone holds the name.
    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "repository name already taken");
    assert_eq!(row_status(&env, duplicate).await, "deleted");

    // The tombstone is intact (it is not a rename redirect to retire).
    let tombstones: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repository_redirects \
         WHERE organization_id=$1 AND lower(name)=lower($2) AND transferred_at IS NOT NULL",
    )
    .bind(env.org_id)
    .bind(&name)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(tombstones, 1, "the restore deleted a transfer tombstone");

    // The old path must not resolve to the moved repository. The transfer
    // slice owns the exact `410`; this asserts the dangerous outcome (200
    // resolving to the repository in another organization) cannot happen.
    let (status, body) = api(
        &env.app,
        "GET",
        &env.api_path(&format!("/{name}")),
        Some(&env.owner_token),
        None,
        None,
    )
    .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_ne!(body["id"], json!(moved));
}

// ---------------------------------------------------------------------------
// SSH transport
// ---------------------------------------------------------------------------

fn has_ssh() -> bool {
    Command::new("ssh").arg("-V").output().is_ok()
        && Command::new("ssh-keygen").arg("-V").output().is_ok()
}

fn ssh_generate_key(path: &Path) {
    let _ = std::fs::remove_file(path);
    let out = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "restore-test",
            "-q",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ssh_fingerprint(path: &Path) -> String {
    let out = Command::new("ssh-keygen")
        .args(["-lf", &path.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database and ssh binaries via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_old_path_is_gone_then_resolves_after_restore() {
    if !has_ssh() {
        eprintln!("SKIP: ssh or ssh-keygen is not installed");
        return;
    }
    let env = setup_with(true).await;
    let name = tag("ssh");
    let id = create_repo(&env, &name).await;
    let (main, _feature) = seed_history(&env, id).await;

    let key_dir = std::env::temp_dir().join(format!("restore-ssh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_generate_key(&client_key);
    let fingerprint = ssh_fingerprint(&client_key);

    let account = env.owner;
    let authenticate: ssh::Authenticate = Arc::new(move |supplied: String| {
        let result = if supplied == fingerprint {
            Ok(Some(ssh::SshIdentity {
                account,
                key_id: Uuid::new_v4(),
                read_only: false,
            }))
        } else {
            Ok(None)
        };
        Box::pin(async move { result })
    });
    let (_ssh_handle, port_rx) = ssh::spawn(env.state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();
    let ssh_cmd = format!(
        "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        client_key.display()
    );
    let ssh_url = format!(
        "ssh://git@127.0.0.1:{ssh_port}/{}/{}.git",
        env.org_slug, name
    );

    delete_repo(&env, &name).await;

    let mut gone = git_cmd(&["ls-remote", &ssh_url]);
    gone.env("GIT_SSH_COMMAND", &ssh_cmd);
    let stderr = git_fail(gone).await;
    assert!(
        stderr.contains("repository is gone"),
        "SSH did not report the gone path: {stderr}"
    );

    // `restoring` is the same gone state over SSH.
    set_status(&env, id, "restoring").await;
    let mut during = git_cmd(&["ls-remote", &ssh_url]);
    during.env("GIT_SSH_COMMAND", &ssh_cmd);
    let stderr = git_fail(during).await;
    assert!(
        stderr.contains("repository is gone"),
        "SSH did not report a restoring path as gone: {stderr}"
    );
    set_status(&env, id, "deleted").await;

    let (status, body) = restore_repo(&env, &name, &env.owner_token, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut resolved = git_cmd(&["ls-remote", &ssh_url]);
    resolved.env("GIT_SSH_COMMAND", &ssh_cmd);
    let refs = git_ok(resolved).await;
    assert!(refs.contains(&main), "SSH path did not resolve: {refs}");

    let _ = std::fs::remove_dir_all(&key_dir);
}
