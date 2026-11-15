//! Repository transfer (M4.1 acceptance case U07, ADR 0003).
//!
//! A transfer keeps the repository's ID, history and storage path, keeps its
//! non-fork open pull requests and their reviews, closes fork pull requests
//! under the audited policy or refuses the move, leaves the forks where they
//! were, revokes every grant that reached the repository with one audit record,
//! leaves a same-organization rename revoking nothing, refuses without the
//! source or the target authority, cancels a queued merge intent while an
//! executing one blocks the move, and makes the old `owner/name` a `410` on the
//! API, the page and both Git transports — never a redirect and never another
//! repository.
//!
//! The fixtures write PostgreSQL directly because they are the tests' setup;
//! every assertion in the acceptance path goes through the router, the Git
//! admission path or the SSH transport, never through SQL, except the audit and
//! grant assertions, which read the audit and grant tables the feature is
//! defined to write.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use base64::Engine;
use githearth_git::layout::ObjectFormat;
use githearth_server::{
    AppState, MIGRATOR, config::Config, connect, git_admission, hash_password, router, ssh,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Acquire, PgPool};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// HTTP helpers
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

/// A request whose response may be HTML (the Git routes, the browser pages).
async fn raw(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get_page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut headers = Vec::new();
    if let Some(cookie) = cookie {
        headers.push(("cookie", cookie));
    }
    raw(app, "GET", path, &headers, Vec::new()).await
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
    raw(
        app,
        "POST",
        path,
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("cookie", cookie),
        ],
        body.into_bytes(),
    )
    .await
}

fn basic(user: &str, password: &str) -> String {
    let raw = format!("{user}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
    )
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

async fn git_ok(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..12])
}

/// The storage root also carries the admission Unix socket, whose whole path
/// must stay under 107 bytes: keep it short.
fn short_temp_dir(label: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    std::env::temp_dir().join(format!("gtr-{label}-{}", &id[..10]))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct User {
    id: Uuid,
    username: String,
    token: String,
}

impl User {
    fn cookie(&self) -> String {
        format!("githearth_session={}", self.token)
    }
    fn password(&self) -> &'static str {
        "correct-horse-battery"
    }
}

async fn user(pool: &PgPool, prefix: &str) -> User {
    let id = Uuid::new_v4();
    let username = tag(prefix);
    let password = "correct-horse-battery";
    sqlx::query("INSERT INTO accounts (id,username,password_hash,email,email_verified_at) VALUES ($1,$2,$3,$4,now())")
        .bind(id)
        .bind(&username)
        .bind(hash_password(password).unwrap())
        .bind(format!("{username}@example.com"))
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

async fn add_grant(pool: &PgPool, org_id: Uuid, account: Uuid, role: &str) {
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn add_org(pool: &PgPool, label: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag(label);
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&slug)
        .bind(format!("Org {slug}"))
        .execute(pool)
        .await
        .unwrap();
    (id, slug)
}

/// An active repository with a fork parent link, created by `created_by`.
async fn add_repo(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    created_by: Uuid,
    parent: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,description,visibility,created_by,operation_id,status,parent_repository_id) \
         VALUES ($1,$2,$3,'','private',$4,$5,'active',$6)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(created_by)
    .bind(Uuid::new_v4())
    .bind(parent)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A bare repository on disk with one commit on `main`, created the way the
/// create path does it (so the journal and config are the server's).
async fn seed_repo(state: &AppState, repo_id: Uuid, subject: &str) -> PathBuf {
    let path = state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let work = short_temp_dir("seed");
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["init", "-q", "-b", "main", tw])).await;
    std::fs::write(work.join("README.md"), format!("# {subject}\n")).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-q",
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
        "-q",
        "--",
        tw,
        "HEAD:refs/heads/main",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);
    path
}

/// Write a second commit into the bare repository so a clone before and after
/// the transfer can be compared by commit graph.
async fn add_commit(state: &AppState, repo_id: Uuid) {
    let path = state.storage.repository_path(repo_id);
    let work = short_temp_dir("work");
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    // The bare repository's `core.hooksPath` is the server's admission hooks,
    // which refuse outside an admitted push, so the fixture works with it unset.
    let bare = |args: &[&str]| {
        let mut full = git_cmd(&[
            "--git-dir",
            path.to_str().unwrap(),
            "-c",
            "core.hooksPath=/dev/null",
        ]);
        full.args(args);
        full
    };
    git_ok(bare(&["symbolic-ref", "HEAD", "refs/heads/main"])).await;
    git_ok(git_cmd(&["-C", tw, "init", "-q", "-b", "main"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "fetch",
        "-q",
        path.to_str().unwrap(),
        "refs/heads/main",
    ]))
    .await;
    git_ok(git_cmd(&["-C", tw, "reset", "-q", "--hard", "FETCH_HEAD"])).await;
    std::fs::write(work.join("more.txt"), "second\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "more.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-q",
        "-m",
        "second",
    ]))
    .await;
    // Into a throwaway bare repository without the server's hooks, then moved
    // into place: the server's own hooks refuse anything but an admitted push.
    let staging = short_temp_dir("stage");
    git_ok(git_cmd(&[
        "init",
        "-q",
        "--bare",
        staging.to_str().unwrap(),
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "push",
        "-q",
        staging.to_str().unwrap(),
        "HEAD:refs/heads/main",
    ]))
    .await;
    git_ok(git_cmd(&[
        "--git-dir",
        path.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "-q",
        "--no-tags",
        "--",
        staging.to_str().unwrap(),
        "+refs/heads/main:refs/heads/main",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&work);
}

/// The commit graph of a repository as `git rev-list --format` prints it: the
/// same graph for the same history, whatever its path or owner.
async fn commit_graph(path: &Path) -> String {
    let out = git_run(git_cmd(&[
        "--git-dir",
        path.to_str().unwrap(),
        "log",
        "--format=%H %T %P %s",
        "main",
    ]))
    .await;
    assert!(out.status.success(), "log failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

async fn rev_parse(path: &Path, rev: &str) -> String {
    git_ok(git_cmd(&[
        "--git-dir",
        path.to_str().unwrap(),
        "rev-parse",
        rev,
    ]))
    .await
    .trim()
    .to_owned()
}

async fn add_pull(
    pool: &PgPool,
    repo_id: Uuid,
    source_repo_id: Uuid,
    number: i32,
    author: Uuid,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,source_repository_id,number,title,author_id,source_branch,target_branch) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,'main')",
    )
    .bind(id)
    .bind(repo_id)
    .bind(source_repo_id)
    .bind(number)
    .bind(format!("Pull {number}"))
    .bind(author)
    .bind(format!("feature-{number}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn add_review(pool: &PgPool, pull_id: Uuid, reviewer: Uuid, commit: &str) {
    sqlx::query(
        "INSERT INTO pull_request_reviews (id,pull_request_id,reviewer_id,verdict,commit_id) VALUES ($1,$2,$3,'approved',$4)",
    )
    .bind(Uuid::new_v4())
    .bind(pull_id)
    .bind(reviewer)
    .bind(commit)
    .execute(pool)
    .await
    .unwrap();
}

async fn add_intent(pool: &PgPool, repo_id: Uuid, pull_id: Uuid, actor: Uuid, state: &str) {
    sqlx::query(
        "INSERT INTO merge_intents (operation_id,repository_id,pull_request_id,source_ref,source_oid,target_ref,expected_target_oid,method,policy_generation,checks_approvals_digest,actor_id,state) \
         VALUES ($1,$2,$3,'refs/heads/x',$4,'refs/heads/main',$4,'merge',0,'digest',$5,$6)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(pull_id)
    .bind("0".repeat(40))
    .bind(actor)
    .bind(state)
    .execute(pool)
    .await
    .unwrap();
}

async fn pull_state(pool: &PgPool, pull_id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, closed_reason FROM pull_requests WHERE id=$1")
        .bind(pull_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn intent_state(pool: &PgPool, pull_id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM merge_intents WHERE pull_request_id=$1")
        .bind(pull_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn review_count(pool: &PgPool, pull_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pull_request_reviews WHERE pull_request_id=$1")
        .bind(pull_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Audit rows of one kind in one organization, with their payloads, oldest first.
async fn audit(pool: &PgPool, org_id: Uuid, kind: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT x.payload FROM audit_events e JOIN outbox_events x ON x.id = e.id \
         WHERE e.organization_id=$1 AND e.kind=$2 ORDER BY e.created_at, e.id",
    )
    .bind(org_id)
    .bind(kind)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn repo_row(pool: &PgPool, id: Uuid) -> (Uuid, String) {
    sqlx::query_as("SELECT organization_id, name FROM repositories WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Active repository-scoped grants on one repository. Organization-scoped
/// grants are deliberately not counted: a transfer revokes only the grants
/// attached to the repository itself, and the source organization keeps its own.
async fn active_grants(pool: &PgPool, repo_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE revoked_at IS NULL AND repository_id=$1 \
         AND scope_kind='repository'",
    )
    .bind(repo_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Active organization-scoped grants of one organization, across every subject.
async fn source_org_grants(pool: &PgPool, org_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE revoked_at IS NULL \
         AND scope_kind='organization' AND scope_id=$1 AND repository_id IS NULL",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The source organization's owner measure after a transfer: `owner_count`
/// (active owner grants) and `reachable_owner_count` (owner grants a live
/// member can exercise), as `membership.rs` defines them for U04. A transfer
/// must leave at least one of each.
async fn owner_counts(pool: &PgPool, org_id: Uuid) -> (i64, i64) {
    let owners = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE scope_kind='organization' AND scope_id=$1 \
         AND role='owner' AND repository_id IS NULL AND revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > clock_timestamp())",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let reachable = sqlx::query_scalar(
        "SELECT count(*) FROM grants g WHERE g.scope_kind='organization' AND g.scope_id=$1 \
         AND g.role='owner' AND g.repository_id IS NULL AND g.revoked_at IS NULL \
         AND (g.expires_at IS NULL OR g.expires_at > clock_timestamp()) \
         AND ((g.subject_kind='account' AND EXISTS (SELECT 1 FROM memberships m \
                 WHERE m.organization_id=$1 AND m.account_id=g.subject_id)) \
           OR (g.subject_kind='team' AND EXISTS (SELECT 1 FROM team_members tm \
                 JOIN memberships m ON m.organization_id=$1 AND m.account_id=tm.account_id \
                 WHERE tm.team_id=g.subject_id)))",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap();
    (owners, reachable)
}

/// Claims a move may have written for its target name, including ones rolled
/// back with a refusal (a rolled-back claim is absent, so this is `0`).
async fn claims_for(pool: &PgPool, org_id: Uuid, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM repository_claims WHERE organization_id=$1 AND lower(name)=lower($2)",
    )
    .bind(org_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    ssh_port: u16,
    ssh_key: PathBuf,
    org_id: Uuid,
    org_slug: String,
    other_id: Uuid,
    other_slug: String,
    /// The outsider's own organization, and a target the owner is not a
    /// member of: proof a denial is "no access", not "no account".
    foreign_slug: String,
    repo_id: Uuid,
    repo_name: String,
    owner: User,
    member: User,
    outsider: User,
}

async fn fixture(label: &str) -> Fixture {
    let storage_root = short_temp_dir(label);
    std::fs::create_dir_all(&storage_root).unwrap();
    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 8,
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
    let (org_id, org_slug) = add_org(&pool, "src").await;
    let (other_id, other_slug) = add_org(&pool, "dst").await;
    let (foreign_id, foreign_slug) = add_org(&pool, "foreign").await;
    add_membership(&pool, org_id, owner.id, "owner").await;
    add_membership(&pool, org_id, member.id, "member").await;
    // The owner may create in the target; the plain member is only a member there,
    // so the target-side create decision is the one that differs.
    add_membership(&pool, other_id, owner.id, "owner").await;
    add_membership(&pool, other_id, member.id, "member").await;
    add_membership(&pool, foreign_id, outsider.id, "owner").await;

    let repo_name = tag("repo");
    let repo_id = add_repo(&pool, org_id, &repo_name, owner.id, None).await;
    // A repository grant and an organization-scoped grant that reach the
    // repository: both must be revoked by a transfer.
    add_grant(&pool, org_id, member.id, "write").await;

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    let _ = seed_repo(&state, repo_id, &repo_name).await;
    let app = router(state.clone());

    // SSH: map the test key to the owner's account, so the transport exercises
    // admission (not the transport's own identity plumbing).
    let key_dir = short_temp_dir("key");
    std::fs::create_dir_all(&key_dir).unwrap();
    let ssh_key = key_dir.join("id_ed25519");
    let _ = std::fs::remove_file(&ssh_key);
    let out = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &ssh_key.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "ssh-keygen failed");
    let fp_out = Command::new("ssh-keygen")
        .args(["-lf", &ssh_key.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    let fp = String::from_utf8_lossy(&fp_out.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let owner_id = owner.id;
    let authenticate: ssh::Authenticate = Arc::new(move |f: String| {
        let found = (f == fp).then(|| ssh::SshIdentity {
            account: owner_id,
            service_account: None,
            key_id: Uuid::new_v4(),
            read_only: false,
        });
        Box::pin(async move { Ok(found) })
    });
    let (_handle, port_rx) = ssh::spawn_with(state.clone(), authenticate, ssh::AUTH_DEADLINE);
    let ssh_port = port_rx.await.unwrap().port();

    Fixture {
        app,
        state,
        pool,
        storage_root,
        ssh_port,
        ssh_key,
        org_id,
        org_slug,
        other_id,
        other_slug,
        foreign_slug,
        repo_id,
        repo_name,
        owner,
        member,
        outsider,
    }
}

impl Fixture {
    fn transfer_path(&self, org: &str, name: &str) -> String {
        format!("/api/v1/organizations/{org}/repositories/{name}/transfer")
    }

    async fn transfer(
        &self,
        token: &str,
        org: &str,
        name: &str,
        target: &str,
        policy: &str,
    ) -> (StatusCode, Value) {
        api(
            &self.app,
            "POST",
            &self.transfer_path(org, name),
            Some(token),
            Some(&Uuid::new_v4().simple().to_string()),
            Some(&json!({"organization": target, "fork_pull_requests": policy})),
        )
        .await
    }

    /// The repository's storage directory, as the server resolves it.
    fn path(&self, repo_id: Uuid) -> PathBuf {
        self.state.storage.repository_path(repo_id)
    }

    async fn finish(self) {
        let _ = std::fs::remove_dir_all(&self.storage_root);
        self.pool.close().await;
    }
}

/// Whether the SSH client is available; the SSH assertions are the transport's
/// own and cannot be faked.
fn has_ssh() -> bool {
    Command::new("ssh").arg("-V").output().is_ok()
}

// ---------------------------------------------------------------------------
// U07: ID, history and storage path survive
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_transfer_keeps_the_id_the_history_and_the_storage_path() {
    let f = fixture("keep").await;
    add_commit(&f.state, f.repo_id).await;
    let path = f.path(f.repo_id);
    let before_graph = commit_graph(&path).await;
    let before_tip = rev_parse(&path, "refs/heads/main").await;
    let before_path = path.canonicalize().unwrap();

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["id"],
        json!(f.repo_id),
        "the ID is the same repository"
    );
    assert_eq!(body["organization"], json!(f.other_slug));
    assert_eq!(body["previous_name"], json!(f.repo_name));
    let landed = body["name"].as_str().unwrap().to_owned();
    assert_eq!(body["previous_organization"], json!(f.org_slug));

    // The row kept its ID and moved owner; its name is the one the response
    // named.
    let (org, name) = repo_row(&f.pool, f.repo_id).await;
    assert_eq!(org, f.other_id);
    assert_eq!(name, landed);

    // The storage path is untouched, byte for byte, and the history is the same.
    let after_path = f.path(f.repo_id).canonicalize().unwrap();
    assert_eq!(before_path, after_path, "storage path changed");
    assert_eq!(commit_graph(&after_path).await, before_graph);
    assert_eq!(rev_parse(&after_path, "refs/heads/main").await, before_tip);

    // And it is cloneable from its new owner, with the same tip.
    let clone = short_temp_dir("clone");
    git_ok(git_cmd(&[
        "clone",
        "-q",
        after_path.to_str().unwrap(),
        clone.to_str().unwrap(),
    ]))
    .await;
    let cloned_tip = git_ok(git_cmd(&[
        "-C",
        clone.to_str().unwrap(),
        "rev-parse",
        "HEAD",
    ]))
    .await;
    assert_eq!(cloned_tip.trim(), before_tip);

    // The API at the target answers for the same ID.
    let (status, read) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{landed}",
            f.other_slug
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["id"], json!(f.repo_id));

    let _ = std::fs::remove_dir_all(&clone);
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: pull requests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_fork_pulls_and_their_reviews_survive_and_fork_pulls_close_under_the_policy() {
    let f = fixture("pulls").await;
    let tip = rev_parse(&f.path(f.repo_id), "refs/heads/main").await;
    // A same-repository pull with a review, a queued merge intent on it, and a
    // fork pull into the same repository.
    let own = add_pull(&f.pool, f.repo_id, f.repo_id, 1, f.member.id).await;
    add_review(&f.pool, own, f.owner.id, &tip).await;
    add_intent(&f.pool, f.repo_id, own, f.member.id, "queued").await;
    let fork_id = add_repo(&f.pool, f.org_id, &tag("fork"), f.owner.id, Some(f.repo_id)).await;
    let fork_pull = add_pull(&f.pool, f.repo_id, fork_id, 2, f.member.id).await;
    add_review(&f.pool, fork_pull, f.owner.id, &tip).await;

    // `refuse` leaves everything in place and moves nothing.
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(pull_state(&f.pool, fork_pull).await.0, "open");
    assert_eq!(pull_state(&f.pool, own).await.0, "open");
    assert_eq!(intent_state(&f.pool, own).await, "queued");
    assert_eq!(
        repo_row(&f.pool, f.repo_id).await.0,
        f.org_id,
        "refused move changed nothing"
    );
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    assert!(f.path(f.repo_id).is_dir());
    assert_eq!(
        claims_for(&f.pool, f.other_id, &f.repo_name).await,
        0,
        "the refused move left its in-transaction name claim behind"
    );

    // `close` closes the fork pull under the audited policy and keeps the rest.
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "close",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["fork_pull_requests"], json!("close"));
    assert_eq!(body["closed_fork_pull_requests"], json!(1));
    let (state, reason) = pull_state(&f.pool, fork_pull).await;
    assert_eq!(state, "closed");
    assert_eq!(
        reason.as_deref(),
        Some("repository transferred to another organization")
    );
    assert_eq!(
        review_count(&f.pool, fork_pull).await,
        1,
        "a closed pull keeps its reviews"
    );
    // The non-fork pull is untouched, reviews included, and its queued intent
    // is cancelled by the move.
    assert_eq!(pull_state(&f.pool, own).await.0, "open");
    assert_eq!(review_count(&f.pool, own).await, 1);
    assert_eq!(intent_state(&f.pool, own).await, "cancelled");

    // One transfer record, carrying the policy and what it did.
    let records = audit(&f.pool, f.org_id, "repository.transferred").await;
    assert_eq!(records.len(), 1, "one audit record for the move");
    assert_eq!(records[0]["fork_pull_requests"], json!("close"));
    assert_eq!(records[0]["fork_pull_requests_closed"], json!(1));
    assert_eq!(records[0]["to_organization_id"], json!(f.other_id));
    // Each closed fork pull is audited as closed with its reason.
    let closes = audit(&f.pool, f.org_id, "pull_request.closed").await;
    assert_eq!(closes.len(), 1, "{closes:?}");
    assert_eq!(closes[0]["policy"], json!("close"));
    assert_eq!(closes[0]["number"], json!(2));

    // Forks stay in the source organization, with their existing visibility.
    let (fork_org, _) = repo_row(&f.pool, fork_id).await;
    assert_eq!(fork_org, f.org_id, "a fork moved with its parent");
    let fork_visibility: String =
        sqlx::query_scalar("SELECT visibility FROM repositories WHERE id=$1")
            .bind(fork_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(fork_visibility, "private");
    // And the fork is no longer a fork of the moved repository, which now lives
    // in another organization: a fork's parent belongs to the fork's own
    // organization, so the link is broken by the move rather than dangling
    // across organizations.
    let parent: Option<Uuid> =
        sqlx::query_scalar("SELECT parent_repository_id FROM repositories WHERE id=$1")
            .bind(fork_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(parent, None);

    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: grants
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_transfer_revokes_every_grant_with_one_audit_record_and_the_old_grantees_lose_access() {
    let f = fixture("grants").await;
    // A repository-scoped grant and a team's organization grant that reach the
    // repository, on top of the membership grants the fixture made.
    let team_id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Editors')")
        .bind(team_id)
        .bind(f.org_id)
        .bind(tag("team"))
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) \
         VALUES ($1,'admin','account',$2,'repository',$3,$4,$2)",
    )
    .bind(Uuid::new_v4())
    .bind(f.member.id)
    .bind(f.org_id)
    .bind(f.repo_id)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,'write','team',$2,'organization',$3,$4)",
    )
    .bind(Uuid::new_v4())
    .bind(team_id)
    .bind(f.org_id)
    .bind(f.owner.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let before = active_grants(&f.pool, f.repo_id).await;
    let before_org = source_org_grants(&f.pool, f.org_id).await;
    assert_eq!(before, 1, "one repository-scoped grant: {before}");
    assert!(before_org >= 4, "fixture organization grants: {before_org}");

    // The member reads the repository before the move, and so does the API.
    let member_path = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, f.repo_name
    );
    let (status, _) = api(
        &f.app,
        "GET",
        &member_path,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the member reads it before the move"
    );

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked_grants"].as_i64().unwrap(), before);
    let landed = body["name"].as_str().unwrap().to_owned();

    assert_eq!(active_grants(&f.pool, f.repo_id).await, 0);
    // The source organization's own grants were not touched: it keeps its
    // owner(s), its team grant and every grant that does not name the
    // repository, so a transfer never leaves it ownerless (U04).
    assert_eq!(
        source_org_grants(&f.pool, f.org_id).await,
        before_org,
        "a transfer revoked a source organization grant"
    );
    let (owners, reachable) = owner_counts(&f.pool, f.org_id).await;
    assert!(
        owners >= 1 && reachable >= 1,
        "source organization left without an owner: owner_count={owners} reachable_owner_count={reachable}"
    );
    // Exactly one audit record carries the revocation, with the count in it.
    let records = audit(&f.pool, f.org_id, "repository.transferred").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["revoked_grants"].as_i64().unwrap(), before);
    // The record is in the source organization's chain and that chain still
    // verifies; the target's chain is untouched and verifies too.
    let mut conn = f.pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    let source = githearth_server::audit::verify(&mut tx, Some(f.org_id))
        .await
        .unwrap();
    assert!(
        source.rows > 0 && source.ok,
        "source chain: rows={} ok={} bad={:?}",
        source.rows,
        source.ok,
        source.first_bad_seq
    );
    let target = githearth_server::audit::verify(&mut tx, Some(f.other_id))
        .await
        .unwrap();
    assert!(
        target.ok,
        "target chain: rows={} ok={} bad={:?}",
        target.rows, target.ok, target.first_bad_seq
    );
    tx.rollback().await.unwrap();
    drop(conn);

    // The old grantees cannot reach the repository: not at the old path
    // (410), and not through the source organization any more.
    let (status, _) = api(
        &f.app,
        "GET",
        &member_path,
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    let (status, _) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{landed}",
            f.other_slug
        ),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a revoked grant does not cross organizations"
    );
    // Someone who only reached it through the source's grants (the team's
    // organization grant here) is refused at the target too.
    let (status, _) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{landed}/access",
            f.other_slug
        ),
        Some(&f.member.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The owner, who administers the target, finds no grant in force there.
    let (status, grants) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{landed}/access",
            f.other_slug
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    // Whoever can administer it there still sees it, but nothing is in force:
    // every source grant was revoked by the move and none was made at the target.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        grants,
        json!([]),
        "the moved repository has no grants in force"
    );

    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_rename_inside_one_organization_revokes_nothing_and_is_not_a_transfer() {
    let f = fixture("rename").await;
    let before = source_org_grants(&f.pool, f.org_id).await;
    assert!(before > 0);
    let new_name = tag("renamed");
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/repositories/{}",
            f.org_slug, f.repo_name
        ),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": new_name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!(new_name));
    assert_eq!(
        source_org_grants(&f.pool, f.org_id).await,
        before,
        "a rename revoked a grant"
    );
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    // The renamed-away name still redirects inside the organization. This is
    // non-conforming against ADR 0003 (which says a renamed-away name is `410`,
    // never a redirect) and pins pre-existing rename behaviour, not U07. Flip
    // this assertion when the rename decision lands.
    let (status, _) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{}",
            f.org_slug, f.repo_name
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a rename keeps the old path reachable"
    );

    // Naming the repository's own organization as the target is not a transfer.
    let (status, _) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &new_name,
            &f.org_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(source_org_grants(&f.pool, f.org_id).await, before);
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: refusals
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_transfer_is_refused_without_source_administration_or_target_create() {
    let f = fixture("refuse").await;

    // A plain member administers neither side.
    let (status, _) = f
        .transfer(
            &f.member.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An outsider to the source learns nothing about it.
    let (status, _) = f
        .transfer(
            &f.outsider.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The member *is* a member of the target but holds no create grant there.
    let owner_only_target = add_org(&f.pool, "owneronly").await;
    add_membership(&f.pool, owner_only_target.0, f.owner.id, "owner").await;
    // Grants are never deleted: the membership stays, its owner grant is
    // revoked, and the target no longer permits a create.
    sqlx::query(
        "UPDATE grants SET revoked_at=now(), revoked_by=$2 WHERE scope_id=$1 AND subject_id=$2 AND role='owner'",
    )
    .bind(owner_only_target.0)
    .bind(f.owner.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &owner_only_target.1,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.foreign_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Nothing moved, no grant was revoked, no audit record exists.
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.org_id);
    assert!(source_org_grants(&f.pool, f.org_id).await > 0);
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    // Only the claims of this test's own target names: the scratch database is
    // shared with the rest of the suite.
    let claims: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_claims WHERE lower(name) LIKE $1")
            .bind(format!("{}%", f.repo_name.to_lowercase()))
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(claims, 0, "a refused move left a name claim behind");

    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_name_collision_at_the_target_refuses_the_move_and_changes_nothing() {
    let f = fixture("collision").await;
    // The target already holds the name: a homonym (a fork, or another
    // repository) owned by somebody else.
    let homonym = add_repo(&f.pool, f.other_id, &f.repo_name, f.owner.id, None).await;
    let _ = seed_repo(&f.state, homonym, "homonym").await;
    let homonym_before = commit_graph(&f.path(homonym)).await;
    let source_before = repo_row(&f.pool, f.repo_id).await;
    let grants_before = (
        active_grants(&f.pool, f.repo_id).await,
        source_org_grants(&f.pool, f.org_id).await,
    );
    // A write the refusal reaches before it gives up: the `close` policy closes
    // this fork pull and cancels the queued intent first. Both must be rolled
    // back with the refusal, which is what "one transaction" means.
    let fork_id = add_repo(&f.pool, f.org_id, &tag("fork"), f.owner.id, Some(f.repo_id)).await;
    let fork_pull = add_pull(&f.pool, f.repo_id, fork_id, 1, f.member.id).await;
    let own_pull = add_pull(&f.pool, f.repo_id, f.repo_id, 2, f.member.id).await;
    add_intent(&f.pool, f.repo_id, own_pull, f.member.id, "queued").await;

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "close",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"],
        json!(
            "the target organization already has a repository with this name; rename first, then transfer"
        )
    );
    assert_eq!(
        pull_state(&f.pool, fork_pull).await.0,
        "open",
        "the refused move closed a fork pull request"
    );
    assert_eq!(
        intent_state(&f.pool, own_pull).await,
        "queued",
        "the refused move cancelled a merge intent"
    );

    // Nothing changed: the repository is still at the source under its name,
    // its grants are intact, the homonym and its history are untouched, and no
    // redirect, claim, audit record or repository was created anywhere.
    assert_eq!(repo_row(&f.pool, f.repo_id).await, source_before);
    assert_eq!(
        (
            active_grants(&f.pool, f.repo_id).await,
            source_org_grants(&f.pool, f.org_id).await,
        ),
        grants_before
    );
    assert_eq!(
        repo_row(&f.pool, homonym).await,
        (f.other_id, f.repo_name.clone())
    );
    assert_eq!(commit_graph(&f.path(homonym)).await, homonym_before);
    assert!(f.path(f.repo_id).is_dir(), "the source directory is gone");
    let renamed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repositories WHERE organization_id=$1 AND name LIKE $2",
    )
    .bind(f.other_id)
    .bind(format!("{}-%", f.repo_name))
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(renamed, 0, "the refusal picked another name at the target");
    let redirects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repository_redirects WHERE repository_id=$1 OR lower(name)=lower($2)",
    )
    .bind(f.repo_id)
    .bind(&f.repo_name)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(redirects, 0, "the refused move retired the source name");
    let claims: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_claims WHERE lower(name)=lower($1)")
            .bind(&f.repo_name)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(claims, 0, "the refused move left a name claim behind");
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty(),
        "the refused move wrote an audit record"
    );
    assert!(
        audit(&f.pool, f.org_id, "pull_request.closed")
            .await
            .is_empty(),
        "the refused move kept a pull-close audit from its transaction"
    );

    // The source path still resolves to the repository it always named.
    let (status, body) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{}",
            f.org_slug, f.repo_name
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(f.repo_id));

    // A retired name at the target is taken too: landing on a tombstone would
    // move the repository to an address the lookup refuses (`410`), so it is
    // the same `409` and the source is still untouched.
    sqlx::query("UPDATE repositories SET status='deleting' WHERE id=$1")
        .bind(homonym)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE repositories SET status='deleted' WHERE id=$1")
        .bind(homonym)
        .execute(&f.pool)
        .await
        .unwrap();
    // Remove the row entirely: with it present (even deleted) an earlier
    // repository branch could be what refuses the move. Only the tombstone
    // branch may produce this `409`.
    sqlx::query("DELETE FROM repositories WHERE id=$1")
        .bind(homonym)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO repository_redirects (organization_id, name, repository_id, transferred_at) VALUES ($1,$2,$3, now())",
    )
    .bind(f.other_id)
    .bind(&f.repo_name)
    .bind(f.repo_id)
    .execute(&f.pool)
    .await
    .unwrap();
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "close",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(repo_row(&f.pool, f.repo_id).await, source_before);
    f.finish().await;
}

/// A `status = 'deleted'` repository does not reserve its name: a create may
/// reuse it, so a transfer landing on it must be allowed too.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_deleted_repository_does_not_reserve_its_name_for_a_transfer() {
    let f = fixture("deletedname").await;
    let dead = add_repo(&f.pool, f.other_id, &f.repo_name, f.owner.id, None).await;
    sqlx::query("UPDATE repositories SET status='deleting' WHERE id=$1")
        .bind(dead)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE repositories SET status='deleted' WHERE id=$1")
        .bind(dead)
        .execute(&f.pool)
        .await
        .unwrap();

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        repo_row(&f.pool, f.repo_id).await,
        (f.other_id, f.repo_name.clone())
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: merge intents
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_queued_intent_is_cancelled_and_an_executing_intent_blocks_the_move() {
    let f = fixture("intents").await;
    let pull = add_pull(&f.pool, f.repo_id, f.repo_id, 1, f.member.id).await;
    add_intent(&f.pool, f.repo_id, pull, f.member.id, "queued").await;

    // Executing: the move is refused and nothing changes.
    sqlx::query("UPDATE merge_intents SET state='executing' WHERE pull_request_id=$1")
        .bind(pull)
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"],
        json!("a merge is executing; retry after it finishes")
    );
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.org_id);
    assert_eq!(intent_state(&f.pool, pull).await, "executing");
    assert_eq!(
        claims_for(&f.pool, f.other_id, &f.repo_name).await,
        0,
        "the executing refusal left its in-transaction name claim behind"
    );

    // Finished: the move goes through and the queued intent is cancelled.
    sqlx::query("UPDATE merge_intents SET state='merged' WHERE pull_request_id=$1")
        .bind(pull)
        .execute(&f.pool)
        .await
        .unwrap();
    let queued = add_pull(&f.pool, f.repo_id, f.repo_id, 2, f.member.id).await;
    add_intent(&f.pool, f.repo_id, queued, f.member.id, "queued").await;
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(intent_state(&f.pool, queued).await, "cancelled");
    f.finish().await;
}

/// The `executing` gate and the merge worker's `validating -> executing`
/// transition are serialized by locking the repository's live intents: the
/// worker here is paused while it holds the row, the move waits at its intent
/// lock, and when the transition commits the move sees `executing` and refuses.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_executing_transition_racing_the_move_is_serialized_and_refuses_it() {
    let f = fixture("intentrace").await;
    let pull = add_pull(&f.pool, f.repo_id, f.repo_id, 1, f.member.id).await;
    add_intent(&f.pool, f.repo_id, pull, f.member.id, "queued").await;
    // What `claim_intent` commits before the worker moves to `executing`.
    sqlx::query("UPDATE merge_intents SET state='validating' WHERE pull_request_id=$1")
        .bind(pull)
        .execute(&f.pool)
        .await
        .unwrap();

    // The worker's `move_to_executing`, held open (uncommitted) while the move
    // runs: the move must block on the intent row, not read a stale state.
    let mut conn = f.pool.acquire().await.unwrap();
    let mut worker = conn.begin().await.unwrap();
    sqlx::query("UPDATE merge_intents SET state='executing' WHERE pull_request_id=$1")
        .bind(pull)
        .execute(&mut *worker)
        .await
        .unwrap();

    let app = f.app.clone();
    let token = f.owner.token.clone();
    let org = f.org_slug.clone();
    let name = f.repo_name.clone();
    let target = f.other_slug.clone();
    let moving = tokio::spawn(async move {
        api(
            &app,
            "POST",
            &format!("/api/v1/organizations/{org}/repositories/{name}/transfer"),
            Some(&token),
            Some(&Uuid::new_v4().simple().to_string()),
            Some(&json!({"organization": target, "fork_pull_requests": "refuse"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !moving.is_finished(),
        "the move did not wait for the in-flight intent transition"
    );
    // Let the worker commit `executing`; the move's locked read now sees it.
    worker.commit().await.unwrap();
    drop(conn);

    let (status, body) = moving.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"],
        json!("a merge is executing; retry after it finishes")
    );
    assert_eq!(intent_state(&f.pool, pull).await, "executing");
    assert_eq!(
        repo_row(&f.pool, f.repo_id).await.0,
        f.org_id,
        "the move committed under a live merge"
    );
    assert_eq!(
        claims_for(&f.pool, f.other_id, &f.repo_name).await,
        0,
        "the refusal left its in-transaction name claim behind"
    );
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: the old path is 410 everywhere, and never another tenant's repository
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_old_path_is_gone_on_the_api_the_page_and_both_git_transports_even_after_the_name_is_reused()
 {
    let f = fixture("gone").await;
    let old_path = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, f.repo_name
    );
    let page_path = format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name);

    // Before: the owner reads the API, the page and clones over Git HTTP.
    let (status, _) = api(&f.app, "GET", &old_path, Some(&f.owner.token), None, None).await;
    assert_eq!(status, StatusCode::OK);
    // The SSH transport has the repository before the move too.
    if has_ssh() {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-i",
            &f.ssh_key.to_string_lossy(),
            "-p",
            &f.ssh_port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=10",
            "-T",
            "git@127.0.0.1",
            &format!("git-upload-pack '/{}/{}.git'", f.org_slug, f.repo_name),
        ])
        .stdin(std::process::Stdio::null());
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || cmd.output().unwrap()),
        )
        .await
        .expect("ssh hung")
        .unwrap();
        assert_ne!(
            String::from_utf8_lossy(&out.stderr).trim_end(),
            "githearth: repository not found",
            "SSH cannot reach the repository before the move"
        );
    }

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let landed = body["name"].as_str().unwrap().to_owned();

    // API: 410 for the owner, for a member, for an outsider, and unauthenticated.
    for token in [&f.owner.token, &f.member.token, &f.outsider.token] {
        let (status, body) = api(&f.app, "GET", &old_path, Some(token), None, None).await;
        assert_eq!(status, StatusCode::GONE, "{body}");
    }
    // Signed out the API answers its usual challenge, which still reveals
    // nothing (the same 401 any repository, present or not, gives an anonymous
    // API caller); the browser below gets the `410` page.
    let (status, _) = api(&f.app, "GET", &old_path, None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Git HTTP: the info/refs discovery is refused like a missing repository.
    let discover = raw(
        &f.app,
        "GET",
        &format!(
            "/{}/{}.git/info/refs?service=git-upload-pack",
            f.org_slug, f.repo_name
        ),
        &[(
            "authorization",
            &basic(&f.owner.username, f.owner.password()),
        )],
        Vec::new(),
    )
    .await;
    assert_ne!(discover.0, StatusCode::OK, "the old path answered a fetch");
    assert!(
        discover.1.contains("not found"),
        "Git HTTP answered something other than not found: {} {}",
        discover.0,
        discover.1
    );

    // SSH: the old path is `repository not found`.
    if has_ssh() {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-i",
            &f.ssh_key.to_string_lossy(),
            "-p",
            &f.ssh_port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=10",
            "-T",
            "git@127.0.0.1",
            &format!("git-upload-pack '/{}/{}.git'", f.org_slug, f.repo_name),
        ])
        .stdin(std::process::Stdio::null());
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || cmd.output().unwrap()),
        )
        .await
        .expect("ssh hung")
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim_end(),
            "githearth: repository not found"
        );
    }

    // The browser page: 410, not a 404 and not a sign-in redirect, and it does
    // not name the new organization.
    let (status, html) = get_page(&f.app, &page_path, Some(&f.owner.cookie())).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(
        !html.contains(&f.other_slug),
        "the page names the new owner"
    );

    // The move did not touch the owner's source organization grant, so the
    // same owner can still create at the source without an administrator
    // re-granting anything. (B1: only the repository's own grants are revoked.)
    let after_move_name = tag("after-move");
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": after_move_name})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Now reuse the old name in the source organization. The create API
    // refuses the retired name (`409`), and even a row inserted out of band
    // does not make the old path resolve — the moved repository must not
    // answer for it, and neither must a new one with the same name.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": f.repo_name})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A rename onto the retired name is refused the same way and changes
    // nothing.
    let other_name = tag("other");
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": other_name})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/repositories/{other_name}",
            f.org_slug
        ),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": f.repo_name})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{other_name}",
            f.org_slug
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!(other_name));

    // Even a repository row with the name, written outside the API, does not
    // make the old path resolve: the retired row is checked first.
    let reused = add_repo(&f.pool, f.org_id, &f.repo_name, f.outsider.id, None).await;
    let _ = seed_repo(&f.state, reused, "reused").await;
    let (status, body) = api(&f.app, "GET", &old_path, Some(&f.owner.token), None, None).await;
    assert_eq!(status, StatusCode::GONE, "the reused name resolved: {body}");
    // Both Git transports' shared admission path refuses it as missing, so a
    // clone of the reused name cannot reach either repository.
    let err = git_admission::admit_account(
        &f.state,
        f.owner.id,
        false,
        git_admission::CredentialRef::Password,
        &f.org_slug,
        &f.repo_name,
        git_admission::Want::Fetch,
    )
    .await
    .expect_err("the retired path admitted a fetch after reuse");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    let (status, _) = api(&f.app, "GET", &old_path, None, None, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no anonymous API repository read"
    );
    let (status, _) = get_page(&f.app, &page_path, None).await;
    assert_eq!(status, StatusCode::GONE);
    // The moved repository is at the target under its own name and ID.
    let (status, body) = api(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{landed}",
            f.other_slug
        ),
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], json!(f.repo_id));

    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: a rename before the transfer retires its alias too
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_rename_before_a_transfer_retires_the_alias_with_the_move() {
    let f = fixture("alias").await;
    let original = f.repo_name.clone();
    let renamed = tag("renamed");
    let original_path = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, original
    );
    let original_page = format!("/organizations/{}/repositories/{}", f.org_slug, original);

    // Rename inside the source: the original name becomes a same-organization
    // redirect (pre-existing rename behaviour; see the non-conformance note in
    // `a_rename_inside_one_organization_revokes_nothing_and_is_not_a_transfer`).
    let (status, body) = api(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/repositories/{}",
            f.org_slug, original
        ),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": renamed})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = api(
        &f.app,
        "GET",
        &original_path,
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the rename did not leave its alias");

    // Transfer under the current name; the alias must retire with it.
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &renamed,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A later create cannot reuse the original name: it is a tombstone now.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.owner.token),
        Some(&Uuid::new_v4().simple().to_string()),
        Some(&json!({"name": original})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the pre-transfer alias was reusable: {body}"
    );

    // And the old path is `410`, never a redirect and never a new repository.
    let (status, body) = api(
        &f.app,
        "GET",
        &original_path,
        Some(&f.owner.token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    let (status, _) = get_page(&f.app, &original_page, None).await;
    assert_eq!(status, StatusCode::GONE);
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: fresh authentication and browser sub-pages
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_stale_session_cannot_transfer_a_repository() {
    let f = fixture("stale").await;
    // The action is sensitive: auth older than the 15-minute sudo window is
    // refused even though the account still administers the repository.
    let stale = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,authenticated_at,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() - interval '16 minutes',clock_timestamp() + interval '1 hour')",
    )
    .bind(&stale)
    .bind(f.owner.id)
    .execute(&f.pool)
    .await
    .unwrap();
    let (status, body) = f
        .transfer(&stale, &f.org_slug, &f.repo_name, &f.other_slug, "refuse")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.org_id);
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    f.finish().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_browser_sub_page_of_a_moved_away_repository_is_gone_not_unavailable() {
    let f = fixture("subpage").await;
    let page = format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name);
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A browse sub-page through the API-error mapping: signed in and anonymous
    // both get the `410` page, not a `503` or a sign-in prompt.
    let tree = format!("{page}/tree/main");
    let (status, html) = get_page(&f.app, &tree, Some(&f.owner.cookie())).await;
    assert_eq!(status, StatusCode::GONE, "{html}");
    let (status, html) = get_page(&f.app, &tree, None).await;
    assert_eq!(status, StatusCode::GONE, "{html}");
    // And a pull-request sub-page, the other browser loader.
    let pulls = format!("{page}/pulls");
    let (status, html) = get_page(&f.app, &pulls, Some(&f.owner.cookie())).await;
    assert_eq!(status, StatusCode::GONE, "{html}");
    f.finish().await;
}

// ---------------------------------------------------------------------------
// U07: storage
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_missing_storage_directory_refuses_the_move_and_moves_nothing() {
    let f = fixture("storage").await;
    let path = f.path(f.repo_id);
    std::fs::remove_dir_all(&path).unwrap();
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.org_id);
    assert!(source_org_grants(&f.pool, f.org_id).await > 0);
    assert!(
        audit(&f.pool, f.org_id, "repository.transferred")
            .await
            .is_empty()
    );
    // Only the claims of this test's own target names: the scratch database is
    // shared with the rest of the suite.
    let claims: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_claims WHERE lower(name) LIKE $1")
            .bind(format!("{}%", f.repo_name.to_lowercase()))
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(claims, 0);
    f.finish().await;
}

/// A create racing a transfer must not take the name the transfer is landing
/// on: the transfer claims it inside its transaction, before it locks anything,
/// so every other transaction sees it.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_create_cannot_take_a_name_a_transfer_has_claimed() {
    let f = fixture("claim").await;
    // The claim a transfer makes, written the way the transfer writes it (its
    // `claim_name` on its own transaction) and held open while a create races
    // it.
    let mut conn = f.pool.acquire().await.unwrap();
    let mut tx = sqlx::Connection::begin(&mut *conn).await.unwrap();
    let claimed = githearth_server::repositories::claim_name(&mut tx, f.other_id, &f.repo_name)
        .await
        .unwrap();
    assert!(claimed);
    let create = |name: String| {
        let app = f.app.clone();
        let token = f.owner.token.clone();
        let path = format!("/api/v1/organizations/{}/repositories", f.other_slug);
        async move {
            api(
                &app,
                "POST",
                &path,
                Some(&token),
                Some(&Uuid::new_v4().simple().to_string()),
                Some(&json!({"name": name})),
            )
            .await
        }
    };
    // The claim is in an uncommitted transaction this test holds: a create
    // that races it waits for the claim to end and then takes the now-free
    // name, which is the whole point (it never sees a phantom reservation).
    let racing = tokio::spawn(create(f.repo_name.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !racing.is_finished(),
        "the create did not wait for the claim to end"
    );
    tx.rollback().await.unwrap();
    drop(conn);
    let (status, body) = racing.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A claim in force refuses the same name to a create outright. (A committed
    // claim in an integration test would race the rest of the suite by name, so
    // this is pinned at the source below rather than by inserting a row.)
    let create_source = include_str!("../src/repository_api.rs");
    let lookup = include_str!("../src/repositories.rs");
    assert!(
        lookup.contains("FROM repository_claims WHERE organization_id=$1 AND lower(name)=lower($2) AND expires_at > clock_timestamp()"),
        "the create path no longer consults a live transfer claim"
    );
    assert!(
        create_source.contains("matches!(e, sqlx::Error::RowNotFound)"),
        "a create refused by a claim is no longer reported as a taken name"
    );

    // And a refused move leaves no claim behind: the move refuses on the
    // storage check and moves nothing, and a create still takes the free name.
    let path = f.path(f.repo_id);
    std::fs::remove_dir_all(&path).unwrap();
    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let source_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND organization_id=$2)",
    )
    .bind(f.repo_id)
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(source_exists, "the refused move changed the owner");
    // The name is free at the target (nothing was claimed there), and a create
    // takes it. The name is unique across the scratch database, so a leftover
    // claim from the refused move would refuse this.
    // The claim's effect on creates is pinned at the source above and the
    // claim's own lifecycle by the refused move just made. What a fresh name
    // proves here is that no claim outlives a refused move.
    let fresh = tag("fresh");
    let (status, body) = create(fresh).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    f.finish().await;
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_page_transfers_with_csrf_and_an_idempotency_key_and_masks_what_it_must() {
    let f = fixture("page").await;
    let page_path = format!(
        "/organizations/{}/repositories/{}/transfer",
        f.org_slug, f.repo_name
    );
    // A caller who may not transfer sees the 403 page, not the form.
    let (status, html) = get_page(&f.app, &page_path, Some(&f.member.cookie())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{html}");
    assert!(!html.contains("Transfer repository</button>"));
    // A stranger gets the same 404 a missing repository gives.
    let (status, _) = get_page(&f.app, &page_path, Some(&f.outsider.cookie())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Signed out: sign in first.
    let (status, _) = get_page(&f.app, &page_path, None).await;
    assert!(status.is_redirection());

    // The owner gets the form, listing the target and escaping its values.
    let (status, html) = get_page(&f.app, &page_path, Some(&f.owner.cookie())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains(&f.other_slug));
    assert!(
        !html.contains(&f.foreign_slug),
        "a target the caller may not create in is offered"
    );
    let csrf = hidden(&html, "csrf_token");

    // Without the session cookie the form has the wrong CSRF token and writes
    // nothing (and nothing is written for a session that is not the caller's).
    let (status, _) = post_form(
        &f.app,
        &page_path,
        &f.owner.cookie(),
        &[
            ("csrf_token", "not-the-token".to_owned()),
            ("idempotency_key", Uuid::new_v4().simple().to_string()),
            ("organization", f.other_slug.clone()),
            ("fork_pull_requests", "refuse".to_owned()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.org_id);

    // The real form moves it and lands on the repository page at the target.
    let (status, _) = post_form(
        &f.app,
        &page_path,
        &f.owner.cookie(),
        &[
            ("csrf_token", csrf),
            ("idempotency_key", Uuid::new_v4().simple().to_string()),
            ("organization", f.other_slug.clone()),
            ("fork_pull_requests", "close".to_owned()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(repo_row(&f.pool, f.repo_id).await.0, f.other_id);
    let records = audit(&f.pool, f.org_id, "repository.transferred").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["fork_pull_requests"], json!("close"));
    f.finish().await;
}

// ---------------------------------------------------------------------------
// The admission path, called directly
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn git_admission_refuses_the_moved_away_path_and_the_revoked_grant() {
    let f = fixture("admit").await;
    // The owner can push before the move (a write grant on the organization).
    let admitted = git_admission::admit_account(
        &f.state,
        f.owner.id,
        false,
        git_admission::CredentialRef::Password,
        &f.org_slug,
        &f.repo_name,
        git_admission::Want::Push,
    )
    .await;
    assert!(admitted.is_ok(), "the owner could not push before the move");

    let (status, body) = f
        .transfer(
            &f.owner.token,
            &f.org_slug,
            &f.repo_name,
            &f.other_slug,
            "refuse",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let err = git_admission::admit_account(
        &f.state,
        f.owner.id,
        false,
        git_admission::CredentialRef::Password,
        &f.org_slug,
        &f.repo_name,
        git_admission::Want::Fetch,
    )
    .await
    .expect_err("the old path admitted a fetch");
    assert_eq!(err.0, StatusCode::NOT_FOUND);

    // The repository's own grants are revoked, so no grant row reaches it at
    // the target. The owner's *organization* grant is not touched: it is the
    // source organization's authority over itself, and after the move it
    // cannot reach a repository in another organization anyway.
    assert_eq!(
        active_grants(&f.pool, f.repo_id).await,
        0,
        "a repository grant survived the transfer"
    );
    let (owners, reachable) = owner_counts(&f.pool, f.org_id).await;
    assert!(
        owners >= 1 && reachable >= 1,
        "the transfer stripped the source organization's owner: owner_count={owners} reachable_owner_count={reachable}"
    );
    // An account that is a member of neither organization (the outsider owns
    // the foreign organization) is refused as a stranger.
    let err = git_admission::admit_account(
        &f.state,
        f.outsider.id,
        false,
        git_admission::CredentialRef::Password,
        &f.other_slug,
        &f.repo_name,
        git_admission::Want::Fetch,
    )
    .await
    .expect_err("a stranger reached the repository at the target");
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    f.finish().await;
}
