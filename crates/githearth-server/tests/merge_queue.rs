//! Merge queue contract tests (PostgreSQL-backed).
//!
//! A branch protection rule can demand `require_merge_queue`; merge intents for
//! pull requests targeting that branch are enqueued (in_queue) and merged one
//! at a time. The worker re-targets a queue entry whose target moved instead of
//! failing it, and skips/dequeues entries that genuinely fail with a reason.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::merge_worker;
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
    token: String,
    repo_name: String,
    repo_id: Uuid,
    feature_head: String,
    number: i64,
    repo_path: PathBuf,
    work: PathBuf,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-mq-{}", Uuid::new_v4()));
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

    let owner = fixture_account(&pool, &tag("owner")).await;
    let token = fixture_session(&pool, owner).await;
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;

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
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-mq-work-{}", Uuid::new_v4()));
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
        token,
        repo_name,
        repo_id,
        feature_head,
        number,
        repo_path,
        work,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn create_merge_intent(f: &Fixture, number: i64, head: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, number),
        Some(&f.token),
        None,
        json!({"method": "merge", "expected_head_oid": head}),
    )
    .await
}

async fn set_merge_queue(f: &Fixture) {
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"pattern": "main", "require_merge_queue": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn main_tip(f: &Fixture) -> String {
    git_output(git_cmd(&[
        "--git-dir",
        f.repo_path.to_str().unwrap(),
        "rev-parse",
        "main",
    ]))
    .await
}

async fn intent_state(f: &Fixture, operation_id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
        .bind(operation_id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

async fn advance_branch(f: &Fixture, branch: &str, file: &str, content: &str) -> String {
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", branch])).await;
    std::fs::write(f.work.join(file), content).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", file])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        &format!("advance {branch}"),
    ]))
    .await;
    let oid = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        f.repo_path.to_str().unwrap(),
        branch,
    ]))
    .await;
    oid
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn queue_entry_is_enqueued_and_merges() {
    let f = build_fixture().await;
    set_merge_queue(&f).await;

    let (status, intent) = create_merge_intent(&f, f.number, &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(intent["in_queue"], true);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "merged");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn queue_entry_re_targets_when_the_target_moves_instead_of_going_stale() {
    let f = build_fixture().await;
    set_merge_queue(&f).await;

    let (status, intent) = create_merge_intent(&f, f.number, &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    let original_target: String = intent["expected_target_oid"].as_str().unwrap().to_owned();

    // The target moves after the intent is queued.
    advance_branch(&f, "main", "main2.txt", "main2\n").await;

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    // The entry is claimed once to re-target and once to merge.
    assert_eq!(ran, 2);
    assert_eq!(
        intent_state(&f, op).await,
        "merged",
        "re-targeted and merged"
    );
    let expected_target: String =
        sqlx::query_scalar("SELECT expected_target_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_ne!(
        expected_target, original_target,
        "the queue entry re-targeted to the new target tip"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_queue_entries_merge_one_at_a_time_in_order() {
    let f = build_fixture().await;
    set_merge_queue(&f).await;

    // A second branch and pull request, both targeting main.
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "-b", "feature2"])).await;
    std::fs::write(f.work.join("feat2.txt"), "feature2\n").unwrap();
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
        "feature2",
    ]))
    .await;
    let feature2_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        f.repo_path.to_str().unwrap(),
        "feature2",
    ]))
    .await;
    let (status, pull2) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": "pull2", "source_branch": "feature2", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull2}");
    let number2 = pull2["number"].as_i64().unwrap();

    let (s1, intent1) = create_merge_intent(&f, f.number, &f.feature_head).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, intent2) = create_merge_intent(&f, number2, &feature2_head).await;
    assert_eq!(s2, StatusCode::CREATED);
    let op1: Uuid = intent1["operation_id"].as_str().unwrap().parse().unwrap();
    let op2: Uuid = intent2["operation_id"].as_str().unwrap().parse().unwrap();

    // One worker pass runs both entries in order; the second is re-targeted to
    // the first's merge result and then merges too.
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    // The first entry is claimed once and the second twice (re-target + merge).
    assert_eq!(ran, 3, "both queue entries ran");
    assert_eq!(intent_state(&f, op1).await, "merged");
    assert_eq!(intent_state(&f, op2).await, "merged");

    let result2: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op2)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(main_tip(&f).await, result2, "main ends at the second merge");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cancelling_a_queue_entry_prevents_its_merge() {
    let f = build_fixture().await;
    set_merge_queue(&f).await;

    let (status, intent) = create_merge_intent(&f, f.number, &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    let (status, _) = request(
        &f.app,
        "DELETE",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let before = main_tip(&f).await;
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0, "a cancelled queue entry is not claimable");
    assert_eq!(intent_state(&f, op).await, "cancelled");
    assert_eq!(main_tip(&f).await, before);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_stale_approval_cannot_authorize_a_different_head_in_the_queue() {
    let f = build_fixture().await;
    set_merge_queue(&f).await;

    // An approval at the current head.
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id = $1")
        .bind(f.repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let reviewer = add_member(&f.pool, org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&reviewer_token),
        None,
        json!({"verdict": "approved", "commit_id": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, intent) = create_merge_intent(&f, f.number, &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // The source head moves after the approval and intent; the digest recorded
    // by the intent binds the old head, so the queue entry must not merge the
    // new head.
    advance_branch(&f, "feature", "feat2.txt", "feat2\n").await;
    let before = main_tip(&f).await;

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "stale");
    assert_eq!(main_tip(&f).await, before, "the ref must not move");

    finish(f).await;
}
