//! M3 exit-criteria test suite.
//!
//! Proves each roadmap M3 exit criterion against the real code, reusing the
//! helpers and patterns of the other PostgreSQL-backed contract tests
//! (`merge_worker.rs`, `merge_intents.rs`, `reconcile.rs`, `pulls*.rs`):
//!
//! 1. Two concurrent merges into one target: exactly one wins, the loser
//!    re-validates stale; the target history is never rewound.
//! 2. A passing check on head A cannot authorize merging head B.
//! 3. Losing the metadata response after the Git ref moved is reconciled
//!    exactly once.
//! 4. Delivering the same merge operation twice merges once, with one journal
//!    entry and one event.
//! 5. Revoking the merger's permission between intent creation and execution
//!    behaves as ADR 0004 documents (stale, "actor no longer authorized").
//! 6. API + UI exercise every supported merge method, refuse a conflict, and
//!    cancel a queued merge.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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
        .args(args);
    cmd
}

async fn git_run(mut cmd: Command) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(60),
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

// ---------------------------------------------------------------------------
// JSON API helper
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

/// Adds an account with the given organization-scope grant role; returns its id.
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

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
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
    head: String,
    feature_head: String,
    work: PathBuf,
    number: i64,
}

/// One repository with `main` (target) and `feature` (source), plus an open
/// pull request `feature -> main`. With `conflict`, the two branches edit the
/// same file so a merge candidate conflicts.
async fn build_fixture(conflict: bool) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-m3-{}", Uuid::new_v4()));
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

    let work = std::env::temp_dir().join(format!("git-m3-work-{}", Uuid::new_v4()));
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

    let (head, feature_head) = if conflict {
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
        head,
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

async fn main_tip(f: &Fixture) -> String {
    git_output(git_cmd(&[
        "--git-dir",
        state_repo_path(&f.storage_root, f.repo_id)
            .to_str()
            .unwrap(),
        "rev-parse",
        "main",
    ]))
    .await
}

async fn commit_parents(f: &Fixture, oid: &str) -> Vec<String> {
    let line = git_output(git_cmd(&[
        "--git-dir",
        state_repo_path(&f.storage_root, f.repo_id)
            .to_str()
            .unwrap(),
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
        state_repo_path(&f.storage_root, f.repo_id)
            .to_str()
            .unwrap(),
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
// UI helpers
// ---------------------------------------------------------------------------

async fn ui_send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
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

async fn ui_get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, _, body) = ui_send(app, builder.body(Body::empty()).unwrap()).await;
    (status, body)
}

async fn ui_post_form(
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
    ui_send(
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

fn ui_hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

fn ui_csrf(html: &str) -> String {
    ui_hidden(html, "csrf_token")
}

fn ui_key(html: &str) -> String {
    ui_hidden(html, "idempotency_key")
}

// ---------------------------------------------------------------------------
// Criterion 1: two concurrent merges into the same target
// ---------------------------------------------------------------------------

/// A repository with two feature branches (`feature1`, `feature2`) and two
/// open pull requests, both targeting `main` and editing different files so
/// neither conflicts with the other.
async fn build_two_pr_fixture() -> (Fixture, String, i64, String, i64) {
    let f = build_fixture(false).await;
    // `build_fixture` already made `feature` + PR #1. Make a second branch and
    // PR from the same root so it does not overlap the first.
    let tw = f.work.to_str().unwrap();
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
        state_repo_path(&f.storage_root, f.repo_id)
            .to_str()
            .unwrap(),
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

    let feature1_head = f.feature_head.clone();
    let number1 = f.number;
    (f, feature1_head, number1, feature2_head, number2)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_concurrent_merges_into_one_target_one_wins_and_history_is_never_rewound() {
    let (f, feature1_head, number1, feature2_head, number2) = build_two_pr_fixture().await;
    let root = f.head.clone();

    let (s1, intent1) = request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, number1),
        Some(&f.token),
        None,
        json!({"method": "merge", "expected_head_oid": feature1_head}),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "{intent1}");
    let op1: Uuid = intent1["operation_id"].as_str().unwrap().parse().unwrap();

    let (s2, intent2) = request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, number2),
        Some(&f.token),
        None,
        json!({"method": "merge", "expected_head_oid": feature2_head}),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED, "{intent2}");
    let op2: Uuid = intent2["operation_id"].as_str().unwrap().parse().unwrap();

    // Two passes race over the same target; the per-repository lock and the
    // compare-and-swap ensure exactly one moves `main`.
    let (a, b) = tokio::join!(
        githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id)),
        githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id)),
    );
    let total = a.unwrap() + b.unwrap();
    assert_eq!(total, 2, "both intents are claimed and run");

    let state1 = intent_state(&f, op1).await;
    let state2 = intent_state(&f, op2).await;
    let merged = [&state1, &state2]
        .iter()
        .filter(|s| s.as_str() == "merged")
        .count();
    let stale = [&state1, &state2]
        .iter()
        .filter(|s| s.as_str() == "stale")
        .count();
    assert_eq!(merged, 1, "exactly one intent merges: {state1}/{state2}");
    assert_eq!(stale, 1, "the loser re-validates stale: {state1}/{state2}");

    let winner = if state1 == "merged" { op1 } else { op2 };
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(winner)
            .fetch_one(&f.pool)
            .await
            .unwrap();

    // The target contains the winner and was never rewound: the root commit is
    // still an ancestor of the new tip.
    assert_eq!(main_tip(&f).await, result_oid);
    let anc = git_output(git_cmd(&[
        "--git-dir",
        state_repo_path(&f.storage_root, f.repo_id)
            .to_str()
            .unwrap(),
        "merge-base",
        "--is-ancestor",
        &root,
        &result_oid,
    ]))
    .await;
    assert_eq!(anc, "", "root must remain an ancestor of the winner");

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Criterion 2: a passing check on head A cannot authorize head B
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_passing_check_on_head_a_cannot_authorize_merging_head_b() {
    let f = build_fixture(false).await;
    let head_a = f.feature_head.clone();

    // Require "ci" on main.
    let (status, _) = request(
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
    assert_eq!(status, StatusCode::CREATED);

    // A passing check at head A.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/commits/{head_a}/statuses",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Push after the check: the source head moves to B.
    let head_b = advance_branch(&f, "feature", "feat2.txt", "second\n").await;

    // Merging B must be refused: the only passing "ci" status binds to A.
    let (status, body) = create_merge_intent(&f, "merge", &head_b).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let intents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE repository_id = $1")
            .bind(f.repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intents, 0, "no intent may be queued for un-checked code");

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Criterion 3: lost metadata after the Git commit is reconciled exactly once
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn lost_metadata_after_git_commit_is_reconciled_exactly_once() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Move the ref through the journaling hook (a fast-forward merge) without
    // recording any metadata: this is the "committed before metadata" crash.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let hooks_dir = f.storage_root.join("hooks");
    let actor = f.owner_id;
    let old = f.head.clone();
    let new = f.feature_head.clone();
    let op_for_hook = op;
    tokio::task::spawn_blocking(move || {
        githearth_git::merge::update_ref_expected(
            &repo_path,
            &hooks_dir,
            "refs/heads/main",
            &new,
            &old,
            op_for_hook,
            actor,
            githearth_git::journal::Origin::Merge,
            Duration::from_secs(30),
        )
    })
    .await
    .unwrap()
    .unwrap();

    // The ref moved but metadata is still absent.
    assert_eq!(main_tip(&f).await, f.feature_head);
    assert_eq!(intent_state(&f, op).await, "queued");
    let applied: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id = $1")
        .bind(f.repo_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(applied, 0, "metadata was not yet applied");

    // The reconciler's journal replay records the merge.
    let _guard = f
        .state
        .storage
        .repository_lock(f.repo_id)
        .lock_owned()
        .await;
    let summary = githearth_server::pushes::replay(&f.state, f.repo_id)
        .await
        .unwrap();
    assert_eq!(summary.committed, 1);

    assert_eq!(intent_state(&f, op).await, "merged");
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(result_oid, f.feature_head);
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
    assert_eq!(refs, 1, "the merge is recorded exactly once");

    // Replaying again changes nothing.
    let second = githearth_server::pushes::replay(&f.state, f.repo_id)
        .await
        .unwrap();
    assert_eq!(second.committed, 0);
    drop(_guard);
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(f.repo_id)
    .bind(sequence)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1);

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Criterion 4: duplicate delivery merges once, one journal entry, one event
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn duplicate_delivery_merges_once_with_one_journal_entry_and_one_event() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Simulate a worker that moved the ref and journaled the merge, then died
    // before recording metadata: the intent holds an expired lease in
    // `executing`, and the journal has the committed merge record.
    sqlx::query(
        "UPDATE merge_intents SET state = 'executing', lease_holder = $1, \
         lease_attempt = 1, lease_expires_at = now() - interval '1 second' WHERE operation_id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(op)
    .execute(&f.pool)
    .await
    .unwrap();
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let hooks_dir = f.storage_root.join("hooks");
    let actor = f.owner_id;
    let old = f.head.clone();
    let new = f.feature_head.clone();
    let op_for_hook = op;
    tokio::task::spawn_blocking(move || {
        githearth_git::merge::update_ref_expected(
            &repo_path,
            &hooks_dir,
            "refs/heads/main",
            &new,
            &old,
            op_for_hook,
            actor,
            githearth_git::journal::Origin::Merge,
            Duration::from_secs(30),
        )
    })
    .await
    .unwrap()
    .unwrap();

    // A second worker reclaims the intent and re-delivers the same operation:
    // the journal replay records it and returns AlreadyMerged.
    let ran = githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "merged");

    // A third delivery is a no-op: a merged intent is terminal and not
    // claimable.
    let ran = githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0);

    let sequence: i64 =
        sqlx::query_scalar("SELECT journal_sequence FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let pushes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repository_pushes WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(f.repo_id)
    .bind(sequence)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(pushes, 1, "one journal entry");
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(f.repo_id)
    .bind(sequence)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1, "the ref moved once");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE organization_id = $1 AND kind = 'pull_request.merged'",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "one merge event");

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Criterion 5: revocation between intent creation and execution
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn revoking_the_merger_between_intent_and_execution_goes_stale() {
    let f = build_fixture(false).await;

    // A write member may merge; create the intent as that member.
    let member = add_member(&f.pool, f.org_id, "write").await;
    let member_token = fixture_session(&f.pool, member).await;
    let (status, intent) = request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&member_token),
        None,
        json!({"method": "merge", "expected_head_oid": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    // Revoke the member's organization grant before the worker executes.
    sqlx::query(
        "UPDATE grants SET revoked_at = now(), revoked_by = $1 \
         WHERE subject_id = $2 AND scope_id = $3 AND revoked_at IS NULL",
    )
    .bind(f.owner_id)
    .bind(member)
    .bind(f.org_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let before = main_tip(&f).await;
    let ran = githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert_eq!(intent_state(&f, op).await, "stale");
    let reason = intent_failure_reason(&f, op).await.unwrap();
    assert_eq!(reason, "actor no longer authorized");
    assert_eq!(main_tip(&f).await, before, "the ref must not move");

    finish(f).await;
}

// ---------------------------------------------------------------------------
// Criterion 6: API + UI merge methods, conflict, cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn each_supported_merge_method_merges_successfully() {
    for method in ["merge", "squash", "rebase", "ff_only"] {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, method, &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED, "{method}: {intent}");
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

        let ran = githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id))
            .await
            .unwrap();
        assert_eq!(ran, 1, "{method}");
        assert_eq!(intent_state(&f, op).await, "merged", "{method}");

        let result_oid: String =
            sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
                .bind(op)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(main_tip(&f).await, result_oid, "{method}");

        match method {
            "merge" => {
                let parents = commit_parents(&f, &result_oid).await;
                assert_eq!(
                    parents.len(),
                    3,
                    "merge commit has two parents: {parents:?}"
                );
            }
            "ff_only" => {
                assert_eq!(result_oid, f.feature_head, "fast-forward reuses the source");
            }
            _ => {}
        }

        finish(f).await;
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_conflicting_merge_is_refused_without_moving_the_ref() {
    let f = build_fixture(true).await;
    let before = main_tip(&f).await;

    // A conflicting merge is refused up front: the API records a `conflicted`
    // intent and returns 409, moving no ref. The worker's `Conflicted` safety
    // net is covered directly in `tests/merge_execute.rs`.
    let (status, body) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["state"], "conflicted");
    assert_eq!(main_tip(&f).await, before, "the ref must not move");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_queued_merge_can_be_cancelled_through_the_api() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op = intent["operation_id"].as_str().unwrap().to_string();

    let (status, body) = request(
        &f.app,
        "DELETE",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "cancelled");
    assert_eq!(body["operation_id"].as_str().unwrap(), op);

    let before = main_tip(&f).await;
    let ran = githearth_server::merge_worker::run_once(&f.state, Some(f.repo_id))
        .await
        .unwrap();
    assert_eq!(ran, 0, "a cancelled intent is not claimable");
    assert_eq!(main_tip(&f).await, before, "the ref must not move");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ui_merge_form_offers_methods_queues_and_cancels() {
    let f = build_fixture(false).await;
    let cookie = format!("githearth_session={}", f.token);
    let base = format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name);

    // The owner sees the merge form with every method on the open PR.
    let (status, page) = ui_get(&f.app, &format!("{base}/pulls/{}", f.number), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    for label in [
        "Merge commit",
        "Squash and merge",
        "Rebase and merge",
        "Fast-forward only",
    ] {
        assert!(page.contains(label), "{label} missing from {page}");
    }

    // Submit the merge form (squash) to queue an intent.
    let (token, key, head) = (
        ui_csrf(&page),
        ui_key(&page),
        ui_hidden(&page, "expected_head_oid"),
    );
    assert_eq!(head, f.feature_head);
    let (status, location, _) = ui_post_form(
        &f.app,
        &format!("{base}/pulls/{}/merge", f.number),
        &cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &key),
            ("method", "squash"),
            ("expected_head_oid", &head),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.as_deref(),
        Some(format!("{base}/pulls/{}", f.number).as_str())
    );

    let (status, page) = ui_get(&f.app, &format!("{base}/pulls/{}", f.number), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Merge: Queued"), "{page}");
    assert!(!page.contains("name=\"method\""), "no form while queued");

    // Cancel the queued intent through the API; the page then reports Cancelled.
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

    let (status, page) = ui_get(&f.app, &format!("{base}/pulls/{}", f.number), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Merge: Cancelled"), "{page}");

    finish(f).await;
}
