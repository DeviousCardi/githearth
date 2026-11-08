//! Review request contract tests (PostgreSQL-backed).
//!
//! A pull request author (or an approver) requests a review from another
//! account; the reviewer is notified in-band, and the request disappears once
//! the reviewer submits a review or it is removed. A real Git fixture is built
//! once per test because submitting a review must name the live head commit.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
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

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    work: PathBuf,
    org_slug: String,
    org_id: Uuid,
    author_id: Uuid,
    author_username: String,
    author_token: String,
    repo_name: String,
    feature_head: String,
    number: i64,
    pull_id: Uuid,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn review_requests_href(org: &str, repo: &str, number: i64) -> String {
    format!("{}/{number}/requested_reviewers", repo_href(org, repo))
}

async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-reviews-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 8,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        job_poll_interval_ms: 50,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let author_username = tag("author");
    let author_id = fixture_account(&pool, &author_username).await;
    let author_token = fixture_session(&pool, author_id).await;
    let (org_id, org_slug) = fixture_organization(&pool, author_id).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&author_token),
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);

    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-reviews-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
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
    let feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
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
        Some(&author_token),
        json!({"title": "pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();

    Fixture {
        app,
        state,
        pool,
        storage_root,
        work,
        org_slug,
        org_id,
        author_id,
        author_username,
        author_token,
        repo_name,
        feature_head,
        number,
        pull_id,
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
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
}

// ---------------------------------------------------------------------------
// Requests, listing, dedup, and notifications
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn author_requests_a_member_and_is_notified() {
    let f = build_fixture().await;

    let (reviewer_id, reviewer_username) = add_member(&f.pool, f.org_id, "write").await;
    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);

    // The author requests the write member: 200 and the member is listed.
    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reviewer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reviewer_id"], reviewer_id.to_string());
    assert_eq!(items[0]["username"], reviewer_username);
    assert_eq!(items[0]["requested_by"], f.author_id.to_string());

    // GET lists the same single request.
    let (status, body) = request(&f.app, "GET", &href, Some(&f.author_token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["reviewer_id"], reviewer_id.to_string());

    // Requesting again is idempotent: still one row, still one job.
    let (status, _) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reviewer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pull_request_review_requests WHERE pull_request_id = $1",
    )
    .bind(f.pull_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'notification.pull_request_review_requested' \
         AND payload->>'pull_request_id' = $1",
    )
    .bind(f.pull_id.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(jobs, 1);

    // The reviewer gets exactly one unread notification.
    run_jobs(&f.state).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 \
         AND kind = 'pull_request.review_requested' AND read_at IS NULL",
    )
    .bind(reviewer_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reviewer_submitting_a_review_clears_the_request() {
    let f = build_fixture().await;

    let (reviewer_id, reviewer_username) = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer_id).await;
    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);

    let (status, _) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reviewer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&reviewer_token),
        json!({"verdict": "approved", "commit_id": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = request(&f.app, "GET", &href, Some(&f.author_token), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0, "{body}");

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Validation and permission rules
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn request_rules_reject_author_unknown_and_read_only_reviewer() {
    let f = build_fixture().await;
    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);

    // Requesting the author themself.
    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": f.author_username}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"],
        "the author cannot review their own pull request"
    );

    // Unknown username.
    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": "no-such-user"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "reviewer not found");

    // A read-only member cannot approve, so cannot be requested.
    let (_reader_id, reader_username) = add_member(&f.pool, f.org_id, "read").await;
    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reader_username}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "reviewer cannot approve this pull request");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_member_cannot_request_someone() {
    let f = build_fixture().await;

    let (_writer_id, writer_username) = add_member(&f.pool, f.org_id, "write").await;
    let (reader_id, _reader_username) = add_member(&f.pool, f.org_id, "read").await;
    let reader_token = fixture_session(&f.pool, reader_id).await;
    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);

    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&reader_token),
        json!({"reviewer": writer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "not permitted");

    // The writer (who can approve) could still be requested by the author.
    let (status, _) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": writer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    finish(f).await;
}

// ---------------------------------------------------------------------------
// DELETE and closed pull request
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn remove_request_and_remove_again() {
    let f = build_fixture().await;

    let (_reviewer_id, reviewer_username) = add_member(&f.pool, f.org_id, "write").await;
    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);
    let (status, _) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reviewer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let item_href = format!("{href}/{reviewer_username}");
    let (status, _) = request(
        &f.app,
        "DELETE",
        &item_href,
        Some(&f.author_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = request(
        &f.app,
        "DELETE",
        &item_href,
        Some(&f.author_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn closed_pull_request_refuses_a_request() {
    let f = build_fixture().await;

    let (_reviewer_id, reviewer_username) = add_member(&f.pool, f.org_id, "write").await;

    // Close the pull request.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.author_token),
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let href = review_requests_href(&f.org_slug, &f.repo_name, f.number);
    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.author_token),
        json!({"reviewer": reviewer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pull request is not open");

    finish(f).await;
}
