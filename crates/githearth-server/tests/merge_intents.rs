//! Merge intent contract tests (PostgreSQL-backed).
//!
//! A repository is created through the API, then filled by a plain `git` CLI
//! in a temporary work tree pushing `main` and a `feature` branch directly to
//! the storage path — never through HTTP. Merge intents are Person A's side of
//! ADR 0004: they validate and record, but never move a ref (Person B does).

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

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

/// One repository with a `main` (target) and `feature` (source) branch, plus an
/// open pull request `feature -> main`.
async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-merge-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool, &tag("owner")).await;
    let token = fixture_session(&pool, owner).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);

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
    let head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

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

async fn approve(
    f: &Fixture,
    number: i64,
    verdict: &str,
    commit: &str,
    token: &str,
) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
        Some(token),
        None,
        json!({"verdict": verdict, "commit_id": commit}),
    )
    .await
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

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_refuses_closed_head_moved_no_permission_few_approvals_checks_and_duplicate() {
    let f = build_fixture().await;
    let merge = merge_href(&f.org_slug, &f.repo_name, f.number);

    // Head moved: expected oid is not the current head.
    let (status, _) = request(
        &f.app,
        "POST",
        &merge,
        Some(&f.token),
        None,
        json!({"method": "merge", "expected_head_oid": f.head}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // No permission: a read member cannot merge.
    let reader = add_member(&f.pool, f.org_id, "read").await;
    let reader_token = fixture_session(&f.pool, reader).await;
    let (status, _) = request(
        &f.app,
        "POST",
        &merge,
        Some(&reader_token),
        None,
        json!({"method": "merge", "expected_head_oid": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Too few approvals.
    request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"pattern": "main", "required_approvals": 1}),
    )
    .await;
    let (status, _) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // require_checks refuses in M3 (no checks recorded).
    let (_, prot) = request(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let rule_id = prot["items"][0]["id"].as_str().unwrap().to_string();
    request(
        &f.app,
        "PATCH",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection/{rule_id}",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"require_checks": true, "required_approvals": 0}),
    )
    .await;
    let (status, _) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Remove the protection so the owner may create an intent.
    request(
        &f.app,
        "DELETE",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection/{rule_id}",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;

    let (status, first) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(first["state"], "queued");
    assert_eq!(first["source_oid"], f.feature_head.as_str());
    assert_eq!(first["target_ref"], "main");
    assert_eq!(first["source_ref"], "feature");
    assert_eq!(first["expected_target_oid"], f.head.as_str());

    // Existing open intent -> 409.
    let (status, _) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CONFLICT);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_refuses_a_closed_pull_request() {
    let f = build_fixture().await;
    let pull_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM pull_requests WHERE number = $1 \
         AND repository_id IN (SELECT id FROM repositories WHERE organization_id = $2)",
    )
    .bind(f.number as i32)
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    // Close the pull request first.
    request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    let (status, _) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // No intent may be queued on a closed pull request.
    let intents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intents, 0);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_reports_not_open_before_branch_lookup_on_a_closed_pull_with_no_source_branch() {
    let f = build_fixture().await;
    let pull_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM pull_requests WHERE number = $1 \
         AND repository_id IN (SELECT id FROM repositories WHERE organization_id = $2)",
    )
    .bind(f.number as i32)
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    // Delete the source branch, then close the pull request.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        ":feature",
    ]))
    .await;
    request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;

    // The early open check must win over the missing-branch error, so the
    // response is 409 "pull request is not open", not 422 "branch not found".
    let (status, body) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "pull request is not open");

    let intents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intents, 0);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_replay_returns_same_intent_once() {
    let f = build_fixture().await;
    let key = tag("idem");
    let body = json!({"method": "squash", "expected_head_oid": f.feature_head});
    let merge = merge_href(&f.org_slug, &f.repo_name, f.number);

    let (status, first) = request(
        &f.app,
        "POST",
        &merge,
        Some(&f.token),
        Some(("Idempotency-Key", &key)),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status2, second) = request(
        &f.app,
        "POST",
        &merge,
        Some(&f.token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(first, second);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE repository_id = $1")
            .bind(f.repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cancel_queued_and_refuse_executing() {
    let f = build_fixture().await;
    let merge = merge_href(&f.org_slug, &f.repo_name, f.number);

    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let operation_id = intent["operation_id"].as_str().unwrap().to_string();

    // Cancel a queued intent.
    let (status, body) = request(&f.app, "DELETE", &merge, Some(&f.token), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "cancelled");
    assert_eq!(body["operation_id"].as_str().unwrap(), operation_id);

    // A fresh intent, set directly to executing, refuses cancellation.
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let op2 = intent["operation_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(Uuid::parse_str(&op2).unwrap())
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, _) = request(&f.app, "DELETE", &merge, Some(&f.token), None, Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn record_merge_result_is_idempotent() {
    let f = build_fixture().await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let operation_id: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    let pull_id: Uuid = intent["pull_request_id"].as_str().unwrap().parse().unwrap();

    let old_oid = f.head.clone();
    let new_oid = "b".repeat(40);
    let ref_name = "refs/heads/main";

    let mut conn = f.pool.acquire().await.unwrap();
    githearth_server::merge_intents::record_merge_result(
        &mut conn,
        f.repo_id,
        42,
        operation_id,
        f.owner_id,
        &old_oid,
        &new_oid,
        ref_name,
    )
    .await
    .unwrap();
    drop(conn);

    // The intent and pull request are merged.
    let state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(operation_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(state, "merged");
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(operation_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(result_oid, new_oid);
    let pull_state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
        .bind(pull_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(pull_state, "merged");

    // The ref update is recorded.
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = 42",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1);

    // Calling again changes nothing.
    let mut conn = f.pool.acquire().await.unwrap();
    githearth_server::merge_intents::record_merge_result(
        &mut conn,
        f.repo_id,
        42,
        operation_id,
        f.owner_id,
        &old_oid,
        &new_oid,
        ref_name,
    )
    .await
    .unwrap();
    drop(conn);
    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = 42",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn close_during_execution_still_ends_merged_and_merged_is_terminal() {
    let f = build_fixture().await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let operation_id: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    let pull_id: Uuid = intent["pull_request_id"].as_str().unwrap().parse().unwrap();
    let href = format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number);

    // The intent is executing, so closing the pull request does not cancel it.
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, _) = request(
        &f.app,
        "PATCH",
        &href,
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The ref moved anyway, so recording it marks the closed pull request merged.
    let mut conn = f.pool.acquire().await.unwrap();
    githearth_server::merge_intents::record_merge_result(
        &mut conn,
        f.repo_id,
        43,
        operation_id,
        f.owner_id,
        &f.head,
        &"c".repeat(40),
        "refs/heads/main",
    )
    .await
    .unwrap();
    drop(conn);
    let pull_state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
        .bind(pull_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(pull_state, "merged");

    // A merged pull request can be neither reopened nor closed.
    for target in ["open", "closed"] {
        let (status, body) = request(
            &f.app,
            "PATCH",
            &href,
            Some(&f.token),
            None,
            json!({"state": target}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{target}: {body}");
        assert_eq!(body["error"], "pull request is merged");
    }
    // Its title stays editable.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &href,
        Some(&f.token),
        None,
        json!({"title": "renamed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "merged");
    assert_eq!(body["title"], "renamed");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn closing_a_pull_request_cancels_its_queued_intent() {
    let f = build_fixture().await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let operation_id = intent["operation_id"].as_str().unwrap().to_string();

    request(
        &f.app,
        "PATCH",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;

    let state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(Uuid::parse_str(&operation_id).unwrap())
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(state, "cancelled");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn digest_changes_when_a_change_request_arrives_at_the_same_head() {
    let f = build_fixture().await;
    let merge = merge_href(&f.org_slug, &f.repo_name, f.number);

    // A reviewer approves at the head.
    let reviewer = add_member(&f.pool, f.org_id, "write").await;
    let reviewer_token = fixture_session(&f.pool, reviewer).await;
    let (status, _) = approve(&f, f.number, "approved", &f.feature_head, &reviewer_token).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, first) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let digest1 = first["checks_approvals_digest"]
        .as_str()
        .unwrap()
        .to_string();

    // Cancel, then the same reviewer requests changes at the same head.
    request(&f.app, "DELETE", &merge, Some(&f.token), None, Value::Null).await;
    let (status, _) = approve(
        &f,
        f.number,
        "changes_requested",
        &f.feature_head,
        &reviewer_token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, second) = create_merge_intent(&f, "merge", &f.feature_head).await;
    assert_eq!(status, StatusCode::CREATED);
    let digest2 = second["checks_approvals_digest"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ne!(
        digest1, digest2,
        "a new change request must change the digest"
    );

    finish(f).await;
}
