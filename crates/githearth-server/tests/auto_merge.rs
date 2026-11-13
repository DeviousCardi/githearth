//! Auto-merge contract tests (PostgreSQL-backed).
//!
//! Auto-merge lets a user with merge permission pre-authorize a merge method on
//! an open pull request. The server raises a merge intent automatically once
//! the pull request becomes mergeable, and cancels the auto-merge when the head
//! changes, the pull request closes, or the enabler loses permission.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_git::journal::{Journal, Kind, Record, Update};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, merge_worker, pushes, router};
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
    header_key: Option<(&str, &str)>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some((k, v)) = header_key {
        builder = builder.header(k, v);
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

async fn add_member(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let account = fixture_account(pool, &tag("member")).await;
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
    account
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
    org_slug: String,
    org_id: Uuid,
    token: String,
    owner_id: Uuid,
    repo_name: String,
    repo_id: Uuid,
    feature_head: String,
    work: PathBuf,
    number: i64,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn auto_merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/auto_merge")
}

async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-am-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        // The journaling hook execs the server binary; without it the merge
        // ref update is not journaled, so the worker can never complete one.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool, &tag("owner")).await;
    let token = fixture_session(&pool, owner).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        None,
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);
    // The fixture pushes refs straight to the bare repo, bypassing the server's
    // admission socket; disable the journaling hook for those direct pushes so
    // only the worker's compare-and-swap goes through it.
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-am-work-{}", Uuid::new_v4()));
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
        Some(&token),
        None,
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
        org_slug,
        org_id,
        token,
        owner_id: owner,
        repo_name,
        repo_id,
        feature_head,
        work,
        number,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn approve(f: &Fixture, verdict: &str, commit: &str, token: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(token),
        None,
        json!({"verdict": verdict, "commit_id": commit}),
    )
    .await
}

async fn auto_merge_count(f: &Fixture) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM auto_merges WHERE pull_request_id IN \
        (SELECT id FROM pull_requests WHERE repository_id = $1 AND number = $2)",
    )
    .bind(f.repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap()
}

async fn intent_count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE repository_id = $1")
        .bind(f.repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

async fn set_required_approvals(f: &Fixture, approvals: i32) {
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"pattern": "main", "required_approvals": approvals}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Adds a commit on the feature branch and pushes it straight to storage,
/// bypassing the hooks.
async fn advance_feature(f: &Fixture) -> String {
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat2.txt"), "feat2\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat2.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "advance feature",
    ]))
    .await;
    let oid = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
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
    oid
}

/// Moves `main` (the target) to conflict with the unchanged `feature` head: both
/// now add `feat.txt` with different content. The source head is untouched, so
/// this conflicts without tripping the "head changed" auto-merge cancellation.
async fn conflict_by_moving_main(f: &Fixture) {
    let tw = f.work.to_str().unwrap();
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(f.work.join("feat.txt"), "main\n").unwrap();
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
        "main adds feat",
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn enable_records_auto_merge_and_reads_it_back() {
    let f = build_fixture().await;
    let href = auto_merge_href(&f.org_slug, &f.repo_name, f.number);

    let (status, body) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.token),
        None,
        json!({"method": "squash"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["method"], "squash");
    assert_eq!(body["source_oid"], f.feature_head.as_str());
    assert_eq!(body["enabled_by"], f.owner_id.to_string());

    let (status, read) = request(&f.app, "GET", &href, Some(&f.token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["method"], "squash");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn enable_requires_merge_permission() {
    let f = build_fixture().await;
    let reader = add_member(&f.pool, f.org_id, "read").await;
    let reader_token = fixture_session(&f.pool, reader).await;

    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&reader_token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(auto_merge_count(&f).await, 0);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn enable_immediately_raises_a_merge_intent_when_mergeable() {
    let f = build_fixture().await;
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(auto_merge_count(&f).await, 1);
    assert_eq!(
        intent_count(&f).await,
        1,
        "auto-merge raises the intent immediately"
    );

    let in_queue: bool =
        sqlx::query_scalar("SELECT in_queue FROM merge_intents WHERE repository_id = $1")
            .bind(f.repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(!in_queue, "no queue required by default");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn approval_satisfying_requirements_raises_the_intent() {
    let f = build_fixture().await;
    set_required_approvals(&f, 1).await;

    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent_count(&f).await, 0, "not mergeable before approval");

    let reviewer = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = approve(&f, "approved", &f.feature_head, &reviewer_token).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent_count(&f).await, 1, "approval satisfies auto-merge");
    assert_eq!(auto_merge_count(&f).await, 1, "auto-merge stays enabled");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_status_satisfying_requirements_raises_the_intent() {
    let f = build_fixture().await;

    // Require "ci" on main, but no approvals.
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"pattern": "main", "required_status_checks": ["ci"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Enabling auto-merge is allowed even though the required check is still
    // unsatisfied, and it must not raise the intent prematurely.
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent_count(&f).await, 0, "not mergeable before the check");

    // Posting the required success status must raise the intent without any
    // further review or push.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/commits/{}/statuses",
            f.org_slug, f.repo_name, f.feature_head
        ),
        Some(&f.token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent_count(&f).await, 1, "status satisfies auto-merge");
    assert_eq!(auto_merge_count(&f).await, 1, "auto-merge stays enabled");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cancel_removes_the_auto_merge_and_audits() {
    let f = build_fixture().await;
    let href = auto_merge_href(&f.org_slug, &f.repo_name, f.number);

    let (status, _) = request(
        &f.app,
        "POST",
        &href,
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = request(&f.app, "DELETE", &href, Some(&f.token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(auto_merge_count(&f).await, 0);

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'pull_request.auto_merge_cancelled' \
         AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn closing_the_pull_request_cancels_auto_merge() {
    let f = build_fixture().await;
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(auto_merge_count(&f).await, 0);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_head_change_by_the_reviewer_cancels_auto_merge() {
    let f = build_fixture().await;
    set_required_approvals(&f, 1).await;

    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Someone other than the enabler pushes a new commit to the source branch.
    let new_head = advance_feature(&f).await;
    assert_ne!(new_head, f.feature_head);

    // The next review triggers re-validation, which sees the moved head and
    // cancels instead of authorizing the old head.
    let reviewer = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = approve(&f, "approved", &new_head, &reviewer_token).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(
        auto_merge_count(&f).await,
        0,
        "moved head cancels auto-merge"
    );
    assert_eq!(intent_count(&f).await, 0, "no stale intent was raised");

    // The auto-cancellation audit event names the triggering reviewer and the
    // reason, matching the `on_push` cancellation.
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind = 'pull_request.auto_merge_cancelled' \
         AND organization_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(payload["reason"], "head changed");
    assert_eq!(payload["actor_id"], reviewer.to_string());

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_push_by_someone_else_cancels_auto_merge() {
    let f = build_fixture().await;

    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(auto_merge_count(&f).await, 1);

    // A different account pushes a new commit to the source branch through the
    // committed-push path (`on_push`), which must cancel the auto-merge.
    let other = add_member(&f.pool, f.org_id, "write").await;
    let mut conn = f.pool.acquire().await.unwrap();
    let new_oid = "1".repeat(40);
    let record = Record {
        sequence: 1,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(other),
        origin: None,
        updates: vec![Update {
            old: f.feature_head.clone(),
            new: new_oid,
            reference: "refs/heads/feature".into(),
        }],
    };
    assert!(
        pushes::apply_committed(&mut conn, f.repo_id, &record)
            .await
            .unwrap()
            .0,
        "push applied"
    );

    assert_eq!(
        auto_merge_count(&f).await,
        0,
        "a push by someone else cancels auto-merge"
    );
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'pull_request.auto_merge_cancelled' \
         AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn losing_merge_permission_cancels_auto_merge() {
    let f = build_fixture().await;
    set_required_approvals(&f, 1).await;

    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Revoke the enabler's grant, then trigger re-validation with a review.
    sqlx::query(
        "UPDATE grants SET revoked_at = now(), revoked_by = $1 \
         WHERE subject_id = $1 AND revoked_at IS NULL",
    )
    .bind(f.owner_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let reviewer = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = approve(&f, "approved", &f.feature_head, &reviewer_token).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(
        auto_merge_count(&f).await,
        0,
        "lost permission cancels auto-merge"
    );

    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind = 'pull_request.auto_merge_cancelled' \
         AND organization_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(payload["reason"], "permission lost");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn enablers_own_push_re_raises_the_merge_intent() {
    let f = build_fixture().await;

    // Enable auto-merge (no approvals required): the intent is raised at once.
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent_count(&f).await, 1, "intent raised on enable");

    // Simulate the worker consuming that intent (a stale merge), leaving the
    // auto-merge pending with no live intent.
    sqlx::query("UPDATE merge_intents SET state = 'stale' WHERE repository_id = $1")
        .bind(f.repo_id)
        .execute(&f.pool)
        .await
        .unwrap();

    // The enabler pushes a fresh commit to the source branch.
    let new_head = advance_feature(&f).await;
    assert_ne!(new_head, f.feature_head);

    // Journal that push and replay it, exactly as the server does after a push.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let mut journal = Journal::open(&repo_path).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            Some(f.owner_id),
            vec![Update {
                old: f.feature_head.clone(),
                new: new_head.clone(),
                reference: "refs/heads/feature".into(),
            }],
            None,
        )
        .unwrap();
    let summary = pushes::replay(&f.state, f.repo_id).await.unwrap();
    assert_eq!(summary.committed, 1);

    // The enabler's own push re-targeted the recorded head...
    let source_oid: String = sqlx::query_scalar(
        "SELECT source_oid FROM auto_merges WHERE pull_request_id = \
         (SELECT id FROM pull_requests WHERE repository_id = $1 AND number = $2)",
    )
    .bind(f.repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        source_oid, new_head,
        "auto-merge re-targeted to the new head"
    );

    // ...and re-raised a fresh intent at that head.
    let live_oid: String = sqlx::query_scalar(
        "SELECT source_oid FROM merge_intents WHERE repository_id = $1 \
         AND state IN ('queued', 'validating', 'executing')",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        live_oid, new_head,
        "enabler's own push re-raises the merge intent"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn merging_via_auto_merge_records_its_completion() {
    let f = build_fixture().await;

    // Enable auto-merge on a mergeable pull request (no approvals required):
    // the intent is raised immediately.
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(auto_merge_count(&f).await, 1);

    // The worker merges the raised intent, consuming the auto-merge.
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(auto_merge_count(&f).await, 0, "merged auto-merge is gone");

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind = 'pull_request.auto_merge_completed' \
         AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "the merge records the auto-merge completion");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_permanently_conflicting_pull_cancels_auto_merge_instead_of_accumulating_intents() {
    let f = build_fixture().await;

    // Move the target so the (unchanged) head now conflicts permanently.
    conflict_by_moving_main(&f).await;

    // Enabling auto-merge records the row; the immediate re-check then raises
    // one conflicted intent and cancels the auto-merge (not just leave it on).
    let (status, _) = request(
        &f.app,
        "POST",
        &auto_merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        auto_merge_count(&f).await,
        0,
        "auto-merge is cancelled on conflict"
    );
    assert_eq!(intent_count(&f).await, 1, "exactly one conflicted intent");

    let conflicted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_intents WHERE repository_id = $1 AND state = 'conflicted'",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(conflicted, 1);

    // Later triggers (a status, a review) must not raise fresh conflicted
    // intents: the auto-merge is already gone.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/commits/{}/statuses",
            f.org_slug, f.repo_name, f.feature_head
        ),
        Some(&f.token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let reviewer = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = approve(&f, "approved", &f.feature_head, &reviewer_token).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(
        intent_count(&f).await,
        1,
        "no additional intents accumulate"
    );

    finish(f).await;
}
