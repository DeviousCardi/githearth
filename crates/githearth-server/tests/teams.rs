//! Teams with repository grants (M4.1 acceptance case U05).
//!
//! A team carries a grant and a team member admits through it on the API, the
//! browser, Git over HTTP and Git over SSH; removing the grant or the member,
//! or deleting the team, denies the next admission including for an
//! already-issued token and SSH key; a team's organization owner grant counts
//! toward the last-owner rule; and every refusal in the owner-only `team.*`
//! group is masked for a caller who may read the organization.
//!
//! The fixtures write PostgreSQL directly because they are the tests' setup;
//! every assertion in the acceptance path goes through the router, the Git
//! admission path or the SSH transport, never through SQL.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_git::layout::ObjectFormat;
use githearth_server::{
    AppState, MIGRATOR, config::Config, connect, git_admission, hash_password, router, ssh,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: Option<&Value>,
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
    let body = body.map_or_else(String::new, ToString::to_string);
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
        .map(|(k, v)| {
            // `aggressive` encoding, which is what a browser sends and the only
            // form `application/x-www-form-urlencoded` parses back: a literal
            // `+` (correctly sent as `%2B`) would otherwise be read as a space.
            let shelf = if v.contains('%') {
                v.clone()
            } else {
                urlencode(v)
            };
            format!("{k}={shelf}")
        })
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

async fn git_fails(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        !out.status.success(),
        "expected git to fail, it succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..12])
}

/// The storage root also carries the admission Unix socket, whose whole path
/// must stay under 107 bytes: keep it short.
fn short_temp_dir(label: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("ght-{label}-{}", &id[..10]))
}

fn b64(input: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(input)
}

struct User {
    id: Uuid,
    username: String,
    token: String,
    password: String,
}

impl User {
    fn cookie(&self) -> String {
        format!("githearth_session={}", self.token)
    }
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    let password = "correct-horse-battery".to_owned();
    sqlx::query("INSERT INTO accounts (id,username,password_hash,email,email_verified_at) VALUES ($1,$2,$3,$4,now())")
        .bind(id)
        .bind(&username)
        .bind(hash_password(&password).unwrap())
        .bind(format!("{username}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    add_session(pool, id).await
}

/// A signed-in session for an account, as `user` creates it. Lets a test build
/// the account row itself (for example without an email) and still sign in.
async fn add_session(pool: &PgPool, id: Uuid) -> User {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    User {
        id,
        username,
        token,
        password: "correct-horse-battery".to_owned(),
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

async fn seed_repo(state: &AppState, repo_id: Uuid) -> PathBuf {
    let path = state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let work = std::env::temp_dir().join(format!("ght-seed-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Teams\n").unwrap();
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
    git_ok(git_cmd(&[
        "--git-dir",
        path.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        tw,
        "HEAD:refs/heads/main",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);
    path
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    org_id: Uuid,
    org_slug: String,
    repo_name: String,
    owner: User,
    member: User,
    outsider: User,
}

async fn fixture() -> Fixture {
    let storage_root = short_temp_dir("teams");
    std::fs::create_dir_all(&storage_root).unwrap();
    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        git_timeout_seconds: 60,
        ssh_enabled: true,
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = user(&pool, "owner").await;
    let member = user(&pool, "member").await;
    let outsider = user(&pool, "outsider").await;
    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Teams Fixture')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    add_membership(&pool, org_id, owner.id, "owner").await;
    add_membership(&pool, org_id, member.id, "member").await;

    // A second organization for the outsider, so a denial is "no access" and
    // not "no account": reads of it prove the outsider is a working caller.
    let outsider_org_id = Uuid::new_v4();
    let outsider_slug = tag("foreign");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Elsewhere')")
        .bind(outsider_org_id)
        .bind(&outsider_slug)
        .execute(&pool)
        .await
        .unwrap();
    add_membership(&pool, outsider_org_id, outsider.id, "owner").await;

    let repo_name = tag("repo");
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,description,visibility,created_by,operation_id,status) VALUES ($1,$2,$3,'','private',$4,$5,'active')")
        .bind(repo_id)
        .bind(org_id)
        .bind(&repo_name)
        .bind(owner.id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let _ = seed_repo(&state, repo_id).await;
    let app = router(state.clone());
    Fixture {
        app,
        state,
        pool,
        storage_root,
        org_id,
        org_slug,
        repo_name,
        owner,
        member,
        outsider,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

fn teams_path(f: &Fixture) -> String {
    format!("/api/v1/organizations/{}/teams", f.org_slug)
}

fn team_path(f: &Fixture, team: &str) -> String {
    format!("/api/v1/organizations/{}/teams/{team}", f.org_slug)
}

async fn serve(app: &Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

/// The Git admission decision for one caller, through the real admission path
/// (the same one Git HTTP and SSH use): `Ok` admits, `Err` is the refusal.
async fn admit(
    f: &Fixture,
    account: &User,
    want: git_admission::Want,
) -> Result<(), (StatusCode, &'static str)> {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!(
            "Basic {}",
            b64(format!("{}:{}", account.username, account.password).as_bytes())
        )
        .parse()
        .unwrap(),
    );
    git_admission::admit(&f.state, &headers, &f.org_slug, &f.repo_name, want)
        .await
        .map(|_| ())
        .map_err(|e| (e.0, e.1))
}

/// A test SSH client authenticated as `account`.
struct SshClient {
    key_dir: PathBuf,
    command: String,
    url: String,
    _server: tokio::task::JoinHandle<()>,
}

impl SshClient {
    fn ls_remote(&self) -> Command {
        let mut cmd = git_cmd(&["ls-remote", &self.url]);
        cmd.env("GIT_SSH_COMMAND", &self.command);
        cmd
    }
}

impl Drop for SshClient {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.key_dir);
    }
}

async fn ssh_client(f: &Fixture, account: Uuid) -> SshClient {
    assert!(
        Command::new("ssh").arg("-V").output().is_ok(),
        "the SSH transport tests require the `ssh` binary"
    );
    let key_dir = std::env::temp_dir().join(format!("ght-ssh-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let generated = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-C",
            "teams-test",
            "-q",
        ])
        .output()
        .unwrap();
    assert!(generated.status.success());
    let fingerprint = String::from_utf8_lossy(
        &Command::new("ssh-keygen")
            .args(["-lf", &client_key.to_string_lossy(), "-E", "sha256"])
            .output()
            .unwrap()
            .stdout,
    )
    .split_whitespace()
    .nth(1)
    .unwrap()
    .to_owned();
    let authenticate: ssh::Authenticate = Arc::new(move |fp: String| {
        let identity = (fp == fingerprint).then_some(ssh::SshIdentity {
            account,
            service_account: None,
            key_id: Uuid::new_v4(),
            read_only: false,
        });
        Box::pin(async move { Ok(identity) })
    });
    let (server, port_rx) = ssh::spawn(f.state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();
    SshClient {
        key_dir,
        command: format!(
            "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
            client_key.display()
        ),
        url: format!(
            "ssh://git@127.0.0.1:{ssh_port}/{}/{}.git",
            f.org_slug, f.repo_name
        ),
        _server: server,
    }
}

/// Create a team through the API as the owner and return its slug.
async fn create_team(f: &Fixture, slug: &str) -> Uuid {
    let (status, body) = api(
        &f.app,
        "POST",
        &teams_path(f),
        Some(&f.owner.token),
        None,
        Some(&json!({"slug": slug, "name": "Platform"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

/// Grant the team `role` on the fixture repository, as the owner, and return
/// the grant id.
async fn grant_repo(f: &Fixture, team: &str, role: &str) -> Uuid {
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/grants", team_path(f, team)),
        Some(&f.owner.token),
        None,
        Some(&json!({"repository": f.repo_name, "role": role})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn add_team_member(f: &Fixture, team: &str, username: &str) -> StatusCode {
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/members", team_path(f, team)),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": username})),
    )
    .await;
    assert!(body.get("error").is_none() || status.is_success(), "{body}");
    status
}

async fn repo_api(f: &Fixture) -> String {
    format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, f.repo_name
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn owner_creates_and_deletes_a_team_and_sees_it_audited() {
    let f = fixture().await;
    let slug = tag("platform");
    let id = create_team(&f, &slug).await;
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_created").await,
        1
    );

    let (status, body) = api(
        &f.app,
        "GET",
        &teams_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array().unwrap().iter().any(|t| t["slug"] == slug),
        "{body}"
    );

    // A duplicate slug in the organization is a conflict.
    let (status, body) = api(
        &f.app,
        "POST",
        &teams_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"slug": slug, "name": "Again"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The members page and the teams page render for the owner.
    let cookie = f.owner.cookie();
    let (status, page) = get_page(
        &f.app,
        &format!("/organizations/{}/teams", f.org_slug),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains(&slug), "{page}");

    // The organization page links the teams surface for an owner only.
    let (_, org_page) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&cookie),
    )
    .await;
    assert!(org_page.contains(&format!("/organizations/{}/teams", f.org_slug)));
    let (_, member_page) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&f.member.cookie()),
    )
    .await;
    assert!(!member_page.contains(&format!("/organizations/{}/teams", f.org_slug)));

    // Delete through the browser form, with the session's CSRF token.
    let (status, page) = get_page(
        &f.app,
        &format!("/organizations/{}/teams/{}", f.org_slug, slug),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let csrf = hidden(&page, "csrf_token");
    let key = Uuid::new_v4().simple().to_string();
    let (status, _) = post_form(
        &f.app,
        &format!("/organizations/{}/teams/{}/delete", f.org_slug, slug),
        &cookie,
        &[
            ("csrf_token", csrf.clone()),
            ("idempotency_key", key.clone()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_deleted").await,
        1
    );
    let (status, _) = api(
        &f.app,
        "DELETE",
        &team_path(&f, &slug),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams WHERE id = $1)")
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!exists);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn team_grant_authorizes_a_push_and_an_api_read_until_removed() {
    let f = fixture().await;
    let team = tag("devs");
    create_team(&f, &team).await;
    // Without a team or a direct grant the member reads nothing.
    let (status, _) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        admit(&f, &f.member, git_admission::Want::Fetch)
            .await
            .is_err()
    );

    grant_repo(&f, &team, "write").await;
    // The grant alone admits nobody who is not in the team.
    let (status, _) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        add_team_member(&f, &team, &f.member.username).await,
        StatusCode::CREATED
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_member_added").await,
        1
    );

    // API read and browser read through the team's grant.
    let (status, body) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A push: the admission path admits `receive-pack` and a branch update.
    admit(&f, &f.member, git_admission::Want::Push)
        .await
        .unwrap();

    // Revoke the grant through the API: the very next admission refuses, as
    // an existing token would be denied on its next request.
    let grant = grant_repo(&f, &team, "write").await;
    let (status, _) = api(
        &f.app,
        "DELETE",
        &format!("{}/grants/{grant}", team_path(&f, &team)),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_grant_revoked").await,
        1
    );
    let (status, _) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, message) = admit(&f, &f.member, git_admission::Want::Push)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND, "{message}");

    // Removing the member (with the grant active again) denies the same way.
    grant_repo(&f, &team, "write").await;
    admit(&f, &f.member, git_admission::Want::Fetch)
        .await
        .unwrap();
    let (status, _) = api(
        &f.app,
        "DELETE",
        &format!("{}/members/{}", team_path(&f, &team), f.member.username),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_member_removed").await,
        1
    );
    assert!(
        admit(&f, &f.member, git_admission::Want::Fetch)
            .await
            .is_err()
    );

    // Deleting the team denies its (re-added) member too.
    add_team_member(&f, &team, &f.member.username).await;
    admit(&f, &f.member, git_admission::Want::Fetch)
        .await
        .unwrap();
    let (status, _) = api(
        &f.app,
        "DELETE",
        &team_path(&f, &team),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        admit(&f, &f.member, git_admission::Want::Fetch)
            .await
            .is_err()
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn revoking_a_team_grant_denies_an_existing_token_and_an_existing_ssh_key() {
    let f = fixture().await;
    let team = tag("devs");
    create_team(&f, &team).await;
    grant_repo(&f, &team, "write").await;
    add_team_member(&f, &team, &f.member.username).await;

    // A real clone over HTTP with the member's session token as Basic auth,
    // then a push of a new branch from that clone.
    let port = serve(&f.app).await;
    let token_url = format!(
        "http://{}:{}@127.0.0.1:{port}/{}/{}.git",
        f.member.username, f.member.password, f.org_slug, f.repo_name
    );
    let clone = std::env::temp_dir().join(format!("ght-clone-{}", Uuid::new_v4().simple()));
    git_ok(git_cmd(&[
        "clone",
        "--",
        &token_url,
        clone.to_str().unwrap(),
    ]))
    .await;
    let cw = clone.to_str().unwrap();
    std::fs::write(clone.join("work.txt"), "work\n").unwrap();
    git_ok(git_cmd(&["-C", cw, "checkout", "-b", "feature"])).await;
    git_ok(git_cmd(&["-C", cw, "add", "work.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        cw,
        "-c",
        "user.name=Member",
        "-c",
        "user.email=m@example.com",
        "commit",
        "-m",
        "work",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cw, "push", "origin", "feature"])).await;
    // The server deleted the branch with a zero object ID: a branch that is
    // gone afterwards still reads as "permission denied" to Git, which hides a
    // real failure, so finish the push the way the protocol expects.
    git_ok(git_cmd(&[
        "-C",
        cw,
        "-c",
        "user.name=Member",
        "-c",
        "user.email=m@example.com",
        "commit",
        "--allow-empty",
        "-m",
        "done",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        cw,
        "push",
        "origin",
        "HEAD:refs/heads/feature",
        ":refs/heads/0000000000000000000000000000000000000000",
    ]))
    .await;

    // An SSH key that works before the revocation.
    let ssh_client = ssh_client(&f, f.member.id).await;
    git_ok(ssh_client.ls_remote()).await;

    // Revoke the team grant: the same token and the same key are refused on
    // their next admission, with no re-login and no key change.
    let grant = grant_repo(&f, &team, "write").await;
    let (status, _) = api(
        &f.app,
        "DELETE",
        &format!("{}/grants/{grant}", team_path(&f, &team)),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    git_ok(git_cmd(&["-C", cw, "checkout", "-b", "other"])).await;
    std::fs::write(clone.join("other.txt"), "other\n").unwrap();
    git_ok(git_cmd(&["-C", cw, "add", "other.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        cw,
        "-c",
        "user.name=Member",
        "-c",
        "user.email=m@example.com",
        "commit",
        "-m",
        "other",
    ]))
    .await;
    let stderr = git_fails(git_cmd(&["-C", cw, "push", "origin", "other"])).await;
    assert!(
        stderr.contains("not found") || stderr.contains("403") || stderr.contains("not permitted"),
        "unexpected HTTP refusal: {stderr}"
    );
    let stderr = git_fails(ssh_client.ls_remote()).await;
    assert!(
        stderr.contains("repository not found"),
        "unexpected SSH refusal: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&clone);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_team_owner_grant_counts_for_the_last_owner_rule() {
    let f = fixture().await;
    let demote = |username: String| {
        let path = format!("/api/v1/organizations/{}/members/{}", f.org_slug, username);
        let token = f.owner.token.clone();
        let app = f.app.clone();
        async move {
            let (status, body) = api(
                &app,
                "PATCH",
                &path,
                Some(&token),
                None,
                Some(&json!({"role": "member"})),
            )
            .await;
            (status, body)
        }
    };

    // Before any team: the owner's own demotion is refused as the last owner.
    let (status, body) = demote(f.owner.username.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");

    let team = tag("owners");
    create_team(&f, &team).await;
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/grants", team_path(&f, &team)),
        Some(&f.owner.token),
        None,
        Some(&json!({"on_organization": true, "role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // The team grant has no members yet, so it is not a reachable owner:
    // demoting the sole account owner would strand the organization.
    let (status, body) = demote(f.owner.username.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");
    assert_eq!(
        add_team_member(&f, &team, &f.member.username).await,
        StatusCode::CREATED
    );

    // The same demotion now succeeds, because the organization still has an
    // owner: the team's owner grant. Counting only account grants would refuse
    // it, so this is the assertion that fails if a team grant is left out.
    let (status, body) = demote(f.owner.username.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The member is an owner through the team.
    let (status, _) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Now the team holds the only owner grant. Revoking it, deleting the team
    // and removing its last member are all refused.
    let token = f.member.token.clone();
    let (status, body) = api(
        &f.app,
        "GET",
        &format!("{}/grants", team_path(&f, &team)),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let grant = body
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["role"] == "owner")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = api(
        &f.app,
        "DELETE",
        &format!("{}/grants/{grant}", team_path(&f, &team)),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");
    let (status, body) = api(
        &f.app,
        "DELETE",
        &team_path(&f, &team),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = api(
        &f.app,
        "DELETE",
        &format!("{}/members/{}", team_path(&f, &team), f.member.username),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Making the demoted owner an owner again leaves two owner grants, so the
    // revocation is allowed: the rule is a count, not a special case.
    let (status, _) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.owner.username
        ),
        Some(&token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        &f.app,
        "DELETE",
        &format!("{}/grants/{grant}", team_path(&f, &team)),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    finish(f).await;
}

/// Make `team` hold the organization's only owner grant with `f.member` as its
/// only member, then demote the account owner so the team is the sole route to
/// the owner actions.
async fn hand_the_organization_to_a_team(f: &Fixture, team: &str) {
    create_team(f, team).await;
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/grants", team_path(f, team)),
        Some(&f.owner.token),
        None,
        Some(&json!({"on_organization": true, "role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        add_team_member(f, team, &f.member.username).await,
        StatusCode::CREATED
    );
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.owner.username
        ),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_teams_only_member_cannot_leave_while_the_team_is_the_only_reachable_owner() {
    let f = fixture().await;
    let team = tag("owners");
    hand_the_organization_to_a_team(&f, &team).await;

    let self_leave = |token: String| {
        let path = format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.member.username
        );
        let app = f.app.clone();
        async move { api(&app, "DELETE", &path, Some(&token), None, None).await }
    };

    // The team's only member cannot leave: the team's owner grant would be
    // left active but unreachable.
    let (status, body) = self_leave(f.member.token.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");
    let still_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
    )
    .bind(f.org_id)
    .bind(f.member.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        still_member,
        "the refused leave must not remove the membership"
    );
    // And the member still exercises the team's owner grant.
    let (status, _) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // With a second reachable owner the same leave succeeds.
    let second = user(&f.pool, "second-owner").await;
    add_membership(&f.pool, f.org_id, second.id, "owner").await;
    let (status, body) = self_leave(f.member.token.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let still_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
    )
    .bind(f.org_id)
    .bind(f.member.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(!still_member);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_teams_only_member_cannot_be_removed_while_the_team_is_the_only_reachable_owner() {
    let f = fixture().await;
    let team = tag("owners");
    hand_the_organization_to_a_team(&f, &team).await;

    let team_remove = |token: String| {
        let path = format!("{}/members/{}", team_path(&f, &team), f.member.username);
        let app = f.app.clone();
        async move { api(&app, "DELETE", &path, Some(&token), None, None).await }
    };

    // Removing the member through the team handler is refused for the same
    // reason: the team would keep its owner grant with nobody to exercise it.
    let (status, body) = team_remove(f.member.token.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");

    // With a second reachable owner the same removal succeeds.
    let second = user(&f.pool, "second-owner").await;
    add_membership(&f.pool, f.org_id, second.id, "owner").await;
    let (status, body) = team_remove(f.member.token.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let membership: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
    )
    .bind(f.org_id)
    .bind(f.member.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        membership,
        "team removal must not remove the org membership"
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_caller_without_team_authority_is_refused_without_learning_the_team_exists() {
    let f = fixture().await;
    let team = tag("devs");
    create_team(&f, &team).await;

    // A plain member of the organization: forbidden (403), not told whether a
    // team by that name exists. Adding a member — themselves included — is an
    // update the member does not hold. Every call is tried against an existing
    // team and a missing one.
    for (method, path, body) in [
        (
            "POST",
            teams_path(&f),
            Some(json!({"slug": "x", "name": "X"})),
        ),
        (
            "POST",
            format!("{}/members", team_path(&f, &team)),
            Some(json!({"username": f.owner.username})),
        ),
        (
            "POST",
            format!("{}/grants", team_path(&f, &team)),
            Some(json!({"repository": f.repo_name, "role": "write"})),
        ),
        (
            "DELETE",
            format!("{}/members/x", team_path(&f, &team)),
            None,
        ),
        (
            "DELETE",
            format!("{}/members/x", team_path(&f, "no-such-team")),
            None,
        ),
    ] {
        let (status, body) = api(
            &f.app,
            method,
            &path,
            Some(&f.member.token),
            None,
            body.as_ref(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }
    // The same member's join (adding themselves) is the same 403 whether the
    // team exists or not: the action is checked before the team is looked up,
    // so a refusal never tells the two apart.
    for name in [team.clone(), "no-such-team".to_owned()] {
        let (status, body) = api(
            &f.app,
            "POST",
            &format!("{}/members", team_path(&f, &name)),
            Some(&f.member.token),
            None,
            Some(&json!({"username": f.member.username})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {body}");
    }
    let (status, _) = api(
        &f.app,
        "GET",
        &format!("{}/members", team_path(&f, &team)),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/teams", f.org_slug),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/teams/{}", f.org_slug, team),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An outsider to the organization: the same 404 a missing organization
    // gives, and a team name is not distinguishable from a missing one.
    for path in [
        teams_path(&f),
        format!("{}/members", team_path(&f, &team)),
        format!("{}/grants", team_path(&f, &team)),
        format!("{}/members", team_path(&f, "no-such-team")),
    ] {
        let (status, body) = api(&f.app, "GET", &path, Some(&f.outsider.token), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    // A delete names the team either way: both are the organization's 404.
    for name in [team.clone(), "no-such-team".to_owned()] {
        let (status, body) = api(
            &f.app,
            "DELETE",
            &team_path(&f, &name),
            Some(&f.outsider.token),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: {body}");
    }
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/teams/{}", f.org_slug, team),
        Some(&f.outsider.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An account that cannot administer membership cannot add a team.
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/members", team_path(&f, &team)),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": f.outsider.username})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_plain_member_gets_the_403_page_for_every_team_form_with_a_valid_csrf_token() {
    let f = fixture().await;
    let cookie = f.member.cookie();
    // A team with a grant, created by the owner, then every form as the member.
    let team = tag("team-form");
    create_team(&f, &team).await;
    let grant = grant_repo(&f, &team, "read").await;

    // The member's organization page renders and carries the session's CSRF
    // token, so each form below is a valid form, not the CSRF-failure case.
    let (status, org_page) = get_page(
        &f.app,
        &format!("/organizations/{}", f.org_slug),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{org_page}");
    let csrf = hidden(&org_page, "csrf_token");

    // One form for every team control. A team the member may not administer
    // gives either the "not permitted" page (the organization is readable) or
    // the masked 404; never a write.
    type Form = (&'static str, String, Vec<(&'static str, String)>);
    let forms: [Form; 6] = [
        (
            "create",
            format!("/organizations/{}/teams", f.org_slug),
            vec![("slug", "member-team".to_owned()), ("name", "M".to_owned())],
        ),
        (
            "delete team",
            format!("/organizations/{}/teams/{}/delete", f.org_slug, team),
            vec![],
        ),
        (
            "add a member",
            format!("/organizations/{}/teams/{}", f.org_slug, team),
            vec![("username", f.owner.username.clone())],
        ),
        (
            "remove a member",
            format!(
                "/organizations/{}/teams/{}/members/{}/remove",
                f.org_slug, team, f.member.username
            ),
            vec![],
        ),
        (
            "grant",
            format!("/organizations/{}/teams/{}/grants", f.org_slug, team),
            vec![
                ("repository", f.repo_name.clone()),
                ("role", "read".to_owned()),
            ],
        ),
        (
            "revoke a grant",
            format!(
                "/organizations/{}/teams/{}/grants/{grant}/revoke",
                f.org_slug, team
            ),
            vec![],
        ),
    ];
    for (label, path, fields) in forms {
        let mut form: Vec<(&str, String)> = vec![
            ("csrf_token", csrf.clone()),
            ("idempotency_key", Uuid::new_v4().simple().to_string()),
        ];
        form.extend(fields);
        let (status, body) = post_form(&f.app, &path, &cookie, &form).await;
        // The hidden inputs carry the session's CSRF token and a fresh key, so a
        // 403 here is the authorization refusal, not CSRF. Every form refuses:
        // a reader of the organization gets the 403 page, and a missing team
        // gets either that page or the masked 404 — the same for any name, so
        // neither discloses which teams exist. Never a 201, a 303 or a 400.
        match status {
            StatusCode::FORBIDDEN => assert!(
                body.contains("Only an organization owner can administer teams"),
                "{label}: {body}"
            ),
            StatusCode::NOT_FOUND => assert!(!body.contains("SECRET"), "{label}: {body}"),
            other => panic!("{label}: expected 403 or 404, got {other}: {body}"),
        }
    }

    // Nothing was written: no second team, no team member, no grant change and
    // no delete.
    let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM teams WHERE organization_id = $1")
        .bind(f.org_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(teams, 1);
    let members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM team_members tm JOIN teams t ON t.id = tm.team_id \
         WHERE t.organization_id = $1 AND t.slug = $2",
    )
    .bind(f.org_id)
    .bind(&team)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(members, 0);
    let grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants g JOIN teams t ON t.id = g.subject_id \
         WHERE t.organization_id = $1 AND t.slug = $2 AND g.revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(&team)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(grants, 1);
    for kind in [
        "organization.team_deleted",
        "organization.team_member_added",
        "organization.team_member_removed",
        "organization.team_grant_revoked",
    ] {
        assert_eq!(audit_kinds(&f.pool, f.org_id, kind).await, 0, "{kind}");
    }
    // Exactly the owner's one team and its one grant: none of the forms above
    // added a second.
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_created").await,
        1
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_grant_added").await,
        1
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_team_form_without_the_session_csrf_token_writes_nothing() {
    let f = fixture().await;
    let cookie = f.owner.cookie();
    let (status, page) = get_page(
        &f.app,
        &format!("/organizations/{}/teams", f.org_slug),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let csrf = hidden(&page, "csrf_token");
    let key = Uuid::new_v4().simple().to_string();
    let (status, body) = post_form(
        &f.app,
        &format!("/organizations/{}/teams", f.org_slug),
        &cookie,
        &[
            ("csrf_token", format!("{csrf}0")),
            ("idempotency_key", key.clone()),
            ("slug", "nope".to_owned()),
            ("name", "Nope".to_owned()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let probe: i64 =
        sqlx::query_scalar("SELECT count(*) FROM teams WHERE organization_id=$1 AND slug='nope'")
            .bind(f.org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(probe, 0);
    // The same form with the session's token creates the team.
    let (status, _) = post_form(
        &f.app,
        &format!("/organizations/{}/teams", f.org_slug),
        &cookie,
        &[
            ("csrf_token", csrf),
            ("idempotency_key", key),
            ("slug", "nope".to_owned()),
            ("name", "Nope".to_owned()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_created").await,
        1
    );
    finish(f).await;
}
/// B2, the inverse hole: the team's member is promoted to `owner` (so a direct
/// role demotion of them would leave the team's owner grant unreachable), and
/// the demotion is refused for exactly that reason.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_demotion_that_would_orphan_a_teams_owner_grant_is_refused() {
    let f = fixture().await;
    let team = tag("owners");
    // The member joins the owner team as an organization owner.
    hand_the_organization_to_a_team(&f, &team).await;
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.member.username
        ),
        Some(&f.member.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Demoting them now would leave the team's owner grant with nobody who can
    // exercise it (the demoted account is excluded from the count).
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.member.username
        ),
        Some(&f.member.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("owner"), "{body}");
    let role: String = sqlx::query_scalar(
        "SELECT role FROM memberships WHERE organization_id = $1 AND account_id = $2",
    )
    .bind(f.org_id)
    .bind(f.member.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(role, "owner");

    // With a second reachable owner the same demotion succeeds.
    let second = user(&f.pool, "second-owner").await;
    add_membership(&f.pool, f.org_id, second.id, "owner").await;
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/members/{}",
            f.org_slug, f.member.username
        ),
        Some(&f.member.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    finish(f).await;
}

/// No role system in M4.1 lets a fixture carry a custom role: the 0002 grant
/// trigger refuses any role name but a built-in, and the server's engine has no
/// injected role. The reach of each action is therefore pinned at the engine
/// with a custom role that holds `team.update` alone, over the same request
/// shape `facts::on_organization` builds for a member.
fn engine_may(policy: &str, action: githearth_authz::Action) -> bool {
    let mut engine = githearth_authz::Engine::new().unwrap();
    engine.add_custom_role("editor", policy).unwrap();
    let org = githearth_authz::Organization {
        id: Uuid::new_v4(),
        suspended: false,
    };
    let who = githearth_authz::Account {
        id: Uuid::new_v4(),
        suspended: false,
        site_admin: false,
        organizations: vec![org.id],
        teams: vec![],
        confinement: None,
    };
    let grants = [githearth_authz::Grant {
        role: githearth_authz::Role::Custom("editor".to_owned()),
        subject: githearth_authz::Subject::Account(who.id),
        scope: githearth_authz::Scope::Organization(org.id),
    }];
    engine
        .authorize(&githearth_authz::Request {
            principal: githearth_authz::Principal::Account(&who),
            teams: &[],
            grants: &grants,
            action,
            organization: &org,
            resource: githearth_authz::Resource::Organization,
            merge: None,
            auth_age: Some(Duration::from_secs(0)),
            read_only: false,
        })
        .is_allowed()
}

/// M1: granting a team grant authorizes `team.update`, never `team.delete`, so
/// a role that may edit a team but not delete it can grant. Before the fix
/// `add_grant` (API and page) asked for `team.delete`, and the last assertion
/// here fails: `team.delete` never follows from `team.update`, and the handler
/// must not ask for it.
#[test]
fn a_team_update_only_role_may_grant_but_not_delete_or_revoke() {
    use githearth_authz::Action;
    let policy = "permit (principal in ?principal, \
                  action == GitHearth::Action::\"team.update\", resource in ?resource);";
    assert!(
        engine_may(policy, Action::TeamUpdate),
        "the custom role must reach team.update"
    );
    for action in [Action::TeamDelete, Action::TeamCreate] {
        assert!(
            !engine_may(policy, action),
            "{action:?} must not follow from team.update"
        );
    }
}

/// The handler half of M1: `add_grant` (API) and its page form ask for
/// `team.update`, never `team.delete` — adding a grant is an update, and a
/// caller who may `team.delete` but not `team.update` must not be able to hand a
/// team an organization `owner` grant. Deleting the team and revoking a grant
/// keep `team.delete`. Source-level because the action is not observable through
/// a live caller: no fixture can hold a custom role (the 0002 grant trigger
/// refuses any name but a built-in), so no role distinguishes the two over HTTP.
#[test]
fn the_grant_handlers_ask_for_team_update_and_the_delete_handlers_for_team_delete() {
    let teams = include_str!("../src/teams.rs");
    let functions = |name: &str| -> String {
        let start = teams
            .find(&format!("async fn {name}("))
            .unwrap_or_else(|| panic!("{name} exists"));
        let end = teams[start..]
            .find("\nasync fn ")
            .map_or(teams.len(), |offset| start + offset);
        teams[start..end].to_owned()
    };
    let grant = functions("add_grant");
    assert!(
        grant.contains("Action::TeamUpdate"),
        "add_grant must ask for team.update"
    );
    assert!(
        !grant.contains("Action::TeamDelete"),
        "add_grant must not ask for team.delete"
    );
    for name in ["delete_team_inner", "revoke_grant"] {
        let body = functions(name);
        assert!(
            body.contains("Action::TeamDelete"),
            "{name} must ask for team.delete"
        );
        assert!(
            !body.contains("Action::TeamUpdate"),
            "{name} must not ask for team.update"
        );
    }
    // The page's grant form matches the API's grant handler.
    let page = include_str!("../src/teams_ui.rs");
    let start = page.find("async fn grant_submit(").unwrap();
    let end = page[start..].find("\nasync fn ").unwrap() + start;
    let submit = &page[start..end];
    assert!(submit.contains("Action::TeamUpdate"), "grant_submit");
    assert!(!submit.contains("Action::TeamDelete"), "grant_submit");
}

/// The API half of M1, over a real owner: granting is a 201 and is audited.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn granting_a_team_grant_is_an_owner_update_and_still_works() {
    let f = fixture().await;
    let team = tag("editors");
    create_team(&f, &team).await;
    grant_repo(&f, &team, "read").await;
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.team_grant_added").await,
        1
    );
    finish(f).await;
}
/// Adding a member is `team.update` for every caller, including one adding
/// themselves: a team membership exercises every grant the team holds, so a
/// member joining a team would hand themselves the access the team carries.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_member_cannot_add_themselves_or_anyone_else_to_a_team() {
    let f = fixture().await;
    let team = tag("joins");
    let id = create_team(&f, &team).await;
    let _grant = grant_repo(&f, &team, "read").await;

    // The member adding themselves is refused, and the repository the team's
    // grant covers stays closed to them.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/members", team_path(&f, &team)),
        Some(&f.member.token),
        None,
        Some(&json!({"username": f.member.username})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = api(
        &f.app,
        "GET",
        &repo_api(&f).await,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Adding anyone else is the same refusal.
    for username in [&f.owner.username, &f.outsider.username] {
        let (status, body) = api(
            &f.app,
            "POST",
            &format!("{}/members", team_path(&f, &team)),
            Some(&f.member.token),
            None,
            Some(&json!({"username": username})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let added: i64 = sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id = $1")
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(added, 0, "no join was written");

    // The page is as closed as the control: the member is not on the team and
    // holds no team action, so they get the refusal page, not the team.
    let (status, page) = get_page(
        &f.app,
        &format!("/organizations/{}/teams/{}", f.org_slug, team),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert!(
        page.contains("Only an organization owner can administer team"),
        "{page}"
    );
    assert!(!page.contains("Grant repository access"), "{page}");

    // A signed-in caller with no membership of the organization at all is the
    // organization's 404, the same for an existing team and a missing one.
    let stranger = user(&f.pool, "stranger").await;
    for name in [team.clone(), "no-such-team".to_owned()] {
        let (status, body) = api(
            &f.app,
            "POST",
            &format!("{}/members", team_path(&f, &name)),
            Some(&stranger.token),
            None,
            Some(&json!({"username": stranger.username})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: {body}");
    }

    // The reach that needs no new permission remains: the owner, who holds
    // `team.update`, adds the member as a second member of the team.
    assert_eq!(
        add_team_member(&f, &team, &f.member.username).await,
        StatusCode::CREATED
    );
    finish(f).await;
}
