//! Pull request, review and comment contract tests (PostgreSQL-backed).
//!
//! A repository is created through the API, then filled by a plain `git` CLI
//! in a temporary work tree pushing `main` and a `feature` branch directly to
//! the storage path — never through HTTP.

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
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
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
    repo_name: String,
    repo_id: Uuid,
    head: String,
    feature_head: String,
    work: PathBuf,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

/// One repository with a `main` (target) and `feature` (source) branch.
async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-pulls-{}", Uuid::new_v4()));
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

    let work = std::env::temp_dir().join(format!("git-pulls-work-{}", Uuid::new_v4()));
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

    // Direct storage pushes bypass GitHearth, so skip its ref-transaction hooks.
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

    Fixture {
        app,
        pool,
        storage_root,
        org_slug,
        org_id,
        token,
        repo_name,
        repo_id,
        head,
        feature_head,
        work,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

/// Creates a pull request `feature` -> `main` as the owner and returns the body.
async fn create_pull(f: &Fixture) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": "pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await
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

async fn comment(f: &Fixture, number: i64, token: &str, body: Value) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &format!("{}/{number}/comments", repo_href(&f.org_slug, &f.repo_name)),
        Some(token),
        None,
        body,
    )
    .await
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_read_and_list_with_shared_numbering() {
    let f = build_fixture().await;

    // An issue and a pull request share one number sequence.
    let (status, issue) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/issues",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        None,
        json!({"title": "first", "body": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(issue["number"], 1);

    let (status, body) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["number"], 2);
    assert_eq!(body["title"], "pull");
    assert_eq!(body["state"], "open");
    assert_eq!(body["source_branch"], "feature");
    assert_eq!(body["target_branch"], "main");
    assert_eq!(body["head_id"], f.feature_head);
    assert_eq!(body["base_id"], f.head);
    assert_eq!(body["approvals"], 0);
    assert_eq!(body["changes_requested"], 0);

    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/2", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["head_id"], f.feature_head);

    // List is newest first with a shared default open state filter.
    let (status, body) = request(
        &f.app,
        "GET",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 2);
    assert_eq!(body["next"], Value::Null);

    finish(f).await;
}

/// Creates a source branch and a pull request against `main`, returning the body.
async fn create_branch_pull(f: &Fixture, n: i32) -> Value {
    let tw = f.work.to_str().unwrap();
    let branch = format!("pr-{n}");
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "-b", &branch])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        &format!("commit {n}"),
    ]))
    .await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        &branch,
    ]))
    .await;
    let (status, body) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": format!("pull {n}"), "source_branch": branch, "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn list_paginates_and_filters_state() {
    let f = build_fixture().await;

    let repo_path = repo_href(&f.org_slug, &f.repo_name);
    // Number 1 from the base feature branch, then four more on fresh branches.
    create_pull(&f).await;
    for n in 2..=5 {
        create_branch_pull(&f, n).await;
    }

    // Newest first, two per page.
    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?limit=2"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["number"], 5);
    assert_eq!(items[1]["number"], 4);
    assert_eq!(body["next"], 4);

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?limit=2&after=4"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["number"], 3);
    assert_eq!(items[1]["number"], 2);
    assert_eq!(body["next"], 2);

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?limit=2&after=2"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 1);
    assert_eq!(body["next"], Value::Null);

    // Close number 3 and filter by state.
    let closed = request(
        &f.app,
        "PATCH",
        &format!("{repo_path}/3"),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(closed.0, StatusCode::OK);

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?state=closed"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 3);

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?state=all"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 5);

    // Merge number 4 and filter by merged.
    let merged = request(
        &f.app,
        "PATCH",
        &format!("{repo_path}/4"),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(merged.0, StatusCode::OK);
    sqlx::query(
        "UPDATE pull_requests SET state = 'merged' WHERE number = 4 \
         AND repository_id IN (SELECT id FROM repositories WHERE organization_id = $1)",
    )
    .bind(f.org_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?state=merged"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["number"], 4);

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?state=all"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 5);

    let (status, _) = request(
        &f.app,
        "GET",
        &format!("{repo_path}?state=bogus"),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_rejects_duplicate_pair_missing_branch_and_bad_names() {
    let f = build_fixture().await;

    let (status, first) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CREATED);

    // A duplicate open pair is a conflict.
    let (status, _) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Closing the first lets the pair be opened again.
    request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{}",
            repo_href(&f.org_slug, &f.repo_name),
            first["number"]
        ),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    let (status, _) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CREATED);

    // A missing branch is unprocessable.
    let (status, _) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": "x", "source_branch": "nope", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Same source and target, or a malformed branch name, is a bad request.
    for body in [
        json!({"title": "x", "source_branch": "main", "target_branch": "main"}),
        json!({"title": "x", "source_branch": "a..b", "target_branch": "main"}),
        json!({"title": "x", "source_branch": "feature", "target_branch": "bad name"}),
    ] {
        let (status, _) = request(
            &f.app,
            "POST",
            &repo_href(&f.org_slug, &f.repo_name),
            Some(&f.token),
            None,
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_create_replay_returns_same_body_once() {
    let f = build_fixture().await;
    let key = tag("idem");
    let body = json!({"title": "dup", "source_branch": "feature", "target_branch": "main"});
    let (status, first) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        Some(("Idempotency-Key", &key)),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status2, second) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        Some(("Idempotency-Key", &key)),
        body,
    )
    .await;
    assert_eq!(status2, StatusCode::CREATED);
    assert_eq!(first, second);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pull_requests WHERE repository_id = $1")
            .bind(f.repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn approval_counts_only_at_the_current_head() {
    let f = build_fixture().await;

    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let write = add_member(&f.pool, f.org_id, "write").await;
    let write_token = fixture_session(&f.pool, write).await;

    // A write member approves at the head.
    let (status, review) = approve(&f, number, "approved", &f.feature_head, &write_token).await;
    assert_eq!(status, StatusCode::CREATED, "{review}");
    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["approvals"], 1);
    assert_eq!(body["changes_requested"], 0);

    // The author approving their own pull request is forbidden.
    let (status, _) = approve(&f, number, "approved", &f.feature_head, &f.token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A comment-only review does not count.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
        Some(&write_token),
        None,
        json!({"verdict": "commented", "commit_id": f.feature_head, "body": "noted"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["approvals"], 1);

    // A review naming a stale commit is a conflict.
    let (status, _) = approve(&f, number, "approved", &f.head, &write_token).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Pushing a new commit to feature makes the earlier approval stale.
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "checkout",
        "-q",
        "feature",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "more",
    ]))
    .await;
    let new_head = git_output(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "rev-parse",
        "HEAD",
    ]))
    .await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["head_id"].as_str().unwrap(), new_head);
    assert_eq!(body["approvals"], 0);
    assert_eq!(body["changes_requested"], 0);

    // The old review is still listed with its commit id.
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    let any_approved = items
        .iter()
        .any(|r| r["verdict"] == "approved" && r["commit_id"] == f.feature_head);
    assert!(any_approved, "stale approval should still be listed");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn latest_verdict_per_reviewer_wins() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let write = add_member(&f.pool, f.org_id, "write").await;
    let write_token = fixture_session(&f.pool, write).await;

    approve(&f, number, "approved", &f.feature_head, &write_token).await;
    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["approvals"], 1);

    // The same reviewer changes their mind: approval is replaced by a change request.
    let (status, _) = approve(
        &f,
        number,
        "changes_requested",
        &f.feature_head,
        &write_token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(body["approvals"], 0);
    assert_eq!(body["changes_requested"], 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn review_permissions_read_forbidden_comment_allowed_closed_conflict() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let read = add_member(&f.pool, f.org_id, "read").await;
    let read_token = fixture_session(&f.pool, read).await;

    // A read member cannot approve.
    let (status, _) = approve(&f, number, "approved", &f.feature_head, &read_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // But may leave a comment review.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
        Some(&read_token),
        None,
        json!({"verdict": "commented", "commit_id": f.feature_head, "body": "looks fine"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Once closed, no one may review.
    request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    let write = add_member(&f.pool, f.org_id, "write").await;
    let write_token = fixture_session(&f.pool, write).await;
    let (status, _) = approve(&f, number, "approved", &f.feature_head, &write_token).await;
    assert_eq!(status, StatusCode::CONFLICT);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn comments_general_anchored_reply_and_pagination() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    // General comment.
    let (status, c0) = comment(&f, number, &f.token, json!({"body": "general"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(c0["seq"], 1);

    // Anchored at the head.
    let (status, c1) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "line comment", "commit_id": f.feature_head, "path": "feat.txt", "line": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(c1["seq"], 2);

    // Anchored at an old commit is a conflict.
    let (status, _) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "stale", "commit_id": f.head, "path": "README.md", "line": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A reply to the general comment.
    let (status, reply) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "reply", "in_reply_to": c0["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(reply["in_reply_to"], c0["id"]);
    assert_eq!(reply["seq"], 3);

    // Replying to a comment on another pull request is a bad request.
    // Create a second pull request and try to reply to this one's comment.
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "checkout",
        "-q",
        "-b",
        "other",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "other",
    ]))
    .await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "other",
    ]))
    .await;
    let (_, other) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": "other", "source_branch": "other", "target_branch": "main"}),
    )
    .await;
    let other_number = other["number"].as_i64().unwrap();
    let (status, _) = comment(
        &f,
        other_number,
        &f.token,
        json!({"body": "cross", "in_reply_to": c0["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Pagination oldest first.
    let (status, body) = request(
        &f.app,
        "GET",
        &format!(
            "{}/{number}/comments?limit=2",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["seq"], 1);
    assert_eq!(items[1]["seq"], 2);
    let next = body["next"].as_i64().unwrap();

    let (_, body) = request(
        &f.app,
        "GET",
        &format!(
            "{}/{number}/comments?limit=2&after={next}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["seq"], 3);
    assert_eq!(body["next"], Value::Null);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn anchored_comments_validate_path_and_line_at_the_commit() {
    let f = build_fixture().await;

    // Extend feature with a directory and a three-line file.
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "checkout",
        "-q",
        "feature",
    ]))
    .await;
    std::fs::create_dir_all(f.work.join("dir")).unwrap();
    std::fs::write(f.work.join("dir/inner.txt"), "one\ntwo\nthree\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "add",
        "dir/inner.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "nested",
    ]))
    .await;
    let new_head = git_output(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "rev-parse",
        "HEAD",
    ]))
    .await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    // The last line of the file is a valid anchor.
    let (status, _) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "last", "commit_id": new_head, "path": "dir/inner.txt", "line": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A line past the end is out of range.
    let (status, _) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "past", "commit_id": new_head, "path": "dir/inner.txt", "line": 4}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A missing path is refused.
    let (status, _) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "missing", "commit_id": new_head, "path": "nope.txt", "line": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A directory is not a line anchor.
    let (status, _) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "dir", "commit_id": new_head, "path": "dir", "line": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn close_reopen_and_reopen_conflict() {
    let f = build_fixture().await;

    let triage = add_member(&f.pool, f.org_id, "triage").await;
    let triage_token = fixture_session(&f.pool, triage).await;

    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    // A triage member can close...
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&triage_token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "closed");
    assert!(body["closed_at"].is_string());

    // ...and reopen.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&triage_token),
        None,
        json!({"state": "open"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state"], "open");
    assert_eq!(body["closed_at"], Value::Null);

    // While it is open, opening another pull request on the same pair conflicts.
    let (status, _) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Reopening while another open pull request has the same pair conflicts:
    // close the first, open a fresh one on the pair (which succeeds), then try
    // to reopen the first: conflict.
    request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&triage_token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    let (status, _) = create_pull(&f).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&triage_token),
        None,
        json!({"state": "open"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_user_404_everywhere() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let stranger = fixture_account(&f.pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&f.pool, stranger).await;

    for (method, path, body) in [
        (
            "GET",
            format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
            Value::Null,
        ),
        ("GET", repo_href(&f.org_slug, &f.repo_name), Value::Null),
        (
            "POST",
            repo_href(&f.org_slug, &f.repo_name),
            json!({"title": "x", "source_branch": "feature", "target_branch": "main"}),
        ),
        (
            "PATCH",
            format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
            json!({"title": "x"}),
        ),
        (
            "GET",
            format!("{}/{number}/comments", repo_href(&f.org_slug, &f.repo_name)),
            Value::Null,
        ),
        (
            "POST",
            format!("{}/{number}/comments", repo_href(&f.org_slug, &f.repo_name)),
            json!({"body": "x"}),
        ),
        (
            "GET",
            format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
            Value::Null,
        ),
        (
            "POST",
            format!("{}/{number}/reviews", repo_href(&f.org_slug, &f.repo_name)),
            json!({"verdict": "approved", "commit_id": f.feature_head}),
        ),
    ] {
        let (status, _) = request(&f.app, method, &path, Some(&stranger_token), None, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    finish(f).await;
}

/// Pushes a new empty commit onto `branch` directly into storage, returning
/// the new head.
async fn advance_branch(f: &Fixture, branch: &str) -> String {
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", branch])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "advance",
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
        branch,
    ]))
    .await;
    oid
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn inline_comments_carry_side_and_turn_outdated() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let (status, c) = comment(
        &f,
        number,
        &f.token,
        json!({
            "body": "```suggestion\nfeature!\n```",
            "commit_id": f.feature_head,
            "path": "feat.txt",
            "line": 1,
            "side": "new",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    assert_eq!(c["side"], "new");
    assert_eq!(c["outdated"], false);
    assert_eq!(c["resolved"], false);
    assert_eq!(c["suggestion"], "feature!\n");

    // A comment without an explicit side is null (treated as new).
    let (status, c2) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "plain", "commit_id": f.feature_head, "path": "feat.txt", "line": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(c2["side"], Value::Null);

    // Pushing a new commit makes the anchored comment outdated.
    advance_branch(&f, "feature").await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/{number}/comments", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items[0]["outdated"], true);
    assert_eq!(items[0]["suggestion"], "feature!\n");
    assert_eq!(items[1]["outdated"], true);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn threads_resolve_and_unresolve() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let (_, root) = comment(&f, number, &f.token, json!({"body": "root"})).await;
    let root_id = root["id"].as_str().unwrap();
    let (_, reply) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "reply", "in_reply_to": root_id}),
    )
    .await;
    let reply_id = reply["id"].as_str().unwrap();

    // Resolving the reply resolves the whole thread.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{number}/comments/{reply_id}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        json!({"resolved": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["resolved"], true);
    assert!(body["resolved_at"].is_string());

    // The list shows both the root and the reply resolved.
    let (_, list) = request(
        &f.app,
        "GET",
        &format!("{}/{number}/comments", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        Value::Null,
    )
    .await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["resolved"], true);
    assert_eq!(items[1]["resolved"], true);

    // Unresolving the root clears the thread.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{number}/comments/{root_id}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        json!({"resolved": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["resolved"], false);

    // A comment on another pull request is not found.
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "checkout",
        "-q",
        "-b",
        "other2",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "--allow-empty",
        "-m",
        "other2",
    ]))
    .await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        f.work.to_str().unwrap(),
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "other2",
    ]))
    .await;
    let (_, other) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&f.token),
        None,
        json!({"title": "other2", "source_branch": "other2", "target_branch": "main"}),
    )
    .await;
    let other_number = other["number"].as_i64().unwrap();
    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{other_number}/comments/{root_id}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        json!({"resolved": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resolve_requires_the_author_or_an_approver() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let (_, root) = comment(&f, number, &f.token, json!({"body": "root"})).await;
    let root_id = root["id"].as_str().unwrap();

    // A read-only member can read and comment, but cannot resolve a thread.
    let reader = add_member(&f.pool, f.org_id, "read").await;
    let reader_token = fixture_session(&f.pool, reader).await;
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{number}/comments/{root_id}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&reader_token),
        None,
        json!({"resolved": true}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The author can resolve the thread.
    let (status, body) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{number}/comments/{root_id}",
            repo_href(&f.org_slug, &f.repo_name)
        ),
        Some(&f.token),
        None,
        json!({"resolved": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["resolved"], true);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replies_attach_to_the_root_not_to_another_reply() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let (_, root) = comment(&f, number, &f.token, json!({"body": "root"})).await;
    let root_id = root["id"].as_str().unwrap();
    let (_, reply) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "reply", "in_reply_to": root_id}),
    )
    .await;
    let reply_id = reply["id"].as_str().unwrap();

    // Replying to a reply is rejected so the display nests one root with its
    // direct replies and a reply's resolution is its root's.
    let (status, body) = comment(
        &f,
        number,
        &f.token,
        json!({"body": "nested", "in_reply_to": reply_id}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resolve_on_a_closed_pull_request_is_refused() {
    let f = build_fixture().await;
    let (_, pull) = create_pull(&f).await;
    let number = pull["number"].as_i64().unwrap();

    let (_, root) = comment(&f, number, &f.token, json!({"body": "root"})).await;
    let root_id = root["id"].as_str().unwrap();

    // Close the pull request; resolving and unresolving must both be refused.
    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!("{}/{number}", repo_href(&f.org_slug, &f.repo_name)),
        Some(&f.token),
        None,
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for resolved in [true, false] {
        let (status, body) = request(
            &f.app,
            "PATCH",
            &format!(
                "{}/{number}/comments/{root_id}",
                repo_href(&f.org_slug, &f.repo_name)
            ),
            Some(&f.token),
            None,
            json!({"resolved": resolved}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "pull request is not open");
    }

    finish(f).await;
}
