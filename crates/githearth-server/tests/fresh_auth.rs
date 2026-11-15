//! M4.1 U22: the fresh-authentication agreement test.
//!
//! Five tests pin the decision recorded on 2026-11-15 in ADRs 0002 and 0003:
//!
//! 1. `git_transports_agree_on_force_push_and_delete` drives the same force
//!    push and the same branch deletion over Git HTTP (a personal access
//!    token) and Git SSH (an SSH key), as the same actor with the same grant,
//!    and asserts the transports decide identically for an unprotected branch,
//!    a protected branch and a caller without the write grant. The actor's
//!    sessions are aged past a deliberately tiny configured window first, so
//!    the test also proves Git does not consult the window at all.
//! 2. `stale_session_is_refused_then_succeeds_after_reauthentication` ages a
//!    session past the configured window and exercises both the JSON API and
//!    the browser UI: the sensitive action is refused with the re-authentication
//!    prompt and succeeds after the password is presented.
//! 3. `fresh_auth_exception_is_scoped_to_git` keeps the same stale session and
//!    shows the account's API call is still refused with the re-authentication
//!    envelope while its Git token may force push and delete, so the exception
//!    belongs to the transport, not to the account.
//! 4. `every_sensitive_surface_returns_the_reauthentication_envelope` asks
//!    every API and page surface that refuses a `sensitive` action, so the fix
//!    is not shaped around `repository.archive`.
//! 5. `a_grantless_or_routine_caller_does_not_get_a_reauthentication_prompt`
//!    keeps the ordinary 403/404 for a caller without the grant and leaves a
//!    non-sensitive action available to a stale session.
//!
//! Staleness is deterministic: the fixture builds the authorization engine with
//! `Engine::with_fresh_auth` and ages `sessions.authenticated_at` in SQL. No
//! test waits on wall-clock time.
//!
//! Every test is `#[ignore]` because it needs a disposable PostgreSQL database
//! via `GITHEARTH_TEST_DATABASE_URL` and, for the agreement test, the system
//! `ssh` and `ssh-keygen` clients.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_authz::Engine;
use githearth_git::layout::ObjectFormat;
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router, ssh};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "fresh-auth-password-1";
const WINDOW: Duration = Duration::from_secs(30);
/// Older than `WINDOW` but *inside* the default 15-minute window, so a refusal
/// proves the fixture's 30-second window is the one in force.
const AGE_SECONDS: i64 = 300;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL to a disposable database");
    let (origin, _) = url.rsplit_once('/').expect("URL must end in database name");
    let maint = PgPool::connect(&format!("{origin}/postgres"))
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

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// One isolated server: its own database, storage root, config and, unlike
/// production, an authorization engine whose fresh-auth window the test picks.
struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    config: Config,
    storage_root: PathBuf,
}

impl Fixture {
    async fn new(window: Duration) -> Self {
        let db_name = format!(
            "githearth_fa_{}",
            Uuid::new_v4().to_string().replace('-', "")
        );
        let database_url = fresh_database(&db_name).await;
        let short = Uuid::new_v4().simple().to_string();
        let storage_root = std::env::temp_dir().join(format!("ghfa{}", &short[..12]));
        std::fs::create_dir_all(&storage_root).unwrap();
        let ssh_host_key_path = storage_root.join("ssh_host_key");
        ssh::generate_host_key(&ssh_host_key_path).unwrap();
        let config = Config {
            database_url,
            database_max_connections: 4,
            database_timeout_seconds: 3,
            storage_root: storage_root.clone(),
            // The hooks must exec the real server binary, not this test harness.
            hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
            git_timeout_seconds: 120,
            ssh_enabled: true,
            ssh_bind: "127.0.0.1:0".parse().unwrap(),
            ssh_host_key_path,
            ..Config::default()
        };
        let pool = connect(&config).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let mut state = AppState::new(pool.clone(), config.clone()).await.unwrap();
        // The API and browser pages read the window from this engine. The
        // installed hook socket keeps the engine AppState::new built, which is
        // harmless: `decide_ref` passes a recency of zero for every Git update,
        // which is the behaviour under test.
        state.authz = Arc::new(Engine::new().unwrap().with_fresh_auth(window));
        let app = router(state.clone());
        Self {
            app,
            state,
            pool,
            config,
            storage_root,
        }
    }

    async fn finish(self) {
        let url = self.config.database_url.clone();
        let _ = std::fs::remove_dir_all(&self.storage_root);
        self.pool.close().await;
        if let Some((origin, name)) = url.rsplit_once('/')
            && let Ok(maint) = PgPool::connect(&format!("{origin}/postgres")).await
        {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&maint)
                .await;
            maint.close().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Database fixtures
// ---------------------------------------------------------------------------

async fn insert_account(pool: &PgPool, is_admin: bool) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let username = tag("fa-user");
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(&username)
    .bind(hash_password(PASSWORD).unwrap())
    .bind(is_admin)
    .execute(pool)
    .await
    .unwrap();
    (id, username)
}

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("fa-org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fresh Auth Org')")
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

async fn create_active_repo(pool: &PgPool, org_id: Uuid, owner: Uuid) -> (Uuid, String) {
    let repo_id = Uuid::new_v4();
    let name = tag("fa-repo");
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind(&name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    (repo_id, name)
}

/// Adds an organization member with one repository-scoped grant. `None` leaves
/// the member without a repository grant.
async fn member(pool: &PgPool, org_id: Uuid, repo_id: Uuid, role: Option<&str>) -> (Uuid, String) {
    let (account, username) = insert_account(pool, false).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    if let Some(role) = role {
        sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) VALUES ($1,$2,'account',$3,'repository',$4,$5,$3)")
            .bind(Uuid::new_v4())
            .bind(role)
            .bind(account)
            .bind(org_id)
            .bind(repo_id)
            .execute(pool)
            .await
            .unwrap();
    }
    (account, username)
}

/// An organization-scope grant, enough to read the organization without
/// holding any repository or administration grant.
async fn org_grant(pool: &PgPool, org_id: Uuid, account: Uuid, role: &str) {
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,$2,'account',$3,'organization',$4,$3)",
    )
    .bind(Uuid::new_v4())
    .bind(role)
    .bind(account)
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn add_protection_rule(pool: &PgPool, repo_id: Uuid, created_by: Uuid) {
    sqlx::query(
        "INSERT INTO branch_protection_rules \
         (id, repository_id, pattern, allow_force_push, allow_deletion, require_pull_request, \
          required_approvals, required_status_checks, created_by) \
         VALUES ($1,$2,'main',false,false,false,0,'{}',$3)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(created_by)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO protection_generations (repository_id, generation) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET generation = protection_generations.generation + 1",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Age every session of `account` so the configured window is unambiguously
/// exceeded, then return the largest resulting age for the assertion below.
async fn age_sessions(pool: &PgPool, account: Uuid, seconds: i64) -> i64 {
    sqlx::query(
        "UPDATE sessions SET authenticated_at = clock_timestamp() - ($2 * interval '1 second') \
         WHERE account_id = $1",
    )
    .bind(account)
    .bind(seconds)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "SELECT COALESCE(MAX(EXTRACT(EPOCH FROM clock_timestamp() - authenticated_at)::bigint), 0) \
         FROM sessions WHERE account_id = $1",
    )
    .bind(account)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// HTTP API / browser helpers
// ---------------------------------------------------------------------------

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn json_request(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn login(f: &Fixture, username: &str) -> String {
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/login",
            None,
            Some(serde_json::json!({"username": username, "password": PASSWORD})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn create_token(f: &Fixture, session: &str, scopes: &[&str]) -> String {
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            "/api/v1/user/tokens",
            Some(session),
            Some(serde_json::json!({"name": tag("fa-token"), "scopes": scopes})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn post_form(path: &str, cookie: &str, fields: &[(&str, &str)]) -> Request<Body> {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", cookie)
        .body(Body::from(body))
        .unwrap()
}

fn get_page(path: &str, cookie: &str) -> Request<Body> {
    Request::get(path)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap()
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn csrf_from(page: &str) -> String {
    let marker = "name=\"csrf_token\" value=\"";
    let start = page.find(marker).expect("page must carry a CSRF field") + marker.len();
    page[start..].split('"').next().unwrap().to_owned()
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
        Err(_) => panic!("git command timed out"),
    }
}

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn check_ok(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "git failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn seed_bare(repo_path: &Path, work_dir: &Path, refspec: &str) {
    let repo = repo_path.to_string_lossy();
    let work = work_dir.to_string_lossy();
    git_ok(git_cmd(&[
        "--git-dir",
        &repo,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        &work,
        refspec,
    ]))
    .await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    allowed: bool,
    reason: String,
}

/// A Git endpoint and the credential that admits it: a personal access token
/// in the HTTP URL, or a client key for SSH.
#[derive(Clone)]
struct Transport {
    url: String,
    ssh_command: Option<String>,
}

impl Transport {
    fn http(port: u16, org: &str, repo: &str, token: &str) -> Self {
        Self {
            url: format!("http://git:{token}@127.0.0.1:{port}/{org}/{repo}.git"),
            ssh_command: None,
        }
    }

    fn ssh(port: u16, org: &str, repo: &str, key: &Path) -> Self {
        Self {
            url: format!("ssh://git@127.0.0.1:{port}/{org}/{repo}.git"),
            ssh_command: Some(format!(
                "ssh -i {} -p {port} -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
                key.display()
            )),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = git_cmd(args);
        if let Some(ssh) = &self.ssh_command {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd
    }

    async fn clone(&self, dir: &Path) {
        let dir = dir.to_string_lossy();
        let out = git_run(self.command(&["clone", "--", &self.url, &dir])).await;
        assert!(
            out.status.success(),
            "clone over {} failed: {}",
            self.url,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    async fn at(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        let dir = dir.to_string_lossy();
        let mut full = vec!["-C", dir.as_ref()];
        full.extend_from_slice(args);
        git_run(self.command(&full)).await
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{label}-{}", Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn commit_file(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
    git_ok(git_cmd(&["-C", &dir.to_string_lossy(), "add", name])).await;
    git_ok(git_cmd(&[
        "-C",
        &dir.to_string_lossy(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        content,
    ]))
    .await;
}

/// Clone, put two commits on a new branch and push both, then rewind and force
/// push. The branch must be left at its first commit if the force push lands.
async fn force_push_new_branch(t: &Transport, branch: &str) -> Outcome {
    let dir = temp_dir("fa-force");
    t.clone(&dir).await;
    check_ok(&t.at(&dir, &["checkout", "-b", branch]).await);
    commit_file(&dir, "one", "one").await;
    check_ok(&t.at(&dir, &["push", "origin", branch]).await);
    commit_file(&dir, "two", "two").await;
    check_ok(&t.at(&dir, &["push", "origin", branch]).await);
    check_ok(&t.at(&dir, &["reset", "--hard", "HEAD~1"]).await);
    let out = t.at(&dir, &["push", "--force", "origin", branch]).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome(&out)
}

async fn delete_branch(t: &Transport, branch: &str) -> Outcome {
    let dir = temp_dir("fa-del");
    t.clone(&dir).await;
    let out = t.at(&dir, &["push", "origin", "--delete", branch]).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome(&out)
}

/// Clone the protected `main`, add a fast-forward commit, rewind and force push.
async fn force_push_main(t: &Transport) -> Outcome {
    let dir = temp_dir("fa-main-force");
    t.clone(&dir).await;
    // A unique file: the first transport's refused force push leaves the
    // fast-forward commit on the remote, so a fixed name would be unchanged in
    // the second clone and produce no force push to refuse.
    let change = tag("main-change");
    commit_file(&dir, &change, &change).await;
    check_ok(&t.at(&dir, &["push", "origin", "main"]).await);
    check_ok(&t.at(&dir, &["reset", "--hard", "HEAD~1"]).await);
    let out = t.at(&dir, &["push", "--force", "origin", "main"]).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome(&out)
}

async fn delete_main(t: &Transport) -> Outcome {
    let dir = temp_dir("fa-main-del");
    t.clone(&dir).await;
    let out = t.at(&dir, &["push", "origin", "--delete", "main"]).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome(&out)
}

/// A caller with read access but no write grant: rewinds an existing branch
/// and force pushes it, then tries to delete it.
async fn force_push_existing(t: &Transport, branch: &str) -> Outcome {
    let dir = temp_dir("fa-nogrant-force");
    t.clone(&dir).await;
    check_ok(&t.at(&dir, &["reset", "--hard", "HEAD~1"]).await);
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = t.at(&dir, &["push", "--force", "origin", &refspec]).await;
    let _ = std::fs::remove_dir_all(&dir);
    outcome(&out)
}

fn outcome(output: &std::process::Output) -> Outcome {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .find_map(|line| {
            // The pre-receive hook writes its refusal to stderr, which Git
            // forwards with a `remote:` prefix on both transports.
            let rest = line.trim_start().strip_prefix("remote:")?.trim();
            (rest.contains("ref.") || rest.starts_with("protected ")).then(|| rest.to_owned())
        })
        .or_else(|| {
            if stderr.contains("not permitted") || stderr.contains("403") {
                Some("not permitted".to_owned())
            } else if stderr.contains("repository not found") {
                Some("not found".to_owned())
            } else {
                None
            }
        })
        .unwrap_or_else(|| stderr.lines().last().unwrap_or("").trim().to_owned());
    Outcome {
        allowed: output.status.success(),
        reason,
    }
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

fn has_ssh() -> bool {
    Command::new("ssh").arg("-V").output().is_ok()
        && Command::new("ssh-keygen").arg("-?").output().is_ok()
}

fn generate_client_key(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let out = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "githearth-test",
            "-q",
        ])
        .output()
        .expect("ssh-keygen failed");
    assert!(
        out.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn sha256_fingerprint(key: &Path) -> String {
    let out = Command::new("ssh-keygen")
        .args(["-lf", &key.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

async fn serve_http(app: &Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    port
}

// ---------------------------------------------------------------------------
// Test 1: the agreement matrix
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL and ssh; see docs/foundation.md"]
async fn git_transports_agree_on_force_push_and_delete() {
    if !has_ssh() {
        eprintln!("SKIP: ssh or ssh-keygen not found");
        return;
    }

    let f = Fixture::new(WINDOW).await;
    let (owner, _owner_name) = insert_account(&f.pool, false).await;
    let (org_id, org_slug) = fixture_organization(&f.pool, owner).await;
    let (repo_id, repo_name) = create_active_repo(&f.pool, org_id, owner).await;
    let repo_path = f
        .state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    // One commit on main, then a rule that forbids force push and deletion.
    let seed = temp_dir("fa-seed");
    git_ok(git_cmd(&[
        "-C",
        &seed.to_string_lossy(),
        "init",
        "-b",
        "main",
    ]))
    .await;
    commit_file(&seed, "README.md", "# seed").await;
    commit_file(&seed, "second", "second").await;
    seed_bare(&repo_path, &seed, "main:refs/heads/main").await;
    // A second branch at the same two commits, for the caller without write
    // access: rewinding it one commit makes a genuine non-fast-forward update.
    seed_bare(&repo_path, &seed, "main:refs/heads/outsider-target").await;
    let _ = std::fs::remove_dir_all(&seed);
    add_protection_rule(&f.pool, repo_id, owner).await;

    // The actor holding the `write` grant and the caller holding only `read`.
    let (writer_id, writer_name) = member(&f.pool, org_id, repo_id, Some("write")).await;
    let (outsider_id, outsider_name) = member(&f.pool, org_id, repo_id, Some("read")).await;

    // A token (HTTP) and a key (SSH) are the two credentials U22 names. Create
    // the token while the session is still fresh, then age every session.
    let writer_session = login(&f, &writer_name).await;
    let writer_token = create_token(&f, &writer_session, &["repo:write"]).await;
    let outsider_session = login(&f, &outsider_name).await;
    let outsider_token = create_token(&f, &outsider_session, &["repo:read"]).await;

    let writer_age = age_sessions(&f.pool, writer_id, AGE_SECONDS).await;
    let outsider_age = age_sessions(&f.pool, outsider_id, AGE_SECONDS).await;
    assert!(
        writer_age > WINDOW.as_secs() as i64 && outsider_age > WINDOW.as_secs() as i64,
        "sessions must be older than the configured window (writer {writer_age}s, outsider {outsider_age}s)"
    );

    // Git HTTP, with the actor's token.
    let http_port = serve_http(&f.app).await;
    // Git SSH, with one key per actor.
    let key_dir = temp_dir("fa-keys");
    let writer_key = generate_client_key(&key_dir, "writer_ed25519");
    let outsider_key = generate_client_key(&key_dir, "outsider_ed25519");
    let writer_fp = sha256_fingerprint(&writer_key);
    let outsider_fp = sha256_fingerprint(&outsider_key);
    let authenticate: ssh::Authenticate = Arc::new(move |fp: String| {
        let result = if fp == writer_fp {
            Ok(Some(ssh::SshIdentity {
                account: writer_id,
                service_account: None,
                key_id: Uuid::new_v4(),
                read_only: false,
            }))
        } else if fp == outsider_fp {
            Ok(Some(ssh::SshIdentity {
                account: outsider_id,
                service_account: None,
                key_id: Uuid::new_v4(),
                read_only: false,
            }))
        } else {
            Ok(None)
        };
        Box::pin(async move { result })
    });
    let (_ssh_handle, port_rx) = ssh::spawn(f.state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();

    let writer_http = Transport::http(http_port, &org_slug, &repo_name, &writer_token);
    let writer_ssh = Transport::ssh(ssh_port, &org_slug, &repo_name, &writer_key);
    let outsider_http = Transport::http(http_port, &org_slug, &repo_name, &outsider_token);
    let outsider_ssh = Transport::ssh(ssh_port, &org_slug, &repo_name, &outsider_key);

    // --- a write grant with no branch protection: both transports allow. ---
    let http_force = force_push_new_branch(&writer_http, "http-unprotected").await;
    let ssh_force = force_push_new_branch(&writer_ssh, "ssh-unprotected").await;
    assert!(http_force.allowed, "HTTP force push: {http_force:?}");
    assert!(ssh_force.allowed, "SSH force push: {ssh_force:?}");
    let http_delete = delete_branch(&writer_http, "http-unprotected").await;
    let ssh_delete = delete_branch(&writer_ssh, "ssh-unprotected").await;
    assert!(http_delete.allowed, "HTTP delete: {http_delete:?}");
    assert!(ssh_delete.allowed, "SSH delete: {ssh_delete:?}");

    // --- a protected branch forbids force push: both refuse, same reason. ---
    let http_protected_force = force_push_main(&writer_http).await;
    let ssh_protected_force = force_push_main(&writer_ssh).await;
    assert_eq!(
        http_protected_force, ssh_protected_force,
        "protected force push must decide identically on HTTP and SSH"
    );
    assert!(!http_protected_force.allowed);
    assert_eq!(
        http_protected_force.reason, "protected branch: ref.force_push",
        "{http_protected_force:?}"
    );

    // --- a protected branch forbids deletion: both refuse, same reason. ---
    let http_protected_delete = delete_main(&writer_http).await;
    let ssh_protected_delete = delete_main(&writer_ssh).await;
    assert_eq!(
        http_protected_delete, ssh_protected_delete,
        "protected deletion must decide identically on HTTP and SSH"
    );
    assert!(!http_protected_delete.allowed);
    assert_eq!(
        http_protected_delete.reason, "protected branch: ref.delete",
        "{http_protected_delete:?}"
    );

    // --- a caller without the write grant: both refuse. ---
    let http_no_grant_force = force_push_existing(&outsider_http, "outsider-target").await;
    let ssh_no_grant_force = force_push_existing(&outsider_ssh, "outsider-target").await;
    assert!(!http_no_grant_force.allowed, "{http_no_grant_force:?}");
    assert!(!ssh_no_grant_force.allowed, "{ssh_no_grant_force:?}");
    assert_eq!(
        http_no_grant_force, ssh_no_grant_force,
        "a caller without the grant must be refused identically"
    );
    let http_no_grant_delete = delete_branch(&outsider_http, "outsider-target").await;
    let ssh_no_grant_delete = delete_branch(&outsider_ssh, "outsider-target").await;
    assert!(!http_no_grant_delete.allowed, "{http_no_grant_delete:?}");
    assert!(!ssh_no_grant_delete.allowed, "{ssh_no_grant_delete:?}");
    assert_eq!(
        http_no_grant_delete, ssh_no_grant_delete,
        "a caller without the grant must be refused identically"
    );

    // Neither transport ever asked for a re-authentication prompt, even though
    // the actor's sessions are stale under the configured window.
    for case in [
        &http_force,
        &ssh_force,
        &http_protected_force,
        &ssh_protected_force,
        &http_no_grant_force,
        &ssh_no_grant_force,
    ] {
        let text = format!("{case:?}").to_lowercase();
        assert!(!text.contains("reauthent"), "{case:?}");
        assert!(!text.contains("fresh"), "{case:?}");
    }

    let _ = std::fs::remove_dir_all(&key_dir);
    f.finish().await;
}

// ---------------------------------------------------------------------------
// Test 2: the browser and API half
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_session_is_refused_then_succeeds_after_reauthentication() {
    let f = Fixture::new(WINDOW).await;
    let (admin_id, admin_name) = insert_account(&f.pool, true).await;
    let session = login(&f, &admin_name).await;

    // 300 seconds is inside the default 15-minute window: only the configured
    // 30-second window can refuse it.
    let age = age_sessions(&f.pool, admin_id, AGE_SECONDS).await;
    assert!(age > WINDOW.as_secs() as i64, "session age {age}s");

    // A routine action still works; the session is valid, just stale.
    assert_eq!(
        send(
            &f.app,
            json_request("GET", "/api/v1/me", Some(&session), None)
        )
        .await
        .0,
        StatusCode::OK
    );

    // --- JSON API: refused with the prompt, then allowed after re-auth. ---
    let username = tag("fa-api-created");
    let create = |token: &str, username: &str| {
        json_request(
            "POST",
            "/api/v1/admin/accounts",
            Some(token),
            Some(serde_json::json!({"username": username, "password": PASSWORD})),
        )
    };
    let (status, _, body) = send(&f.app, create(&session, &username)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(denial["error"], "reauthentication required");
    assert_eq!(denial["reason"], "sensitive");

    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                "/api/v1/me/reauthenticate",
                Some(&session),
                Some(serde_json::json!({"password": PASSWORD})),
            )
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&f.app, create(&session, &username)).await.0,
        StatusCode::CREATED
    );

    // --- Browser UI: the same window, the same prompt, the same rerun. ---
    age_sessions(&f.pool, admin_id, AGE_SECONDS).await;
    let cookie = format!("githearth_session={session}");
    let (status, _, page) = send(&f.app, get_page("/admin/accounts", &cookie)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(page.contains("Confirm your password"), "{page}");
    let csrf = csrf_from(&page);

    let (status, headers, _) = send(
        &f.app,
        post_form(
            "/settings/reauthenticate",
            &cookie,
            &[
                ("csrf_token", &csrf),
                ("password", PASSWORD),
                ("return_to", "/admin/accounts"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/admin/accounts"
    );

    let page_user = tag("fa-page-created");
    let (status, headers, body) = send(
        &f.app,
        post_form(
            "/admin/accounts",
            &cookie,
            &[
                ("csrf_token", &csrf),
                ("username", &page_user),
                ("password", PASSWORD),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        "/admin/accounts?notice=created"
    );

    f.finish().await;
}

// ---------------------------------------------------------------------------
// Test 3: the negative — the exception is scoped to Git, not the account
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fresh_auth_exception_is_scoped_to_git() {
    let f = Fixture::new(WINDOW).await;
    // The actor owns the repository, so the API refusal below is freshness, not
    // permission.
    let (owner_id, owner_name) = insert_account(&f.pool, false).await;
    let (org_id, org_slug) = fixture_organization(&f.pool, owner_id).await;
    let (repo_id, repo_name) = create_active_repo(&f.pool, org_id, owner_id).await;
    let repo_path = f
        .state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let seed = temp_dir("fa-scope-seed");
    git_ok(git_cmd(&[
        "-C",
        &seed.to_string_lossy(),
        "init",
        "-b",
        "main",
    ]))
    .await;
    commit_file(&seed, "README.md", "# seed").await;
    seed_bare(&repo_path, &seed, "main:refs/heads/main").await;
    let _ = std::fs::remove_dir_all(&seed);

    let session = login(&f, &owner_name).await;
    let token = create_token(&f, &session, &["repo:write"]).await;
    let age = age_sessions(&f.pool, owner_id, AGE_SECONDS).await;
    assert!(age > WINDOW.as_secs() as i64, "session age {age}s");

    // The session is valid for routine calls...
    assert_eq!(
        send(
            &f.app,
            json_request("GET", "/api/v1/me", Some(&session), None)
        )
        .await
        .0,
        StatusCode::OK
    );

    // ...but the same actor's sensitive API action is refused as stale. The
    // owner has the permission; only freshness is missing.
    let archive = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/archive");
    let (status, _, body) =
        send(&f.app, json_request("POST", &archive, Some(&session), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    // The owner holds `repository.archive`; only freshness is missing, so the
    // denial names re-authentication instead of the generic refusal.
    assert_eq!(denial["error"], "reauthentication required", "{body}");
    assert_eq!(denial["reason"], "sensitive", "{body}");

    // A second sensitive action on a different surface (membership, not the
    // repository API) carries the same envelope, so the refusal is not shaped
    // around one route.
    let (target_id, target_name) = insert_account(&f.pool, false).await;
    let add_member = format!("/api/v1/organizations/{org_slug}/members");
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &add_member,
            Some(&session),
            Some(serde_json::json!({"username": target_name, "role": "member"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(denial["error"], "reauthentication required", "{body}");
    assert_eq!(denial["reason"], "sensitive", "{body}");
    // The refused add did not create the membership.
    let is_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
    )
    .bind(org_id)
    .bind(target_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(!is_member);

    // The same account's Git token may still force push and delete, proving the
    // exception belongs to the Git transports rather than to the account.
    let port = serve_http(&f.app).await;
    let git = Transport::http(port, &org_slug, &repo_name, &token);
    let force = force_push_new_branch(&git, "scoped-branch").await;
    assert!(
        force.allowed,
        "Git force push must remain allowed: {force:?}"
    );
    let delete = delete_branch(&git, "scoped-branch").await;
    assert!(delete.allowed, "Git delete must remain allowed: {delete:?}");

    // Presenting the password restores the API action, so the refusal above was
    // freshness and nothing else.
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                "/api/v1/me/reauthenticate",
                Some(&session),
                Some(serde_json::json!({"password": PASSWORD})),
            )
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&f.app, json_request("POST", &archive, Some(&session), None))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &add_member,
                Some(&session),
                Some(serde_json::json!({"username": target_name, "role": "member"})),
            )
        )
        .await
        .0,
        StatusCode::CREATED
    );

    f.finish().await;
}

// ---------------------------------------------------------------------------
// Test 4: every sensitive surface keeps the freshness denial
// ---------------------------------------------------------------------------

/// One stale owner, one envelope, every surface that refuses a `sensitive`
/// action: the repository API (archive, delete, transfer, restore),
/// membership, teams, invitations and the audit read (gated on
/// `organization.update`). Each refusal must name re-authentication, the
/// browser membership page must show the same prompt, and none may change a
/// row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn every_sensitive_surface_returns_the_reauthentication_envelope() {
    let f = Fixture::new(WINDOW).await;
    let (owner_id, owner_name) = insert_account(&f.pool, false).await;
    let (org_id, org_slug) = fixture_organization(&f.pool, owner_id).await;
    let (repo_id, repo_name) = create_active_repo(&f.pool, org_id, owner_id).await;
    let (_target_id, target_name) = insert_account(&f.pool, false).await;
    let (_target_org_id, target_org_slug) = fixture_organization(&f.pool, owner_id).await;
    let deleted_id = Uuid::new_v4();
    let deleted_name = tag("fa-deleted");
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
         VALUES ($1,$2,$3,$4,$5,'deleted','private')",
    )
    .bind(deleted_id)
    .bind(org_id)
    .bind(&deleted_name)
    .bind(owner_id)
    .bind(Uuid::new_v4())
    .execute(&f.pool)
    .await
    .unwrap();
    let session = login(&f, &owner_name).await;
    let age = age_sessions(&f.pool, owner_id, AGE_SECONDS).await;
    assert!(age > WINDOW.as_secs() as i64, "session age {age}s");

    let archive = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/archive");
    let delete = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}");
    let transfer = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/transfer");
    let restore = format!("/api/v1/organizations/{org_slug}/repositories/{deleted_name}/restore");
    let members = format!("/api/v1/organizations/{org_slug}/members");
    let teams = format!("/api/v1/organizations/{org_slug}/teams");
    let invitations = format!("/api/v1/organizations/{org_slug}/invitations");
    let audit = format!("/api/v1/organizations/{org_slug}/audit");
    let cases = [
        ("POST", archive.as_str(), None),
        (
            "POST",
            members.as_str(),
            Some(serde_json::json!({"username": target_name, "role": "member"})),
        ),
        (
            "POST",
            teams.as_str(),
            Some(serde_json::json!({"slug": tag("fa-team"), "name": "Fresh Auth Team"})),
        ),
        (
            "POST",
            invitations.as_str(),
            Some(serde_json::json!({"email": "invite@example.com", "role": "member"})),
        ),
        ("GET", audit.as_str(), None),
        (
            "POST",
            transfer.as_str(),
            Some(
                serde_json::json!({"organization": target_org_slug, "fork_pull_requests": "refuse"}),
            ),
        ),
        ("POST", restore.as_str(), None),
        ("DELETE", delete.as_str(), None),
    ];
    for (method, path, body) in cases {
        let (status, _, body) =
            send(&f.app, json_request(method, path, Some(&session), body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
        let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            denial["error"], "reauthentication required",
            "{method} {path}: {body}"
        );
        assert_eq!(denial["reason"], "sensitive", "{method} {path}: {body}");
    }

    // The browser keeps the same reason: the members, teams and transfer pages
    // show the password prompt instead of a bare 403.
    let cookie = format!("githearth_session={session}");
    let members_page = format!("/organizations/{org_slug}/members");
    let teams_page = format!("/organizations/{org_slug}/teams");
    let transfer_page = format!("/organizations/{org_slug}/repositories/{repo_name}/transfer");
    for page_path in [&members_page, &teams_page, &transfer_page] {
        let (status, _, page) = send(&f.app, get_page(page_path, &cookie)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page_path}: {page}");
        assert!(
            page.contains("Confirm your password"),
            "{page_path}: {page}"
        );
        assert!(
            page.contains("Reauthentication required"),
            "{page_path}: {page}"
        );
        assert!(
            page.contains(&format!("name=\"return_to\" value=\"{page_path}\"")),
            "{page_path}: {page}"
        );
    }

    // The restore form's submit is the same refusal: the prompt comes from the
    // API envelope, not from a page-local freshness test.
    let (_, _, members_html) = send(&f.app, get_page(&members_page, &cookie)).await;
    let csrf = csrf_from(&members_html);
    let restore_key = Uuid::new_v4().simple().to_string();
    let (status, _, page) = send(
        &f.app,
        post_form(
            &format!("/organizations/{org_slug}/repositories/{deleted_name}/restore"),
            &cookie,
            &[("csrf_token", &csrf), ("idempotency_key", &restore_key)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");
    assert!(page.contains("Confirm your password"), "{page}");
    assert!(page.contains("Reauthentication required"), "{page}");

    // Nothing was created, deleted or archived by a refused call.
    let archived: bool = sqlx::query_scalar("SELECT archived FROM repositories WHERE id = $1")
        .bind(repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!archived);
    let team_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM teams WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(team_count, 0);
    let invitation_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invitations WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(invitation_count, 0);
    let member_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE organization_id = $1")
            .bind(org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(member_count, 1);
    let source_org: Uuid =
        sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
            .bind(repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(source_org, org_id);
    let deleted_status: String = sqlx::query_scalar("SELECT status FROM repositories WHERE id=$1")
        .bind(deleted_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(deleted_status, "deleted");

    // Presenting the password restores the same actions, so each refusal was
    // freshness and nothing else.
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                "/api/v1/me/reauthenticate",
                Some(&session),
                Some(serde_json::json!({"password": PASSWORD})),
            )
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&f.app, json_request("POST", &archive, Some(&session), None))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &members,
                Some(&session),
                Some(serde_json::json!({"username": target_name, "role": "member"})),
            )
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        send(
            &f.app,
            json_request(
                "POST",
                &teams,
                Some(&session),
                Some(serde_json::json!({"slug": tag("fa-team"), "name": "Fresh Auth Team"})),
            )
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let (status, _, page) = send(&f.app, get_page(&members_page, &cookie)).await;
    assert_eq!(status, StatusCode::OK, "{page}");

    f.finish().await;
}

// ---------------------------------------------------------------------------
// Test 5: the envelope does not spread to ordinary refusals or routine actions
// ---------------------------------------------------------------------------

/// A stale session keeps every other answer: a caller without the sensitive
/// grant still gets the ordinary 403 (or the masked 404), and a routine action
/// still succeeds without a password prompt.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_grantless_or_routine_caller_does_not_get_a_reauthentication_prompt() {
    let f = Fixture::new(WINDOW).await;
    let (owner_id, owner_name) = insert_account(&f.pool, false).await;
    let (org_id, org_slug) = fixture_organization(&f.pool, owner_id).await;
    let (repo_id, repo_name) = create_active_repo(&f.pool, org_id, owner_id).await;
    // A reader: it may read the organization and the repository, but holds
    // neither `repository.archive` nor `organization.manage_members`.
    let (reader_id, reader_name) = member(&f.pool, org_id, repo_id, Some("read")).await;
    org_grant(&f.pool, org_id, reader_id, "member").await;
    // A member with no grant at all: neither the organization nor the private
    // repository is readable, so the refusal is the masked 404.
    let (outsider_id, outsider_name) = member(&f.pool, org_id, repo_id, None).await;
    let (target_id, target_name) = insert_account(&f.pool, false).await;

    let owner_session = login(&f, &owner_name).await;
    let reader_session = login(&f, &reader_name).await;
    let outsider_session = login(&f, &outsider_name).await;
    for id in [owner_id, reader_id, outsider_id] {
        age_sessions(&f.pool, id, AGE_SECONDS).await;
    }

    // A routine action by the stale owner still succeeds: only `sensitive`
    // actions ask for a fresh password.
    let repo_path = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}");
    let (status, _, body) = send(
        &f.app,
        json_request(
            "PATCH",
            &repo_path,
            Some(&owner_session),
            Some(serde_json::json!({"description": "routine, still allowed"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A reader without the sensitive grant gets the ordinary refusal, not a
    // prompt that would reveal the action is otherwise available to it.
    let archive = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}/archive");
    let (status, _, body) = send(
        &f.app,
        json_request("POST", &archive, Some(&reader_session), None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, r#"{"error":"not permitted"}"#, "{body}");

    // The same reader may not administer members: also the ordinary 403.
    let members = format!("/api/v1/organizations/{org_slug}/members");
    let (status, _, body) = send(
        &f.app,
        json_request(
            "POST",
            &members,
            Some(&reader_session),
            Some(serde_json::json!({"username": target_name, "role": "member"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, r#"{"error":"not permitted"}"#, "{body}");

    // A member with no readable organization gets the masked 404, never the
    // prompt.
    let (status, _, body) = send(
        &f.app,
        json_request("POST", &archive, Some(&outsider_session), None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body, r#"{"error":"not found"}"#, "{body}");

    // Nothing was archived or added by the refused calls.
    let archived: bool = sqlx::query_scalar("SELECT archived FROM repositories WHERE id = $1")
        .bind(repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!archived);
    let is_member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE organization_id = $1 AND account_id = $2)",
    )
    .bind(org_id)
    .bind(target_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(!is_member);

    f.finish().await;
}
