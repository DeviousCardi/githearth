//! CODEOWNERS contract tests (PostgreSQL-backed).
//!
//! A repository is created through the API and filled by a plain `git` CLI in
//! a temporary work tree, with a CODEOWNERS file committed to the target
//! branch. Opening a pull request must request reviews from the changed files'
//! code owners; branch protection can require that an owner approves before a
//! merge intent is accepted.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use sqlx::{
    Connection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(args);
    cmd
}

async fn git_run(mut cmd: Command) -> std::process::Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::task::spawn_blocking(move || cmd.output()),
    )
    .await
    .expect("git command timed out")
    .expect("spawn_blocking join error")
    .expect("git command failed to launch")
}

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn git_output(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
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

/// A member with the given builtin role; returns `(account_id, username)`.
async fn add_member(pool: &PgPool, org_id: Uuid, role: &str) -> (Uuid, String) {
    let username = tag("member");
    let account = fixture_account(pool, &username).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(account)
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
    (account, username)
}

/// A team with the given members (already org members); returns the slug.
async fn add_team(pool: &PgPool, org_id: Uuid, members: &[Uuid]) -> String {
    let slug = tag("team");
    let team = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO teams (id,organization_id,slug,name) VALUES ($1,$2,$3,'Fixture Team')",
    )
    .bind(team)
    .bind(org_id)
    .bind(&slug)
    .execute(pool)
    .await
    .unwrap();
    for member in members {
        sqlx::query("INSERT INTO team_members (team_id,account_id) VALUES ($1,$2)")
            .bind(team)
            .bind(member)
            .execute(pool)
            .await
            .unwrap();
    }
    slug
}

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

fn replace_db_name(url: &str, new_db: &str) -> String {
    if let Some(pos) = url.rfind('/') {
        let (base, _) = url.split_at(pos);
        format!("{base}/{new_db}")
    } else {
        format!("{url}/{new_db}")
    }
}

/// A per-test disposable database, so parallel tests do not contend over the
/// shared `jobs` queue (the durable runner claims jobs globally).
struct TestDb {
    db_name: String,
    url: String,
    admin_opts: PgConnectOptions,
}

impl TestDb {
    async fn new() -> Self {
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let db_name = format!("githearth_co_{}", Uuid::new_v4().simple());
        let admin_opts = PgConnectOptions::from_str(&base_url)
            .unwrap()
            .disable_statement_logging();
        let mut admin_conn = admin_opts.clone().connect().await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&mut admin_conn)
            .await
            .unwrap();
        admin_conn.close().await.unwrap();

        let url = replace_db_name(&base_url, &db_name);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(
                PgConnectOptions::from_str(&url)
                    .unwrap()
                    .disable_statement_logging(),
            )
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        pool.close().await;

        Self {
            db_name,
            url,
            admin_opts,
        }
    }

    async fn drop_db(self) {
        let mut admin_conn = self.admin_opts.connect().await.unwrap();
        let _ = sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.db_name))
            .execute(&mut admin_conn)
            .await;
    }
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    work: PathBuf,
    org_slug: String,
    org_id: Uuid,
    owner_token: String,
    repo_name: String,
    repo_id: Uuid,
    number: i64,
    create_body: Value,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

/// Builds a repository whose `main` branch carries a CODEOWNERS file naming
/// `owner_name` for `feat.txt`, with an open pull request `feature -> main`.
async fn build_fixture(database_url: &str, codeowners_line: &str) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-co-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: database_url.to_owned(),
        database_max_connections: 8,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        job_poll_interval_ms: 50,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool, &tag("owner")).await;
    let owner_token = fixture_session(&pool, owner).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&owner_token),
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);

    let work = std::env::temp_dir().join(format!("git-co-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    std::fs::write(work.join("CODEOWNERS"), format!("{codeowners_line}\n")).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "root",
    ]))
    .await;

    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "-b", "feature"])).await;
    std::fs::write(work.join("feat.txt"), "feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "feature",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
        "feature",
    ]))
    .await;

    let (status, pull) = request(
        &app,
        "POST",
        &repo_href(&org_slug, &repo_name),
        Some(&owner_token),
        json!({"title": "pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();

    Fixture {
        app,
        state,
        pool,
        storage_root,
        work,
        org_slug,
        org_id,
        owner_token,
        repo_name,
        repo_id,
        number,
        create_body: pull,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn run_jobs(state: &AppState) {
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    githearth_server::codeowners::register(&mut registry);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
}

async fn pull_id(f: &Fixture) -> Uuid {
    sqlx::query_scalar("SELECT id FROM pull_requests WHERE number = $1 AND repository_id = $2")
        .bind(f.number as i32)
        .bind(f.repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Review requests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn opening_a_pr_requests_review_from_the_account_code_owner() {
    let db = TestDb::new().await;
    // The code owner is a separate write member, not the PR author.
    let f = build_fixture(&db.url, "*").await;
    let (writer_id, writer_username) = add_member(&f.pool, f.org_id, "write").await;

    // Rewrite CODEOWNERS on main to name the writer, and update the PR's
    // target branch so the changed file is owned by the writer.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(
        f.work.join("CODEOWNERS"),
        format!("feat.txt @{writer_username}\n"),
    )
    .unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "owners",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    // A head change on `feature` triggers the request job; a push replays.
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat.txt"), "feature v2\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "v2",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    run_jobs(&f.state).await;

    let pr = pull_id(&f).await;
    let requested: Vec<Uuid> = sqlx::query_scalar(
        "SELECT reviewer_id FROM pull_request_review_requests WHERE pull_request_id = $1",
    )
    .bind(pr)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(requested, vec![writer_id]);

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_team_code_owner_requests_each_member() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "*").await;
    let (m1, _) = add_member(&f.pool, f.org_id, "write").await;
    let (m2, _) = add_member(&f.pool, f.org_id, "write").await;
    let team_slug = add_team(&f.pool, f.org_id, &[m1, m2]).await;

    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(
        f.work.join("CODEOWNERS"),
        format!("feat.txt @{}/{team_slug}\n", f.org_slug),
    )
    .unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "owners",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    // Trigger the request job with a head change.
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat.txt"), "feature v2\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "v2",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    run_jobs(&f.state).await;

    let pr = pull_id(&f).await;
    let requested: Vec<Uuid> = sqlx::query_scalar(
        "SELECT reviewer_id FROM pull_request_review_requests \
         WHERE pull_request_id = $1 ORDER BY reviewer_id",
    )
    .bind(pr)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    let mut expected = vec![m1, m2];
    expected.sort();
    assert_eq!(requested, expected);

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Merge gate
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_and_patch_report_code_owner_facts() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "feat.txt @nobody\n").await;

    // The create response must not claim a false green before CODEOWNERS is
    // evaluated: the required owner is reported and not yet approved (#157).
    assert_eq!(f.create_body["code_owner_review_satisfied"], false);
    let owners = f.create_body["required_code_owners"].as_array().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0]["name"], "@nobody");
    assert_eq!(owners[0]["approved"], false);

    let (status, patched) = request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        json!({"title": "renamed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["code_owner_review_satisfied"], false);
    let owners = patched["required_code_owners"].as_array().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0]["name"], "@nobody");
    assert_eq!(owners[0]["approved"], false);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn require_code_owner_review_blocks_merge_until_an_owner_approves() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "*").await;
    let (writer_id, writer_username) = add_member(&f.pool, f.org_id, "write").await;
    let writer_token = fixture_session(&f.pool, writer_id).await;

    // The writer owns feat.txt on main.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(
        f.work.join("CODEOWNERS"),
        format!("feat.txt @{writer_username}\n"),
    )
    .unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "owners",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;
    let main_head = git_output(git_cmd(&["-C", tw, "rev-parse", "main"])).await;

    // A head change moves the source branch, so the PR now owns feat.txt.
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat.txt"), "feature v2\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "v2",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;
    let feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "feature"])).await;

    // Require code-owner review on main.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.owner_token),
        json!({"pattern": "main", "require_code_owner_review": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let merge = format!(
        "/api/v1/organizations/{}/repositories/{}/pulls/{}/merge",
        f.org_slug, f.repo_name, f.number
    );

    // Without an owner approval the merge is refused.
    let (status, body) = request(
        &f.app,
        "POST",
        &merge,
        Some(&f.owner_token),
        json!({"method": "merge", "expected_head_oid": feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The PR page reports the required owner as unsatisfied.
    let (status, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["code_owner_review_satisfied"], false);
    let owners = detail["required_code_owners"].as_array().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0]["name"], format!("@{writer_username}"));
    assert_eq!(owners[0]["approved"], false);

    // The writer (an owner) approves at the head.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&writer_token),
        json!({"verdict": "approved", "commit_id": feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The PR page now reports the owner approved.
    let (_, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(detail["code_owner_review_satisfied"], true);
    assert_eq!(detail["required_code_owners"][0]["approved"], true);

    // The merge is accepted now.
    let (status, body) = request(
        &f.app,
        "POST",
        &merge,
        Some(&f.owner_token),
        json!({"method": "merge", "expected_head_oid": feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "queued");
    let _ = main_head;

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_author_as_sole_code_owner_satisfies_the_review_gate() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "*").await;

    // The PR author (the org owner) is the only account that owns feat.txt.
    let author_username: String = sqlx::query_scalar(
        "SELECT u.username FROM pull_requests pr JOIN accounts u ON u.id = pr.author_id \
         WHERE pr.repository_id = $1 AND pr.number = $2",
    )
    .bind(f.repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(
        f.work.join("CODEOWNERS"),
        format!("feat.txt @{author_username}\n"),
    )
    .unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "owners",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    // A head change moves the source branch so the PR owns feat.txt.
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat.txt"), "feature v2\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "v2",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;
    let feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "feature"])).await;

    // Require code-owner review on main.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.owner_token),
        json!({"pattern": "main", "require_code_owner_review": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The author is the sole code owner: their ownership satisfies the gate, so
    // the page reports it satisfied rather than permanently blocked.
    let (status, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["code_owner_review_satisfied"], true, "{detail}");
    let owners = detail["required_code_owners"].as_array().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0]["name"], format!("@{author_username}"));
    assert_eq!(owners[0]["approved"], true, "{detail}");

    // The merge is accepted: the sole-owner author does not need a self-review.
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/pulls/{}/merge",
            f.org_slug, f.repo_name, f.number
        ),
        Some(&f.owner_token),
        json!({"method": "merge", "expected_head_oid": feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "queued");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn malformed_codeowners_tokens_are_reported_on_the_pr_page() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "feat.txt @ @acme/ @acme/a/b").await;

    // The JSON API lists the malformed owner tokens as code-owner errors.
    let (status, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let errors = detail["code_owner_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0], "malformed owner token `@`");
    assert_eq!(errors[1], "malformed owner token `@acme/`");
    assert_eq!(errors[2], "malformed owner token `@acme/a/b`");

    // The HTML page renders the same errors.
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/organizations/{}/repositories/{}/pulls/{}",
                    f.org_slug, f.repo_name, f.number
                ))
                .header("cookie", format!("githearth_session={}", f.owner_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("<h2>Code owner errors</h2>"), "{html}");
    assert!(html.contains("malformed owner token `@`"), "{html}");
    assert!(html.contains("malformed owner token `@acme/`"), "{html}");
    assert!(html.contains("malformed owner token `@acme/a/b`"), "{html}");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn review_fails_closed_when_the_changed_file_list_is_truncated() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "*").await;

    // A head change touching more than MAX_DIFF_FILES files truncates the
    // changed-file listing, so the code-owner review cannot be evaluated and
    // must report the pull request as unsatisfied.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    std::fs::create_dir_all(f.work.join("many")).unwrap();
    for i in 0..301 {
        std::fs::write(f.work.join(format!("many/f{i:03}.txt")), "x\n").unwrap();
    }
    git_ok(git_cmd(&["-C", tw, "add", "many"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "many",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    let (status, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["code_owner_review_satisfied"], false);
    assert!(
        detail["required_code_owners"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn review_fails_closed_when_codeowners_is_unreadable() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url, "*").await;

    // A binary (non-UTF-8) CODEOWNERS on the target branch cannot be parsed, so
    // the review must fail closed rather than treat the pull request as having
    // no owners.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(f.work.join("CODEOWNERS"), b"* @owner\xff\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "binary owners",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    let (status, detail) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["code_owner_review_satisfied"], false);
    assert!(!detail["code_owner_errors"].as_array().unwrap().is_empty());

    finish(f).await;
    db.drop_db().await;
}
