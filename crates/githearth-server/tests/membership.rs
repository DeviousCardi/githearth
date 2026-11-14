//! Organization membership administration (M4.1 acceptance case U04).
//!
//! Covers the JSON API and the members page: adding a member by username,
//! changing a role, removing a member and leaving; the masked 403/404 rules;
//! the last-owner rule (including two concurrent removals); idempotent retries;
//! audit and outbox records; and that a removal or role change takes effect on
//! the API, the browser UI, Git over HTTP and Git over SSH.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_git::layout::ObjectFormat;
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router, ssh};
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
// JSON API and page helpers
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

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get_page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, _, body) = send(app, builder.body(Body::empty()).unwrap()).await;
    (status, body)
}

async fn post_form(
    app: &Router,
    path: &str,
    cookie: &str,
    form: &[(&str, &str)],
) -> (StatusCode, Option<String>, String) {
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
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

/// The first hidden input with this name.
fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).expect("hidden field") + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

fn csrf(html: &str) -> String {
    hidden(html, "csrf_token")
}

// ---------------------------------------------------------------------------
// Git helpers
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

async fn git_fails(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        !out.status.success(),
        "expected git to fail, it succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("githearth-{}-{}", label, Uuid::new_v4()))
}

/// The storage root also carries the admission Unix socket, whose whole path
/// must stay under 107 bytes: keep it short.
fn short_temp_dir(label: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("ghm-{label}-{}", &id[..12]))
}

// ---------------------------------------------------------------------------
// Database fixtures
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

struct User {
    id: Uuid,
    username: String,
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
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash_password("testpass").unwrap())
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
        token,
    }
}

async fn organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Membership Fixture')")
        .bind(id)
        .bind(&slug)
        .execute(pool)
        .await
        .unwrap();
    add_membership(pool, id, owner, "owner").await;
    (id, slug)
}

/// Insert one membership and its matching organization grant directly, the way
/// an operator or an accepted invitation eventually will.
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

async fn active_grant_roles(pool: &PgPool, org_id: Uuid, account: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT role FROM grants WHERE scope_kind = 'organization' AND scope_id = $1 \
         AND subject_kind = 'account' AND subject_id = $2 AND revoked_at IS NULL ORDER BY role",
    )
    .bind(org_id)
    .bind(account)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn membership_role(pool: &PgPool, org_id: Uuid, account: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT role FROM memberships WHERE organization_id = $1 AND account_id = $2",
    )
    .bind(org_id)
    .bind(account)
    .fetch_optional(pool)
    .await
    .unwrap()
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
    let work = temp_dir("membership-seed");
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Members\n").unwrap();
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
    repo_path: PathBuf,
    owner: User,
    member: User,
    outsider: User,
}

async fn fixture() -> Fixture {
    let storage_root = short_temp_dir("f");
    std::fs::create_dir_all(&storage_root).unwrap();
    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 2,
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
    let (org_id, org_slug) = organization(&pool, owner.id).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member.id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'member','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(member.id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

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
    let repo_path = seed_repo(&state, repo_id).await;
    let app = router(state.clone());
    Fixture {
        app,
        state,
        pool,
        storage_root,
        org_id,
        org_slug,
        repo_name,
        repo_path,
        owner,
        member,
        outsider,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
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

/// A test SSH client authenticated as `account`, with `git ls-remote` pointed
/// at the fixture repository. `None` when the ssh binary is unavailable.
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

async fn ssh_client(f: &Fixture, account: Uuid) -> Option<SshClient> {
    if Command::new("ssh").arg("-V").output().is_err() {
        eprintln!("SKIP: ssh binary not found");
        return None;
    }
    let key_dir = temp_dir("membership-ssh-key");
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let _ = std::fs::remove_file(&client_key);
    let generated = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-C",
            "membership-test",
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
            key_id: Uuid::new_v4(),
            read_only: false,
        });
        Box::pin(async move { Ok(identity) })
    });
    let (server, port_rx) = ssh::spawn(f.state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();
    Some(SshClient {
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
    })
}

fn members_path(f: &Fixture) -> String {
    format!("/api/v1/organizations/{}/members", f.org_slug)
}

fn member_path(f: &Fixture, username: &str) -> String {
    format!("/api/v1/organizations/{}/members/{username}", f.org_slug)
}

async fn grant_repo(f: &Fixture, account: Uuid, role: &str) {
    let repo_id: Uuid =
        sqlx::query_scalar("SELECT id FROM repositories WHERE organization_id = $1 AND name = $2")
            .bind(f.org_id)
            .bind(&f.repo_name)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) VALUES ($1,$2,'account',$3,'repository',$4,$5,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(f.org_id)
        .bind(repo_id)
        .execute(&f.pool)
        .await
        .unwrap();
}

/// An independent organization-scope grant, not the membership role grant.
async fn grant_org(f: &Fixture, account: Uuid, role: &str) {
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(f.org_id)
        .execute(&f.pool)
        .await
        .unwrap();
}

/// A team in the fixture organization with an organization-scope owner grant.
async fn grant_team_owner(f: &Fixture, org_id: Uuid, members: &[Uuid]) -> Uuid {
    let team_id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Owners')")
        .bind(team_id)
        .bind(org_id)
        .bind(tag("team-owner"))
        .execute(&f.pool)
        .await
        .unwrap();
    for member in members {
        sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
            .bind(team_id)
            .bind(member)
            .execute(&f.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','team',$2,'organization',$3,$4)")
        .bind(Uuid::new_v4())
        .bind(team_id)
        .bind(org_id)
        .bind(f.owner.id)
        .execute(&f.pool)
        .await
        .unwrap();
    team_id
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn owner_lists_adds_changes_and_removes_members() {
    let f = fixture().await;

    // Unauthenticated requests are refused before anything else.
    let (status, _) = api(&f.app, "GET", &members_path(&f), None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The list starts with the creator as the only owner.
    let (status, body) = api(
        &f.app,
        "GET",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let members = body.as_array().unwrap();
    assert!(
        members
            .iter()
            .any(|m| m["username"] == f.owner.username.as_str()
                && m["role"] == "owner"
                && m["account_id"] == f.owner.id.to_string().as_str())
    );
    assert!(
        members
            .iter()
            .any(|m| m["username"] == f.member.username.as_str())
    );

    // Add an existing account by username.
    let newcomer = user(&f.pool, "newcomer").await;
    let (status, body) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": newcomer.username, "role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["username"], newcomer.username.as_str());
    assert_eq!(body["role"], "member");
    assert_eq!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .as_deref(),
        Some("member")
    );
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, newcomer.id).await,
        vec!["member".to_owned()]
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_added").await,
        1
    );

    // The same member cannot be added twice.
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": newcomer.username, "role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Promote to owner: one active grant, the old one revoked.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &newcomer.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "owner");
    assert_eq!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .as_deref(),
        Some("owner")
    );
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, newcomer.id).await,
        vec!["owner".to_owned()]
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_promoted").await,
        1
    );

    // Demote again; the previous grant is revoked and only the member grant is active.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &newcomer.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, newcomer.id).await,
        vec!["member".to_owned()]
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_demoted").await,
        1
    );

    // Removing the member deletes the membership and revokes every grant.
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &newcomer.username),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .is_none()
    );
    assert!(
        active_grant_roles(&f.pool, f.org_id, newcomer.id)
            .await
            .is_empty()
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_removed").await,
        1
    );

    // Every change wrote an audit row and an outbox row sharing one id.
    let paired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events e JOIN outbox_events o ON o.id = e.id \
         WHERE e.organization_id = $1 AND e.kind LIKE 'organization.member%'",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(paired, 4, "add, promote, demote and remove each audit once");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_owner_is_forbidden_and_outsider_is_masked() {
    let f = fixture().await;
    let outsider_member = user(&f.pool, "target").await;
    add_membership(&f.pool, f.org_id, outsider_member.id, "member").await;

    // A plain member may read the organization but not administer membership.
    for (method, path, body) in [
        ("GET", members_path(&f), None),
        (
            "POST",
            members_path(&f),
            Some(json!({"username": outsider_member.username, "role": "member"})),
        ),
        (
            "PATCH",
            member_path(&f, &outsider_member.username),
            Some(json!({"role": "owner"})),
        ),
        ("DELETE", member_path(&f, &outsider_member.username), None),
    ] {
        let (status, out) = api(
            &f.app,
            method,
            &path,
            Some(&f.member.token),
            None,
            body.as_ref(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {out}");
    }

    // A non-member gets the same 404 a missing organization gets on every
    // path, so the organization and its membership stay hidden.
    for (method, path, body) in [
        ("GET", members_path(&f), None),
        (
            "POST",
            members_path(&f),
            Some(json!({"username": outsider_member.username, "role": "member"})),
        ),
        (
            "PATCH",
            member_path(&f, &outsider_member.username),
            Some(json!({"role": "owner"})),
        ),
        ("DELETE", member_path(&f, &outsider_member.username), None),
        (
            "GET",
            format!("/api/v1/organizations/{}/members", tag("absent-org")),
            None,
        ),
    ] {
        let (status, out) = api(
            &f.app,
            method,
            &path,
            Some(&f.outsider.token),
            None,
            body.as_ref(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {out}");
    }

    // Nothing changed.
    assert_eq!(
        membership_role(&f.pool, f.org_id, outsider_member.id)
            .await
            .as_deref(),
        Some("member")
    );
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, outsider_member.id).await,
        vec!["member".to_owned()]
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn last_owner_cannot_be_removed_demoted_or_leave() {
    let f = fixture().await;
    // Make the fixture member an owner and remove the creator's ownership so a
    // single owner remains; then prove every path refuses to empty the set.
    api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &f.owner.username),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // One owner left: removing themselves (leaving) is refused.
    let (status, body) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &f.member.username),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        membership_role(&f.pool, f.org_id, f.member.id)
            .await
            .as_deref(),
        Some("owner")
    );

    // ... and so is demoting themselves, by the API and by the page.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.member.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let page = get_page(
        &f.app,
        &format!("/organizations/{}/members", f.org_slug),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(page.0, StatusCode::OK);
    assert!(
        page.1.contains("Last owner"),
        "page offers no demote control"
    );
    let (status, _, body) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/role",
            f.org_slug, f.member.username
        ),
        &f.member.cookie(),
        &[
            ("csrf_token", &csrf(&page.1)),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
            ("role", "member"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("at least one owner"), "{body}");

    // A second owner makes both operations legal.
    let second = user(&f.pool, "second").await;
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.member.token),
        None,
        Some(&json!({"username": second.username, "role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&second.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a demotion with another owner is allowed"
    );

    // The single remaining owner cannot be removed.
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &second.username),
        Some(&second.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A no-op role change is not an audit event.
    let promotions = audit_kinds(&f.pool, f.org_id, "organization.member_promoted").await;
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &second.username),
        Some(&second.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_promoted").await,
        promotions
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_removals_never_empty_the_owner_set() {
    let f = fixture().await;
    // Two owners, two requests each removing the other, five times over. The
    // organization row lock serializes them: exactly one commits.
    for iteration in 0..5 {
        let (org_id, org_slug) = organization(&f.pool, f.owner.id).await;
        let second = user(&f.pool, "coowner").await;
        add_membership(&f.pool, org_id, second.id, "owner").await;
        let path_a = format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            second.username
        );
        let path_b = format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            f.owner.username
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        // Hold both requests inside the mutation (after authorization, before
        // the organization lock) so neither can commit before the other has
        // started writing: request-start synchronization alone would let one
        // finish first.
        githearth_server::membership::test_pause_mutations(org_id, 500);
        let (a, b) = tokio::join!(
            async {
                barrier.wait().await;
                api(&f.app, "DELETE", &path_a, Some(&f.owner.token), None, None).await
            },
            async {
                barrier.wait().await;
                api(&f.app, "DELETE", &path_b, Some(&second.token), None, None).await
            },
        );
        let statuses = [a.0, b.0];
        assert_eq!(
            githearth_server::membership::test_mutation_entries(org_id),
            2,
            "iteration {iteration}: both requests must reach the mutation"
        );
        assert_eq!(
            statuses.iter().filter(|s| s.is_success()).count(),
            1,
            "iteration {iteration}: exactly one removal must win: {:?} {:?}",
            a.1,
            b.1
        );
        let refusal = *statuses.iter().find(|s| !s.is_success()).unwrap();
        assert!(
            matches!(
                refusal,
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::CONFLICT
            ),
            "iteration {iteration}: loser must be refused, got {refusal}: {:?} {:?}",
            a.1,
            b.1
        );
        let owners: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memberships WHERE organization_id = $1 AND role = 'owner'",
        )
        .bind(org_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(owners, 1, "iteration {iteration}: one owner must remain");
        // The surviving owner still holds exactly its one organization grant.
        let survivor: Uuid = sqlx::query_scalar(
            "SELECT account_id FROM memberships WHERE organization_id = $1 AND role = 'owner'",
        )
        .bind(org_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            active_grant_roles(&f.pool, org_id, survivor).await,
            vec!["owner".to_owned()]
        );
    }

    // The same guarantee for two members leaving at once, in a fresh org with
    // two owners: one leave succeeds, the other is refused.
    let (org_id, org_slug) = organization(&f.pool, f.owner.id).await;
    let other = user(&f.pool, "leaver").await;
    add_membership(&f.pool, org_id, other.id, "owner").await;
    let owner_leave = format!(
        "/api/v1/organizations/{org_slug}/members/{}",
        f.owner.username
    );
    let other_leave = format!(
        "/api/v1/organizations/{org_slug}/members/{}",
        other.username
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    // Same in-mutation barrier for the two leaves.
    githearth_server::membership::test_pause_mutations(org_id, 500);
    let (a, b) = tokio::join!(
        async {
            barrier.wait().await;
            api(
                &f.app,
                "DELETE",
                &owner_leave,
                Some(&f.owner.token),
                None,
                None,
            )
            .await
        },
        async {
            barrier.wait().await;
            api(
                &f.app,
                "DELETE",
                &other_leave,
                Some(&other.token),
                None,
                None,
            )
            .await
        },
    );
    let statuses = [a.0, b.0];
    assert_eq!(
        githearth_server::membership::test_mutation_entries(org_id),
        2,
        "both leaves must reach the mutation"
    );
    assert_eq!(
        statuses.iter().filter(|s| s.is_success()).count(),
        1,
        "exactly one leave must win: {:?} {:?}",
        a.1,
        b.1
    );
    let refusal = *statuses.iter().find(|s| !s.is_success()).unwrap();
    assert!(
        matches!(
            refusal,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::CONFLICT
        ),
        "the other leave must be refused, got {refusal}: {:?} {:?}",
        a.1,
        b.1
    );
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id = $1 AND role = 'owner'",
    )
    .bind(org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(owners, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn member_can_leave_and_is_denied_on_api_ui_http_and_ssh_afterwards() {
    let f = fixture().await;
    // The member can use the repository before leaving.
    grant_repo(&f, f.member.id, "write").await;
    let repo_api = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, f.repo_name
    );
    let (status, body) = api(&f.app, "GET", &repo_api, Some(&f.member.token), None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let port = serve(&f.app).await;
    let clone = temp_dir("membership-clone");
    let url = format!(
        "http://{}:testpass@127.0.0.1:{port}/{}/{}.git",
        f.member.username, f.org_slug, f.repo_name
    );
    git_ok(git_cmd(&["clone", "--", &url, clone.to_str().unwrap()])).await;

    // The same member is admitted over SSH before leaving.
    let ssh = ssh_client(&f, f.member.id).await;
    if let Some(ssh) = &ssh {
        git_ok(ssh.ls_remote()).await;
    }

    // A plain member may remove their own membership.
    let (status, body) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &f.member.username),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_left").await,
        1
    );
    assert!(
        membership_role(&f.pool, f.org_id, f.member.id)
            .await
            .is_none()
    );
    assert!(
        active_grant_roles(&f.pool, f.org_id, f.member.id)
            .await
            .is_empty()
    );

    // The JSON API, the browser UI and Git over HTTP deny the next request.
    let (status, _) = api(&f.app, "GET", &repo_api, Some(&f.member.token), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api(
        &f.app,
        "GET",
        &format!("/api/v1/organizations/{}", f.org_id),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_page(
        &f.app,
        &format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name),
        Some(&f.member.cookie()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let denied_clone = temp_dir("membership-clone-denied");
    let stderr = git_fails(git_cmd(&[
        "clone",
        "--",
        &url,
        denied_clone.to_str().unwrap(),
    ]))
    .await;
    assert!(
        stderr.contains("not found") || stderr.contains("not permitted") || stderr.contains("403"),
        "unexpected HTTP refusal: {stderr}"
    );

    // ... and so does SSH, so the test name's four transports are all proved.
    if let Some(ssh) = &ssh {
        let stderr = git_fails(ssh.ls_remote()).await;
        assert!(
            stderr.contains("repository not found"),
            "unexpected SSH refusal: {stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&clone);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn member_can_leave_a_suspended_organization() {
    let f = fixture().await;
    // A suspended organization is hidden from its members, but leaving only
    // reduces the caller's own access, so it stays permitted: the documented
    // exemption from the `suspended-organization` forbid.
    let (org_id, org_slug) = organization(&f.pool, f.owner.id).await;
    let via_api = user(&f.pool, "susp-api").await;
    let via_page = user(&f.pool, "susp-page").await;
    add_membership(&f.pool, org_id, via_api.id, "member").await;
    add_membership(&f.pool, org_id, via_page.id, "member").await;
    sqlx::query("UPDATE organizations SET suspended = true WHERE id = $1")
        .bind(org_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, _) = api(
        &f.app,
        "GET",
        &format!("/api/v1/organizations/{org_id}"),
        Some(&via_api.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "suspended org is hidden");

    // JSON API self-leave: 204.
    let (status, body) = api(
        &f.app,
        "DELETE",
        &format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            via_api.username
        ),
        Some(&via_api.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Page self-leave: 303 to the home page.
    let (status, home) = get_page(&f.app, "/", Some(&via_page.cookie())).await;
    assert_eq!(status, StatusCode::OK);
    let token = csrf(&home);
    let (status, location, body) = post_form(
        &f.app,
        &format!(
            "/organizations/{org_slug}/members/{}/remove",
            via_page.username
        ),
        &via_page.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(location.as_deref(), Some("/"));

    for member in [&via_api, &via_page] {
        assert!(
            membership_role(&f.pool, org_id, member.id).await.is_none(),
            "{} must no longer be a member",
            member.username
        );
        assert!(
            active_grant_roles(&f.pool, org_id, member.id)
                .await
                .is_empty(),
            "{} must hold no active grant",
            member.username
        );
    }
    assert_eq!(
        membership_role(&f.pool, org_id, f.owner.id)
            .await
            .as_deref(),
        Some("owner")
    );
    assert_eq!(
        audit_kinds(&f.pool, org_id, "organization.member_left").await,
        2
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn removed_member_is_denied_over_ssh() {
    if Command::new("ssh").arg("-V").output().is_err() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }
    let f = fixture().await;
    grant_repo(&f, f.member.id, "write").await;

    let key_dir = temp_dir("membership-ssh-key");
    std::fs::create_dir_all(&key_dir).unwrap();
    let client_key = key_dir.join("id_ed25519");
    let _ = std::fs::remove_file(&client_key);
    let generated = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &client_key.to_string_lossy(),
            "-N",
            "",
            "-C",
            "membership-test",
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

    let member_id = f.member.id;
    let authenticate: ssh::Authenticate = Arc::new(move |fp: String| {
        let identity = (fp == fingerprint).then_some(ssh::SshIdentity {
            account: member_id,
            key_id: Uuid::new_v4(),
            read_only: false,
        });
        Box::pin(async move { Ok(identity) })
    });
    let (_handle, port_rx) = ssh::spawn(f.state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();
    let ssh_command = format!(
        "ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        client_key.display()
    );
    let url = format!(
        "ssh://git@127.0.0.1:{ssh_port}/{}/{}.git",
        f.org_slug, f.repo_name
    );
    let mut ls_remote = git_cmd(&["ls-remote", &url]);
    ls_remote.env("GIT_SSH_COMMAND", &ssh_command);
    git_ok(ls_remote).await;

    // Remove the member through the API, then the next SSH connection is denied
    // exactly like a missing repository.
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mut denied = git_cmd(&["ls-remote", &url]);
    denied.env("GIT_SSH_COMMAND", &ssh_command);
    let stderr = git_fails(denied).await;
    assert!(stderr.contains("repository not found"), "{stderr}");

    let _ = std::fs::remove_dir_all(&key_dir);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn role_change_alters_what_may_be_pushed() {
    let f = fixture().await;
    // A read grant lets the member clone; it carries no write, so the
    // organization role alone decides whether a push is admitted.
    grant_repo(&f, f.member.id, "read").await;
    let port = serve(&f.app).await;
    let url = format!(
        "http://{}:testpass@127.0.0.1:{port}/{}/{}.git",
        f.member.username, f.org_slug, f.repo_name
    );
    let clone = temp_dir("membership-push");
    git_ok(git_cmd(&["clone", "--", &url, clone.to_str().unwrap()])).await;
    std::fs::write(clone.join("member.txt"), "member\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        clone.to_str().unwrap(),
        "add",
        "member.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        clone.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "member change",
    ]))
    .await;

    // A `member` organization role carries no write, so the push is refused.
    let push = || {
        let mut cmd = git_cmd(&[
            "-C",
            clone.to_str().unwrap(),
            "push",
            "origin",
            "HEAD:refs/heads/member-branch",
        ]);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    };
    git_fails(push()).await;

    // Promoting to owner gives the organization-wide write grant the push needs.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, f.member.id).await,
        vec!["owner".to_owned()],
        "the role change replaces the organization grant"
    );
    git_ok(push()).await;
    let remote = git_run(git_cmd(&[
        "--git-dir",
        f.repo_path.to_str().unwrap(),
        "rev-parse",
        "refs/heads/member-branch",
    ]))
    .await;
    assert!(remote.status.success(), "the promoted owner's push landed");

    // Demoting back to member (with another owner present) refuses the next push.
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    std::fs::write(clone.join("member2.txt"), "member again\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        clone.to_str().unwrap(),
        "add",
        "member2.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        clone.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "member change 2",
    ]))
    .await;
    git_fails(push()).await;
    let still = git_run(git_cmd(&[
        "--git-dir",
        f.repo_path.to_str().unwrap(),
        "rev-parse",
        "refs/heads/member-branch",
    ]))
    .await;
    assert_eq!(
        String::from_utf8_lossy(&still.stdout).trim(),
        String::from_utf8_lossy(&remote.stdout).trim(),
        "the refused push did not move the branch"
    );

    let _ = std::fs::remove_dir_all(&clone);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn members_page_renders_controls_and_requires_administration() {
    let f = fixture().await;
    let path = format!("/organizations/{}/members", f.org_slug);
    let outsider = user(&f.pool, "page-outside").await;

    // Anonymous visitors sign in; a member is refused; a stranger sees 404.
    let (status, location, _) = send(
        &f.app,
        Request::builder().uri(&path).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/login"));
    let (status, body) = get_page(&f.app, &path, Some(&f.member.cookie())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("Only an organization owner"));
    let (status, _) = get_page(&f.app, &path, Some(&outsider.cookie())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The owner sees an accessible table and the add form.
    let (status, page) = get_page(&f.app, &path, Some(&f.owner.cookie())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Organization members"));
    assert!(page.contains("<caption"));
    assert!(page.contains("scope=\"col\""));
    assert!(page.contains("<label for=\"member-username\">"));
    assert!(page.contains("<th scope=\"col\">Since</th>"));
    assert!(page.contains("<time datetime="));
    assert!(page.contains("<label for=\"member-role\">"));
    assert!(page.contains(&format!("aria-label=\"Remove {}\"", f.member.username)));
    let token = csrf(&page);
    assert_eq!(token.len(), 32);

    // Adding without the CSRF token changes nothing.
    let newcomer = user(&f.pool, "page-added").await;
    let key = Uuid::new_v4().simple().to_string();
    let (status, _, _) = post_form(
        &f.app,
        &path,
        &f.owner.cookie(),
        &[
            ("csrf_token", "wrong"),
            ("idempotency_key", &key),
            ("username", &newcomer.username),
            ("role", "member"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .is_none()
    );

    // A malformed per-form key is refused with a reload notice, not a 503.
    let (status, _, body) = post_form(
        &f.app,
        &path,
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", "bad key"),
            ("username", &newcomer.username),
            ("role", "member"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("Reload the page"));

    // A member with a valid CSRF token still cannot add, promote themselves or
    // remove someone else: every page submit re-checks authorization.
    let (status, page) = get_page(&f.app, "/", Some(&f.member.cookie())).await;
    assert_eq!(status, StatusCode::OK);
    let member_csrf = csrf(&page);
    let (status, _, _) = post_form(
        &f.app,
        &path,
        &f.member.cookie(),
        &[
            ("csrf_token", &member_csrf),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
            ("username", &newcomer.username),
            ("role", "member"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .is_none()
    );
    let (status, _, _) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/role",
            f.org_slug, f.member.username
        ),
        &f.member.cookie(),
        &[
            ("csrf_token", &member_csrf),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
            ("role", "owner"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        membership_role(&f.pool, f.org_id, f.member.id)
            .await
            .as_deref(),
        Some("member")
    );

    // With it, the form adds, changes and removes a member.
    let (status, location, body) = post_form(
        &f.app,
        &path,
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &key),
            ("username", &newcomer.username),
            ("role", "owner"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let members_page = format!("/organizations/{}/members", f.org_slug);
    assert_eq!(location.as_deref(), Some(members_page.as_str()));
    assert_eq!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .as_deref(),
        Some("owner")
    );

    let (status, location, _) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/role",
            f.org_slug, newcomer.username
        ),
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
            ("role", "member"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.is_some());
    assert_eq!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .as_deref(),
        Some("member")
    );

    let (status, location, _) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/remove",
            f.org_slug, newcomer.username
        ),
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.is_some());
    assert!(
        membership_role(&f.pool, f.org_id, newcomer.id)
            .await
            .is_none()
    );

    // A rejected change re-renders the page with an alert instead of a bare error.
    let (status, _, body) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/remove",
            f.org_slug, f.owner.username
        ),
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("role=\"alert\""));
    assert!(body.contains("at least one owner"));

    // An owner whose organization has a second owner can leave from the page;
    // the browser then returns to the home page, which no longer lists it.
    let second_owner = user(&f.pool, "page-second").await;
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": second_owner.username, "role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, location, _) = post_form(
        &f.app,
        &format!(
            "/organizations/{}/members/{}/remove",
            f.org_slug, f.owner.username
        ),
        &f.owner.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", &Uuid::new_v4().simple().to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/"));
    assert!(
        membership_role(&f.pool, f.org_id, f.owner.id)
            .await
            .is_none()
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_left").await,
        1
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn members_page_error_paths_refuse_before_validation() {
    let f = fixture().await;
    let victim = user(&f.pool, "leak-victim").await;
    add_membership(&f.pool, f.org_id, victim.id, "member").await;
    let role_path = format!(
        "/organizations/{}/members/{}/role",
        f.org_slug, victim.username
    );
    let remove_path = format!("/organizations/{}/members/%00/remove", f.org_slug);

    // A plain member and an outsider submit an invalid role and a NUL
    // username. Authorization must win over validation, so neither response is
    // the rendered members page and neither reveals a member name.
    for (actor, expected) in [
        (&f.member, StatusCode::FORBIDDEN),
        (&f.outsider, StatusCode::NOT_FOUND),
    ] {
        let (status, home) = get_page(&f.app, "/", Some(&actor.cookie())).await;
        assert_eq!(status, StatusCode::OK);
        let token = csrf(&home);

        let role_key = Uuid::new_v4().simple().to_string();
        let (status, _, body) = post_form(
            &f.app,
            &role_path,
            &actor.cookie(),
            &[
                ("csrf_token", &token),
                ("idempotency_key", &role_key),
                ("role", "admin"),
            ],
        )
        .await;
        assert_eq!(status, expected, "invalid role: {body}");
        assert!(!body.contains("Organization members"), "{body}");
        assert!(!body.contains("Add a member"), "{body}");
        for victim_name in [&f.owner.username, &victim.username] {
            assert!(
                !body.contains(victim_name),
                "invalid role leaked {victim_name}: {body}"
            );
        }

        let remove_key = Uuid::new_v4().simple().to_string();
        let (status, _, body) = post_form(
            &f.app,
            &remove_path,
            &actor.cookie(),
            &[("csrf_token", &token), ("idempotency_key", &remove_key)],
        )
        .await;
        assert_eq!(status, expected, "NUL username: {body}");
        assert!(!body.contains("Organization members"), "{body}");
        assert!(!body.contains("Add a member"), "{body}");
        for victim_name in [&f.owner.username, &victim.username] {
            assert!(
                !body.contains(victim_name),
                "NUL username leaked {victim_name}: {body}"
            );
        }
    }

    assert_eq!(
        membership_role(&f.pool, f.org_id, victim.id)
            .await
            .as_deref(),
        Some("member")
    );
    assert_eq!(
        membership_role(&f.pool, f.org_id, f.owner.id)
            .await
            .as_deref(),
        Some("owner")
    );

    // A malformed slug is a client error, not a service failure. Treated as
    // 400 (not 404): the name is invalid, while a well-formed name that names
    // nothing stays the 404 page. The owner's baseline is otherwise a 200, so
    // a 503 here would be a server error for the caller's own typo.
    let (status, body) = get_page(
        &f.app,
        "/organizations/%00/members",
        Some(&f.owner.cookie()),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a malformed slug must not read as a service failure: {body}"
    );
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mutations_honor_idempotency_keys() {
    let f = fixture().await;
    let newcomer = user(&f.pool, "idem").await;
    let path = members_path(&f);

    // The same key replays the stored response: one membership, one event.
    let key = "members-add-1";
    let first = api(
        &f.app,
        "POST",
        &path,
        Some(&f.owner.token),
        Some(key),
        Some(&json!({"username": newcomer.username, "role": "member"})),
    )
    .await;
    let second = api(
        &f.app,
        "POST",
        &path,
        Some(&f.owner.token),
        Some(key),
        Some(&json!({"username": newcomer.username, "role": "member"})),
    )
    .await;
    assert_eq!(first.0, StatusCode::CREATED);
    assert_eq!(second.0, StatusCode::CREATED);
    assert_eq!(first.1, second.1);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_added").await,
        1
    );

    // The same key with a different body is refused as a reuse.
    let (status, body) = api(
        &f.app,
        "POST",
        &path,
        Some(&f.owner.token),
        Some(key),
        Some(&json!({"username": newcomer.username, "role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A concurrent duplicate with one key still creates one membership.
    let other = user(&f.pool, "race").await;
    let concurrent_key = "members-add-race";
    let body = json!({"username": other.username, "role": "member"});
    let (a, b) = tokio::join!(
        api(
            &f.app,
            "POST",
            &path,
            Some(&f.owner.token),
            Some(concurrent_key),
            Some(&body)
        ),
        api(
            &f.app,
            "POST",
            &path,
            Some(&f.owner.token),
            Some(concurrent_key),
            Some(&body)
        ),
    );
    assert!(
        a.0 == StatusCode::CREATED || b.0 == StatusCode::CREATED,
        "{:?} {:?}",
        a.0,
        b.0
    );
    for status in [a.0, b.0] {
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "unexpected status {status}"
        );
    }
    assert_eq!(
        membership_role(&f.pool, f.org_id, other.id)
            .await
            .as_deref(),
        Some("member")
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_added").await,
        2
    );

    // Role changes and removals are idempotent too.
    let role_key = "members-role-1";
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &other.username),
        Some(&f.owner.token),
        Some(role_key),
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &other.username),
        Some(&f.owner.token),
        Some(role_key),
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_promoted").await,
        1
    );

    let remove_key = "members-remove-1";
    for _ in 0..2 {
        let (status, _) = api(
            &f.app,
            "DELETE",
            &member_path(&f, &other.username),
            Some(&f.owner.token),
            Some(remove_key),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_removed").await,
        1
    );

    // A malformed key is a bad request, not a silently ignored header.
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &newcomer.username),
        Some(&f.owner.token),
        Some("bad key with spaces"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_inputs_and_cursor_pagination() {
    let f = fixture().await;

    // Unknown username, invalid role, malformed body and a bad path parameter.
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": tag("nobody"), "role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": f.member.username, "role": "admin"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": f.member.username, "role": "owner", "extra": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.outsider.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a non-member is a 404");
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &tag("nobody")),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown account is a 404");
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "admin"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "admin is not a membership role"
    );
    let (status, _) = api(
        &f.app,
        "DELETE",
        &format!("/api/v1/organizations/{}/members/%00", f.org_slug),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api(
        &f.app,
        "GET",
        &format!("{}?limit=0", members_path(&f)),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = api(
        &f.app,
        "GET",
        "/api/v1/organizations/%00/members",
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Cursor pagination walks every member exactly once in account-id order.
    for index in 0..4 {
        let extra = user(&f.pool, &format!("page{index}")).await;
        add_membership(&f.pool, f.org_id, extra.id, "member").await;
    }
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let url = match &after {
            Some(after) => format!("{}?limit=2&after={after}", members_path(&f)),
            None => format!("{}?limit=2", members_path(&f)),
        };
        let (status, body) = api(&f.app, "GET", &url, Some(&f.owner.token), None, None).await;
        assert_eq!(status, StatusCode::OK);
        let page = body.as_array().unwrap();
        assert!(page.len() <= 2);
        if page.is_empty() {
            break;
        }
        for member in page {
            seen.push(member["account_id"].as_str().unwrap().to_owned());
        }
        after = seen.last().cloned();
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no member is listed twice");
    assert_eq!(seen.len(), 6, "owner, member and four added");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "account id order is stable"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn removal_clears_team_membership_and_repository_grants() {
    let f = fixture().await;
    let target = user(&f.pool, "cleanup").await;
    add_membership(&f.pool, f.org_id, target.id, "member").await;
    grant_repo(&f, target.id, "write").await;

    // Put the member in a team, which the schema allows only for organization
    // members; the team is another engineer's feature, but removal must not
    // leave the membership behind.
    let team_id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Engineers')")
        .bind(team_id)
        .bind(f.org_id)
        .bind(tag("team"))
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
        .bind(team_id)
        .bind(target.id)
        .execute(&f.pool)
        .await
        .unwrap();

    // A role change keeps a direct repository grant: only the organization
    // role moves.
    let (status, _) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &target.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let repo_grants: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_id = $1 AND subject_id = $2 \
         AND repository_id IS NOT NULL AND revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(target.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(repo_grants, 1, "a role change keeps repository grants");

    // Removal revokes the repository grant too, and drops the team membership.
    let (status, _) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &target.username),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_id = $1 AND subject_id = $2 AND revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(target.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(active, 0, "removal revokes every grant in the organization");
    let team_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM team_members WHERE account_id = $1")
            .bind(target.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(team_rows, 0, "removal drops the member's team memberships");

    // Re-adding works and does not restore the old team membership.
    let (status, _) = api(
        &f.app,
        "POST",
        &members_path(&f),
        Some(&f.owner.token),
        None,
        Some(&json!({"username": target.username, "role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        membership_role(&f.pool, f.org_id, target.id)
            .await
            .as_deref(),
        Some("member")
    );
    let team_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM team_members WHERE account_id = $1")
            .bind(target.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(team_rows, 0, "re-adding does not restore team membership");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_role_changes_never_empty_the_owner_set() {
    let f = fixture().await;
    for iteration in 0..5 {
        let (org_id, org_slug) = organization(&f.pool, f.owner.id).await;
        let second = user(&f.pool, "coowner").await;
        add_membership(&f.pool, org_id, second.id, "owner").await;
        let demote_second = format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            second.username
        );
        let demote_first = format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            f.owner.username
        );
        let body = json!({"role": "member"});
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        // Both demotions run their authorization and then wait inside the
        // mutation, before either takes the organization lock.
        githearth_server::membership::test_pause_mutations(org_id, 500);
        let (a, b) = tokio::join!(
            async {
                barrier.wait().await;
                api(
                    &f.app,
                    "PATCH",
                    &demote_second,
                    Some(&f.owner.token),
                    None,
                    Some(&body),
                )
                .await
            },
            async {
                barrier.wait().await;
                api(
                    &f.app,
                    "PATCH",
                    &demote_first,
                    Some(&second.token),
                    None,
                    Some(&body),
                )
                .await
            },
        );
        let statuses = [a.0, b.0];
        assert_eq!(
            githearth_server::membership::test_mutation_entries(org_id),
            2,
            "iteration {iteration}: both requests must reach the mutation"
        );
        assert_eq!(
            statuses.iter().filter(|s| s.is_success()).count(),
            1,
            "iteration {iteration}: exactly one demotion must win: {:?} {:?}",
            a.1,
            b.1
        );
        let refusal = *statuses.iter().find(|s| !s.is_success()).unwrap();
        assert!(
            matches!(
                refusal,
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::CONFLICT
            ),
            "iteration {iteration}: loser must be refused, got {refusal}: {:?} {:?}",
            a.1,
            b.1
        );
        let owners: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memberships WHERE organization_id = $1 AND role = 'owner'",
        )
        .bind(org_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(owners, 1, "iteration {iteration}: one owner must remain");
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn concurrent_adds_of_the_same_member_create_one_row() {
    let f = fixture().await;
    let target = user(&f.pool, "twice").await;
    let body = json!({"username": target.username, "role": "member"});
    let path = members_path(&f);
    // Different idempotency keys: the database's uniqueness is the guarantee.
    let (a, b) = tokio::join!(
        api(
            &f.app,
            "POST",
            &path,
            Some(&f.owner.token),
            Some("add-a"),
            Some(&body)
        ),
        api(
            &f.app,
            "POST",
            &path,
            Some(&f.owner.token),
            Some("add-b"),
            Some(&body)
        ),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_by_key(|s| s.as_u16());
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::CONFLICT],
        "{:?} {:?}",
        a.1,
        b.1
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE organization_id = $1 AND account_id = $2",
    )
    .bind(f.org_id)
    .bind(target.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, target.id).await,
        vec!["member".to_owned()]
    );
    assert_eq!(
        audit_kinds(&f.pool, f.org_id, "organization.member_added").await,
        1
    );
    finish(f).await;
}

// ---------------------------------------------------------------------------
// The cross-review regressions (PR #171, round 3)
// ---------------------------------------------------------------------------

/// Wait until at least one mutation has entered the test pause for `org_id`.
async fn await_mutation_pause(org_id: Uuid) {
    for _ in 0..500 {
        if githearth_server::membership::test_mutation_entries(org_id) >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no membership mutation ever reached the pause");
}

async fn seed_reused_key(pool: &PgPool, actor: Uuid, key: &str, method: &str, path: &str) {
    let hash = githearth_server::idempotency::request_hash(method, path, b"another request");
    sqlx::query(
        "INSERT INTO operations (actor_id,key,request_hash,status,response_status,response_body,expires_at) \
         VALUES ($1,$2,$3,'done',200,'{}'::jsonb, clock_timestamp() + interval '24 hours')",
    )
    .bind(actor)
    .bind(key)
    .bind(hash)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn demoted_owner_cannot_finish_an_in_flight_removal() {
    let f = fixture().await;
    // A second owner can demote the caller while its removal is paused after
    // the outer authorization and before the organization lock.
    let second = user(&f.pool, "coowner").await;
    add_membership(&f.pool, f.org_id, second.id, "owner").await;

    githearth_server::membership::test_pause_mutations(f.org_id, 2_000);
    let app = f.app.clone();
    let token = f.owner.token.clone();
    let path = member_path(&f, &f.member.username);
    let pending =
        tokio::spawn(async move { api(&app, "DELETE", &path, Some(&token), None, None).await });
    await_mutation_pause(f.org_id).await;
    // The demotion itself must not pause.
    githearth_server::membership::test_pause_mutations(f.org_id, 0);
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.owner.username),
        Some(&second.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "real demotion: {body}");

    let (status, body) = pending.await.unwrap();
    assert!(
        matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::CONFLICT
        ),
        "a demoted caller must not finish the removal, got {status}: {body}"
    );
    assert_eq!(
        membership_role(&f.pool, f.org_id, f.member.id)
            .await
            .as_deref(),
        Some("member"),
        "the target membership must still be present"
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn demoted_owner_cannot_finish_an_in_flight_role_change() {
    let f = fixture().await;
    let second = user(&f.pool, "coowner").await;
    add_membership(&f.pool, f.org_id, second.id, "owner").await;

    githearth_server::membership::test_pause_mutations(f.org_id, 2_000);
    let app = f.app.clone();
    let token = f.owner.token.clone();
    let path = member_path(&f, &f.member.username);
    let pending = tokio::spawn(async move {
        api(
            &app,
            "PATCH",
            &path,
            Some(&token),
            None,
            Some(&json!({"role": "owner"})),
        )
        .await
    });
    await_mutation_pause(f.org_id).await;
    githearth_server::membership::test_pause_mutations(f.org_id, 0);
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.owner.username),
        Some(&second.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "real demotion: {body}");

    let (status, body) = pending.await.unwrap();
    assert!(
        matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::CONFLICT
        ),
        "a demoted caller must not finish the role change, got {status}: {body}"
    );
    assert_eq!(
        membership_role(&f.pool, f.org_id, f.member.id)
            .await
            .as_deref(),
        Some("member"),
        "the target role must not change"
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn role_change_keeps_independent_organization_grants() {
    let f = fixture().await;
    let target = user(&f.pool, "grant-keep").await;
    add_membership(&f.pool, f.org_id, target.id, "member").await;
    grant_org(&f, target.id, "write").await;
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, target.id).await,
        vec!["member".to_owned(), "write".to_owned()]
    );

    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &target.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, target.id).await,
        vec!["owner".to_owned(), "write".to_owned()],
        "the independent write grant survives and the old member grant is gone"
    );

    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &target.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        active_grant_roles(&f.pool, f.org_id, target.id).await,
        vec!["member".to_owned(), "write".to_owned()],
        "demotion replaces only the membership grant"
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn team_owner_grant_counts_towards_the_last_owner() {
    let f = fixture().await;
    // A team owner grant makes the sole membership owner non-last, so leaving
    // is allowed even though one `memberships.role = 'owner'` row remains at
    // the start.
    let team_id = grant_team_owner(&f, f.org_id, &[f.member.id]).await;
    let (status, body) = api(
        &f.app,
        "DELETE",
        &member_path(&f, &f.owner.username),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "owner may leave: {body}");
    assert!(
        membership_role(&f.pool, f.org_id, f.owner.id)
            .await
            .is_none()
    );
    let team_owner: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_kind = 'organization' AND scope_id = $1 \
         AND subject_kind = 'team' AND subject_id = $2 AND role = 'owner' AND revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(team_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(team_owner, 1, "the team owner grant remains active");

    // The same count allows a demotion of a sole membership owner.
    let (org_id, org_slug) = organization(&f.pool, f.owner.id).await;
    add_membership(&f.pool, org_id, f.member.id, "member").await;
    grant_team_owner(&f, org_id, &[f.member.id]).await;
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{org_slug}/members/{}",
            f.owner.username
        ),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a team owner grant makes the demotion legal: {body}"
    );
    assert_eq!(
        membership_role(&f.pool, org_id, f.owner.id)
            .await
            .as_deref(),
        Some("member")
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn masked_refusal_precedes_idempotency_checks() {
    let f = fixture().await;
    let victim = user(&f.pool, "mask-victim").await;
    add_membership(&f.pool, f.org_id, victim.id, "member").await;
    let absent = tag("absent-org");
    let existing = member_path(&f, &victim.username);
    let missing = format!("/api/v1/organizations/{absent}/members/{}", victim.username);

    // (a) A malformed key on an existing private organization and on a missing
    // one must both be the masked 404, not a key-validation error.
    let (existing_status, existing_body) = api(
        &f.app,
        "DELETE",
        &existing,
        Some(&f.outsider.token),
        Some("bad key with spaces"),
        None,
    )
    .await;
    let (missing_status, missing_body) = api(
        &f.app,
        "DELETE",
        &missing,
        Some(&f.outsider.token),
        Some("bad key with spaces"),
        None,
    )
    .await;
    assert_eq!(existing_status, StatusCode::NOT_FOUND, "{existing_body}");
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing_body}");
    assert_eq!(existing_body, missing_body, "the refusal must be masked");

    // (b) A valid key already used for a different request must not win over
    // the masked refusal either.
    let key = "mask-reused-key";
    seed_reused_key(&f.pool, f.outsider.id, key, "DELETE", &existing).await;
    let (existing_status, existing_body) = api(
        &f.app,
        "DELETE",
        &existing,
        Some(&f.outsider.token),
        Some(key),
        None,
    )
    .await;
    let (missing_status, missing_body) = api(
        &f.app,
        "DELETE",
        &missing,
        Some(&f.outsider.token),
        Some(key),
        None,
    )
    .await;
    assert_eq!(existing_status, StatusCode::NOT_FOUND, "{existing_body}");
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing_body}");
    assert_eq!(existing_body, missing_body, "the refusal must be masked");

    // The page paths make the same decisions before inspecting the form key.
    let (_, home) = get_page(&f.app, "/", Some(&f.outsider.cookie())).await;
    let token = csrf(&home);
    let existing_remove = format!(
        "/organizations/{}/members/{}/remove",
        f.org_slug, victim.username
    );
    let missing_remove = format!("/organizations/{absent}/members/{}/remove", victim.username);
    for key in ["bad key with spaces", "mask-page-reused"] {
        if key == "mask-page-reused" {
            seed_reused_key(&f.pool, f.outsider.id, key, "DELETE", &existing).await;
        }
        let (existing_status, _, existing_body) = post_form(
            &f.app,
            &existing_remove,
            &f.outsider.cookie(),
            &[("csrf_token", &token), ("idempotency_key", key)],
        )
        .await;
        let (missing_status, _, missing_body) = post_form(
            &f.app,
            &missing_remove,
            &f.outsider.cookie(),
            &[("csrf_token", &token), ("idempotency_key", key)],
        )
        .await;
        assert_eq!(
            existing_status,
            StatusCode::NOT_FOUND,
            "page remove: {existing_body}"
        );
        assert_eq!(
            missing_status,
            StatusCode::NOT_FOUND,
            "page remove: {missing_body}"
        );
        assert_eq!(
            existing_body, missing_body,
            "the page refusal must be masked"
        );
    }

    let existing_role = format!(
        "/organizations/{}/members/{}/role",
        f.org_slug, victim.username
    );
    let missing_role = format!("/organizations/{absent}/members/{}/role", victim.username);
    let (existing_status, _, existing_body) = post_form(
        &f.app,
        &existing_role,
        &f.outsider.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", "bad key with spaces"),
            ("role", "owner"),
        ],
    )
    .await;
    let (missing_status, _, missing_body) = post_form(
        &f.app,
        &missing_role,
        &f.outsider.cookie(),
        &[
            ("csrf_token", &token),
            ("idempotency_key", "bad key with spaces"),
            ("role", "owner"),
        ],
    )
    .await;
    assert_eq!(
        existing_status,
        StatusCode::NOT_FOUND,
        "page role: {existing_body}"
    );
    assert_eq!(
        missing_status,
        StatusCode::NOT_FOUND,
        "page role: {missing_body}"
    );
    assert_eq!(
        existing_body, missing_body,
        "the page refusal must be masked"
    );

    assert_eq!(
        membership_role(&f.pool, f.org_id, victim.id)
            .await
            .as_deref(),
        Some("member")
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn demoted_member_is_denied_on_api_ui_http_and_ssh() {
    let f = fixture().await;
    let repo_api = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, f.repo_name
    );
    let repo_page = format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name);

    // Promote with no direct repository grant, so only the organization role
    // decides admission.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = api(&f.app, "GET", &repo_api, Some(&f.member.token), None, None).await;
    assert_eq!(status, StatusCode::OK, "promoted owner API: {body}");
    let (status, body) = get_page(&f.app, &repo_page, Some(&f.member.cookie())).await;
    assert_eq!(status, StatusCode::OK, "promoted owner page: {body}");

    let port = serve(&f.app).await;
    let url = format!(
        "http://{}:testpass@127.0.0.1:{port}/{}/{}.git",
        f.member.username, f.org_slug, f.repo_name
    );
    git_ok(git_cmd(&["ls-remote", "--", &url])).await;
    let ssh = ssh_client(&f, f.member.id).await;
    if let Some(ssh) = &ssh {
        git_ok(ssh.ls_remote()).await;
    }

    // Demote; the next admission on all four transports is refused.
    let (status, body) = api(
        &f.app,
        "PATCH",
        &member_path(&f, &f.member.username),
        Some(&f.owner.token),
        None,
        Some(&json!({"role": "member"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = api(&f.app, "GET", &repo_api, Some(&f.member.token), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "demoted API: {body}");
    let (status, body) = get_page(&f.app, &repo_page, Some(&f.member.cookie())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "demoted page: {body}");
    let stderr = git_fails(git_cmd(&["ls-remote", "--", &url])).await;
    assert!(
        stderr.contains("not found") || stderr.contains("not permitted") || stderr.contains("403"),
        "unexpected HTTP refusal: {stderr}"
    );
    if let Some(ssh) = &ssh {
        let stderr = git_fails(ssh.ls_remote()).await;
        assert!(
            stderr.contains("repository not found"),
            "unexpected SSH refusal: {stderr}"
        );
    }

    let direct: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_id = $1 AND subject_id = $2 \
         AND repository_id IS NOT NULL AND revoked_at IS NULL",
    )
    .bind(f.org_id)
    .bind(f.member.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(direct, 0, "no direct grant can mask the demotion");
    finish(f).await;
}
