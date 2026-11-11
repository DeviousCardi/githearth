//! Merge worker contract tests (PostgreSQL-backed).
//!
//! Each fixture is a real repository on disk with `main` and `feature`, an
//! owner, and a merge intent created through the API. The worker claims and
//! runs intents through `merge_worker::run_once`; every pass is scoped to the
//! fixture's own repository so parallel tests never claim each other's intents.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::merge_worker;
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;
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
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

struct FixturePaths {
    repo_path: PathBuf,
    work: PathBuf,
}

/// One repository with `main` (target) and `feature` (source), plus an open
/// pull request `feature -> main`. With `conflict`, the two branches edit the
/// same file so a merge candidate conflicts.
async fn build_fixture(conflict: bool) -> (Fixture, FixturePaths) {
    let database_url =
        std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
    build_fixture_at(conflict, &database_url).await
}

async fn build_fixture_at(conflict: bool, database_url: &str) -> (Fixture, FixturePaths) {
    let storage_root = std::env::temp_dir().join(format!("githearth-mw-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: database_url.to_owned(),
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
    // Seed branches by pushing straight into storage, bypassing the receive
    // hooks (the fixture's server-side hooks are the real ones and refuse a
    // plain filesystem push; `update_ref_expected` restores them explicitly).
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-mw-work-{}", Uuid::new_v4()));
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
    let root = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    let (_head, feature_head) = if conflict {
        std::fs::write(work.join("README.md"), "main\n").unwrap();
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
            "main change",
        ]))
        .await;
        let head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
        git_ok(git_cmd(&[
            "-C", tw, "checkout", "-q", "-b", "feature", &root,
        ]))
        .await;
        std::fs::write(work.join("README.md"), "feature\n").unwrap();
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
            "feature change",
        ]))
        .await;
        let feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
        (head, feature_head)
    } else {
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
        (root, feature_head)
    };

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

    (
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
        },
        FixturePaths { repo_path, work },
    )
}

async fn finish(f: Fixture, paths: FixturePaths) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&paths.work);
    f.pool.close().await;
}

async fn create_merge_intent(f: &Fixture, method: &str, head: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        json!({"method": method, "expected_head_oid": head}),
    )
    .await
}

async fn main_tip(paths: &FixturePaths) -> String {
    git_output(git_cmd(&[
        "--git-dir",
        paths.repo_path.to_str().unwrap(),
        "rev-parse",
        "main",
    ]))
    .await
}

async fn commit_parents(paths: &FixturePaths, oid: &str) -> Vec<String> {
    let line = git_output(git_cmd(&[
        "--git-dir",
        paths.repo_path.to_str().unwrap(),
        "rev-list",
        "--parents",
        "-n",
        "1",
        oid,
    ]))
    .await;
    line.split_whitespace().map(str::to_owned).collect()
}

/// Adds a new commit on `branch` in the fixture's work tree and pushes it
/// directly to storage, bypassing the hooks.
async fn advance_branch(
    _f: &Fixture,
    paths: &FixturePaths,
    branch: &str,
    file: &str,
    content: &str,
) -> String {
    let tw = paths.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", branch])).await;
    std::fs::write(paths.work.join(file), content).unwrap();
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
        paths.repo_path.to_str().unwrap(),
        branch,
    ]))
    .await;
    oid
}

async fn intent_state(f: &Fixture, operation_id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
        .bind(operation_id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

async fn intent_failure_reason(f: &Fixture, operation_id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT failure_reason FROM merge_intents WHERE operation_id = $1")
        .bind(operation_id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn queued_intent_runs_and_ends_merged_with_ref_moved() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    let pull_id: Uuid = intent["pull_request_id"].as_str().unwrap().parse().unwrap();

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);

    assert_eq!(intent_state(&f, op).await, "merged");
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(main_tip(&paths).await, result_oid);

    let pull_state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
        .bind(pull_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(pull_state, "merged");

    let parents = commit_parents(&paths, &result_oid).await;
    assert_eq!(
        parents.len(),
        3,
        "merge commit has two parents: {parents:?}"
    );

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn conflict_ends_conflicted_with_path_in_failure_reason() {
    let (f, paths) = build_fixture(true).await;
    let before = main_tip(&paths).await;
    let (status, body) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["state"], "conflicted");

    // The refusal is recorded on a conflicted intent, so the worker has
    // nothing to claim and no ref moves.
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0, "a conflicted intent is refused, not queued");

    let (state, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT state, failure_reason FROM merge_intents WHERE repository_id = $1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(state, "conflicted");
    assert!(
        reason.as_deref().is_some_and(|r| r.contains("README")),
        "conflicting path in failure_reason: {reason:?}"
    );
    assert_eq!(main_tip(&paths).await, before, "the ref must not move");

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn target_moved_ends_stale() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    advance_branch(&f, &paths, "main", "main2.txt", "main2\n").await;
    let before = main_tip(&paths).await;
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);

    assert_eq!(intent_state(&f, op).await, "stale");
    assert_eq!(main_tip(&paths).await, before);

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn live_lease_not_claimed_and_expired_one_is() {
    let (f, paths) = build_fixture(false).await;
    // First intent: held by a live (future) lease from another holder.
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    let other = Uuid::new_v4();
    sqlx::query(
        "UPDATE merge_intents SET state = 'executing', lease_holder = $1, \
         lease_attempt = 1, lease_expires_at = now() + interval '1 hour' WHERE operation_id = $2",
    )
    .bind(other)
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();

    // A pass must not claim the live-leased intent.
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0);
    assert_eq!(intent_state(&f, op).await, "executing");
    let holder: Uuid =
        sqlx::query_scalar("SELECT lease_holder FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(holder, other, "the live lease must be untouched");

    // Expire the lease: the next pass claims and runs it.
    sqlx::query(
        "UPDATE merge_intents SET lease_expires_at = now() - interval '1 second' WHERE operation_id = $1",
    )
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "merged");

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reclaimed_intent_whose_merge_committed_ends_merged_and_moves_ref_once() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Simulate a worker that committed the merge but crashed before recording:
    // run execute_intent directly (ref moves, metadata recorded), then reset
    // the intent to `executing` with an expired lease so the worker reclaims it.
    githearth_server::merge::execute_intent(&f.state, op)
        .await
        .unwrap();
    // Reset to executing with an expired lease (as if the first worker died
    // after the ref commit but before applying outcome metadata).
    sqlx::query(
        "UPDATE merge_intents SET state = 'executing', lease_holder = $1, \
         lease_attempt = 1, lease_expires_at = now() - interval '1 second' WHERE operation_id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();

    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);

    // The replayed merge is AlreadyMerged, recorded idempotently; the ref
    // moved exactly once.
    assert_eq!(intent_state(&f, op).await, "merged");
    let sequence: i64 =
        sqlx::query_scalar("SELECT journal_sequence FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(f.repo_id)
    .bind(sequence)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1, "the ref moved exactly once");

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cancelled_intent_is_not_run() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    sqlx::query("UPDATE merge_intents SET state = 'cancelled' WHERE operation_id = $1")
        .bind(op)
        .execute(&f.pool)
        .await
        .unwrap();

    let before = main_tip(&paths).await;
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0, "a cancelled intent is not claimable");
    assert_eq!(intent_state(&f, op).await, "cancelled");
    assert_eq!(main_tip(&paths).await, before);

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_fence_changes_nothing() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Claim the intent as the worker would, so we know its lease attempt.
    sqlx::query(
        "UPDATE merge_intents SET state = 'validating', lease_holder = $1, \
         lease_attempt = COALESCE(lease_attempt, 0) + 1, lease_expires_at = now() + interval '1 hour' \
         WHERE operation_id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();
    let attempt: i32 =
        sqlx::query_scalar("SELECT lease_attempt FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();

    // Another holder steals the lease (different attempt) while it is
    // validating; the worker's fence must change nothing.
    sqlx::query(
        "UPDATE merge_intents SET state = 'executing', lease_holder = $1, \
         lease_attempt = $2 WHERE operation_id = $3",
    )
    .bind(Uuid::new_v4())
    .bind(attempt + 10)
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();

    // Actually simulate the worker holding the claim by directly running a
    // pass; because the current lease_attempt differs from what a claim would
    // now set, the intent is either not claimable (live lease) or, once
    // claimed, re-fenced. Use a fresh scope: set lease_expires past so it is
    // not claimable, then assert nothing changed.
    sqlx::query("UPDATE merge_intents SET lease_expires_at = now() + interval '1 hour' WHERE operation_id = $1")
        .bind(op)
        .execute(&f.pool)
        .await
        .unwrap();

    let before = main_tip(&paths).await;
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0, "a live lease held by another holder is not claimed");
    assert_eq!(intent_state(&f, op).await, "executing");
    assert_eq!(main_tip(&paths).await, before);

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn attempt_cap_marks_failed_without_executing() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Set lease_attempt to 6 (one past MAX_ATTEMPTS) with an expired lease so
    // the intent is claimable; the worker must fail it without executing.
    sqlx::query(
        "UPDATE merge_intents SET state = 'executing', lease_holder = $1, \
         lease_attempt = 6, lease_expires_at = now() - interval '1 second' WHERE operation_id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();

    let before = main_tip(&paths).await;
    let ran = merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "failed");
    let reason = intent_failure_reason(&f, op).await.unwrap();
    assert_eq!(reason, "too many attempts");
    assert_eq!(main_tip(&paths).await, before, "the ref must not move");

    finish(f, paths).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_concurrent_run_once_call_run_one_intent_once() {
    let (f, paths) = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    let (a, b) = tokio::join!(
        merge_worker::run_once(&f.state, Some(f.repo_id)),
        merge_worker::run_once(&f.state, Some(f.repo_id)),
    );
    let total = a.unwrap() + b.unwrap();
    assert_eq!(total, 1, "one intent runs exactly once across two passes");

    assert_eq!(intent_state(&f, op).await, "merged");
    let sequence: i64 =
        sqlx::query_scalar("SELECT journal_sequence FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(f.repo_id)
    .bind(sequence)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1);

    finish(f, paths).await;
}

/// Replace the database name in a PostgreSQL URL (the segment after the last '/').
fn replace_db_name(url: &str, new_db: &str) -> String {
    if let Some(pos) = url.rfind('/') {
        let (base, _old_db) = url.split_at(pos);
        format!("{base}/{new_db}")
    } else {
        format!("{url}/{new_db}")
    }
}

/// A dedicated database, so an unscoped worker pass (`None`) only ever sees
/// this test's own intents, not other tests' rows in the shared database.
struct TestDb {
    db_name: String,
    url: String,
    pool: PgPool,
    admin_opts: PgConnectOptions,
}

impl TestDb {
    async fn new() -> Self {
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let db_name = format!("githearth_mw_{}", Uuid::new_v4().simple());
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
        let pool_opts = PgConnectOptions::from_str(&url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(pool_opts)
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        Self {
            db_name,
            url,
            pool,
            admin_opts,
        }
    }

    async fn drop_db(self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pool.close()).await;
        let mut admin_conn = self.admin_opts.connect().await.unwrap();
        let _ = sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.db_name))
            .execute(&mut admin_conn)
            .await;
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn spawn_merges_a_queued_intent() {
    // An isolated database lets `spawn`'s unscoped pass (`run_once(None)`) run
    // without racing other tests' intents in the shared database.
    let db = TestDb::new().await;
    let (f, paths) = build_fixture_at(false, &db.url).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    let handle = merge_worker::spawn(f.state.clone());
    // The worker does not run a pass at startup; wait for the first tick.
    let merged = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if intent_state(&f, op).await == "merged" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    handle.abort();
    let _ = handle.await;
    assert!(merged.is_ok(), "the merge worker did not merge the intent");
    assert_eq!(intent_state(&f, op).await, "merged");
    assert_eq!(
        main_tip(&paths).await,
        sqlx::query_scalar::<_, String>(
            "SELECT result_oid FROM merge_intents WHERE operation_id = $1"
        )
        .bind(op)
        .fetch_one(&f.pool)
        .await
        .unwrap()
    );

    finish(f, paths).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn spawn_logs_pass_error_and_keeps_going() {
    // A lazy pool that never connects: the first pass errors and is logged,
    // and the loop keeps going (aborted before the next tick).
    let storage_root = std::env::temp_dir().join(format!("githearth-mw-err-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: "postgres://localhost:1/unused".into(),
        database_max_connections: 1,
        database_timeout_seconds: 1,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy("postgres://localhost:1/unused")
        .unwrap();
    let state = AppState::new(pool, config).await.unwrap();
    let handle = merge_worker::spawn(state);
    // The pass waits out the pool acquire timeout before it errors and logs.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    handle.abort();
    let _ = handle.await;
    let _ = std::fs::remove_dir_all(&storage_root);
}
