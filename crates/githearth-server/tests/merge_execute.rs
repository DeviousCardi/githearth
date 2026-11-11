//! Merge execution contract tests (PostgreSQL-backed).
//!
//! Each fixture is a real repository on disk with `main` and a feature branch,
//! an owner, and a merge intent created through the API. The intent is moved
//! to `executing` with SQL (A's worker does that) before `execute_intent` runs
//! Person B's side of ADR 0004.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::merge::{MergeOutcome, execute_intent};
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
    head: String,
    feature_head: String,
    work: PathBuf,
    repo_path: PathBuf,
    number: i64,
}

impl Fixture {
    fn repo_path_str(&self) -> &str {
        self.repo_path.to_str().unwrap()
    }
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

/// One repository with `main` (target) and `feature` (source), plus an open
/// pull request `feature -> main`. With `conflict`, the two branches edit the
/// same file so a merge candidate conflicts.
async fn build_fixture(conflict: bool) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-merge-{}", Uuid::new_v4()));
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

    let work = std::env::temp_dir().join(format!("git-merge-work-{}", Uuid::new_v4()));
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
        repo_path,
        number,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn create_merge_intent_for(
    f: &Fixture,
    number: i64,
    method: &str,
    head: &str,
) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, number),
        Some(&f.token),
        None,
        json!({"method": method, "expected_head_oid": head}),
    )
    .await
}

async fn create_merge_intent(f: &Fixture, method: &str, head: &str) -> (StatusCode, Value) {
    create_merge_intent_for(f, f.number, method, head).await
}

async fn approve(f: &Fixture, number: i64, verdict: &str, commit: &str, token: &str) {
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
        Some(token),
        None,
        json!({"verdict": verdict, "commit_id": commit}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn set_intent_executing(pool: &PgPool, operation_id: Uuid) {
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(operation_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn main_tip(f: &Fixture) -> String {
    git_output(git_cmd(&[
        "--git-dir",
        f.repo_path_str(),
        "rev-parse",
        "main",
    ]))
    .await
}

async fn commit_parents(f: &Fixture, oid: &str) -> Vec<String> {
    let line = git_output(git_cmd(&[
        "--git-dir",
        f.repo_path_str(),
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
        f.repo_path_str(),
        branch,
    ]))
    .await;
    oid
}

fn expect_merged(outcome: MergeOutcome) -> String {
    match outcome {
        MergeOutcome::Merged { oid } => oid,
        other => panic!("expected Merged, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn merge_method_merges_with_two_parents() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    let oid = expect_merged(execute_intent(&f.state, op).await.unwrap());
    assert_eq!(main_tip(&f).await, oid);

    // The merge commit has the target and source as its two parents.
    let parents = commit_parents(&f, &oid).await;
    assert_eq!(
        parents.len(),
        3,
        "merge commit has two parents: {parents:?}"
    );
    assert_eq!(parents[1], f.head);
    assert_eq!(parents[2], f.feature_head);

    let intent_state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intent_state, "merged");
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(result_oid, oid);
    let pull_id: Uuid =
        sqlx::query_scalar("SELECT pull_request_id FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let pull_state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
        .bind(pull_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(pull_state, "merged");

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

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn squash_and_ff_only_merge() {
    // Squash: one parent, and the PR title appears in the commit message.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "squash", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;

        let oid = expect_merged(execute_intent(&f.state, op).await.unwrap());
        assert_eq!(main_tip(&f).await, oid);
        let parents = commit_parents(&f, &oid).await;
        assert_eq!(
            parents.len(),
            2,
            "squash commit has one parent: {parents:?}"
        );
        let subject = git_output(git_cmd(&[
            "--git-dir",
            f.repo_path_str(),
            "log",
            "-1",
            "--format=%s",
            &oid,
        ]))
        .await;
        assert!(
            subject.contains("pull"),
            "squash message carries the title: {subject}"
        );
        finish(f).await;
    }

    // Fast-forward only: the target advances to the source head.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "ff_only", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;

        let oid = expect_merged(execute_intent(&f.state, op).await.unwrap());
        assert_eq!(oid, f.feature_head);
        assert_eq!(main_tip(&f).await, f.feature_head);
        finish(f).await;
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn second_call_returns_already_merged_without_a_second_ref_update() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    let oid = expect_merged(execute_intent(&f.state, op).await.unwrap());
    let second = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(second, MergeOutcome::AlreadyMerged { oid: oid.clone() });

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
    assert_eq!(
        refs, 1,
        "the second call must not journal a second ref update"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn conflicting_branches_return_conflicted_and_leave_the_ref() {
    let f = build_fixture(true).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    let before = main_tip(&f).await;
    let outcome = execute_intent(&f.state, op).await.unwrap();
    match outcome {
        MergeOutcome::Conflicted { paths } => {
            assert!(
                paths.iter().any(|p| p.contains("README")),
                "conflicting path reported: {paths:?}"
            );
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
    assert_eq!(main_tip(&f).await, before, "the ref must not move");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_cases_return_stale_and_leave_the_ref() {
    // Target moved: a new commit lands on main after the intent is created.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        advance_branch(&f, "main", "main2.txt", "main2\n").await;
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "target moved".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // Source moved: a new commit lands on the feature branch.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        advance_branch(&f, "feature", "feat2.txt", "feat2\n").await;
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "source moved".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // Not executing: the intent is still queued.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "intent is queued".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // The pull request was closed after the intent moved to executing.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        request(
            &f.app,
            "PATCH",
            &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
            Some(&f.token),
            None,
            json!({"state": "closed"}),
        )
        .await;
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "pull request not open".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // The repository was deleted after the intent was queued.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
            .bind(f.repo_id)
            .execute(&f.pool)
            .await
            .unwrap();
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "repository not active".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // The actor's grant was revoked.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        sqlx::query(
            "UPDATE grants SET revoked_at = now(), revoked_by = $1 \
             WHERE subject_id = $2 AND revoked_at IS NULL",
        )
        .bind(f.owner_id)
        .bind(f.owner_id)
        .execute(&f.pool)
        .await
        .unwrap();
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "actor no longer authorized".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // Branch protection changed: adding a rule bumps the policy generation.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        let (status, _) = request(
            &f.app,
            "POST",
            &format!(
                "/api/v1/organizations/{}/repositories/{}/protection",
                f.org_slug, f.repo_name
            ),
            Some(&f.token),
            None,
            json!({"pattern": "main"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "branch protection changed".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }

    // A new approval changes the checks/approvals digest the intent recorded.
    {
        let f = build_fixture(false).await;
        let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
        assert_eq!(status, StatusCode::CREATED);
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;
        let reviewer = add_member(&f.pool, f.org_id, "write").await;
        let reviewer_token = fixture_session(&f.pool, reviewer).await;
        approve(&f, f.number, "approved", &f.feature_head, &reviewer_token).await;
        let before = main_tip(&f).await;
        let outcome = execute_intent(&f.state, op).await.unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Stale {
                reason: "approvals or checks changed".into()
            }
        );
        assert_eq!(main_tip(&f).await, before);
        finish(f).await;
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_concurrent_calls_for_one_intent_merge_exactly_once() {
    let f = build_fixture(false).await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    let (a, b) = tokio::join!(execute_intent(&f.state, op), execute_intent(&f.state, op),);
    let outcomes = [a.unwrap(), b.unwrap()];
    let merged = outcomes
        .iter()
        .filter(|o| matches!(o, MergeOutcome::Merged { .. }))
        .count();
    let already = outcomes
        .iter()
        .filter(|o| matches!(o, MergeOutcome::AlreadyMerged { .. }))
        .count();
    assert_eq!((merged, already), (1, 1), "outcomes: {outcomes:?}");

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
    assert_eq!(refs, 1, "the target moved exactly once");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_different_intents_on_one_target_serialize() {
    let f = build_fixture(false).await;

    // A second feature branch and pull request, both targeting main.
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
        f.repo_path_str(),
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

    let (s1, intent1) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, intent2) = create_merge_intent_for(&f, number2, "merge", &feature2_head).await;
    assert_eq!(s2, StatusCode::CREATED);
    let op1: Uuid = intent1["operation_id"].as_str().unwrap().parse().unwrap();
    let op2: Uuid = intent2["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op1).await;
    set_intent_executing(&f.pool, op2).await;

    let (a, b) = tokio::join!(execute_intent(&f.state, op1), execute_intent(&f.state, op2),);
    let outcomes = [a.unwrap(), b.unwrap()];
    let merged = outcomes
        .iter()
        .filter(|o| matches!(o, MergeOutcome::Merged { .. }))
        .count();
    let stale = outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                MergeOutcome::Stale { reason } if reason == "target moved"
            )
        })
        .count();
    assert_eq!((merged, stale), (1, 1), "outcomes: {outcomes:?}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unknown_intent_returns_failed() {
    let f = build_fixture(false).await;
    let outcome = execute_intent(&f.state, Uuid::new_v4()).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Failed {
            reason: "merge intent not found".into()
        }
    );
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ff_only_on_diverged_branches_is_stale() {
    // The conflict fixture diverges main and feature, so a fast-forward-only
    // merge cannot advance main.
    let f = build_fixture(true).await;
    let (status, intent) = create_merge_intent(&f, "ff_only", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    let before = main_tip(&f).await;
    let outcome = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Stale {
            reason: "target is not an ancestor of the source".into()
        }
    );
    assert_eq!(main_tip(&f).await, before);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_required_check_flipping_after_the_intent_is_stale() {
    let f = build_fixture(false).await;

    // Require "ci" on main, and post a passing status at the head.
    request(
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
    let statuses = format!(
        "/api/v1/organizations/{}/repositories/{}/commits/{}/statuses",
        f.org_slug, f.repo_name, f.feature_head
    );
    let (status, _) = request(
        &f.app,
        "POST",
        &statuses,
        Some(&f.token),
        None,
        json!({"context": "ci", "state": "success"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    // The check flips to failure before the worker revalidates the intent.
    let (status, _) = request(
        &f.app,
        "POST",
        &statuses,
        Some(&f.token),
        None,
        json!({"context": "ci", "state": "failure"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let before = main_tip(&f).await;
    let outcome = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Stale {
            reason: "approvals or checks changed".into()
        }
    );
    assert_eq!(main_tip(&f).await, before);

    finish(f).await;
}
