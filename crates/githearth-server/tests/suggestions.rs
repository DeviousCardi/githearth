//! Applying a ```suggestion block as a commit on the head branch, through the
//! journaled compare-and-swap ref update (PostgreSQL-backed).

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
    feature_head: String,
    work: PathBuf,
    number: i64,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-suggest-{}", Uuid::new_v4()));
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
    let app = router(state);

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);
    // Seed branches directly into storage, bypassing the receive hooks.
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-suggest-work-{}", Uuid::new_v4()));
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
    std::fs::write(work.join("feat.txt"), "before\n").unwrap();
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
        json!({"title": "pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");

    Fixture {
        app,
        pool,
        storage_root,
        org_slug,
        org_id,
        token,
        repo_name,
        repo_id,
        feature_head,
        work,
        number: pull["number"].as_i64().unwrap(),
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn current_head(f: &Fixture) -> String {
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_output(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "rev-parse",
        "feature",
    ]))
    .await
}

async fn add_suggestion_comment(f: &Fixture, token: &str, suggestion: &str) -> (StatusCode, Value) {
    let head = current_head(f).await;
    request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(token),
        json!({
            "body": format!("```suggestion\n{suggestion}\n```"),
            "commit_id": head,
            "path": "feat.txt",
            "line": 1,
            "side": "new",
        }),
    )
    .await
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn applying_a_suggestion_moves_the_head_and_records_the_push() {
    let f = build_fixture().await;

    let (status, comment) = add_suggestion_comment(&f, &f.token, "after").await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");
    let comment_id = comment["id"].as_str().unwrap().to_owned();

    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{comment_id}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_head = body["commit_id"].as_str().unwrap().to_owned();
    assert_ne!(new_head, f.feature_head);

    // The head branch moved to the new commit with the file updated.
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tip = git_output(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "rev-parse",
        "feature",
    ]))
    .await;
    assert_eq!(tip, new_head);
    let content = git_output(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "cat-file",
        "blob",
        &format!("{new_head}:feat.txt"),
    ]))
    .await;
    assert_eq!(content, "after");

    // The comment is now outdated and the push is recorded with its audit event.
    let (status, list) = request(
        &f.app,
        "GET",
        &format!(
            "{}/{}/comments",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"][0]["outdated"], true);

    let pushes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id = $1")
            .bind(f.repo_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(pushes, 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE kind = 'pull_request.suggestion_applied' AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 1);

    // A suggestion commit fires the same push webhook a normal push does.
    let push_audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE kind = 'repository.pushed' AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(push_audit >= 1);
    let push_outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events \
         WHERE kind = 'repository.pushed' AND organization_id = $1",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(push_outbox >= 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn author_and_writer_can_apply_but_a_reader_cannot() {
    let f = build_fixture().await;

    // The reader (read role) is not the author and has no write grant.
    let reader = add_member(&f.pool, f.org_id, "read").await;
    let reader_token = fixture_session(&f.pool, reader).await;
    let writer = add_member(&f.pool, f.org_id, "write").await;
    let writer_token = fixture_session(&f.pool, writer).await;

    // The author applies their own suggestion.
    let (status, comment) = add_suggestion_comment(&f, &f.token, "author\n").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number,
            comment["id"].as_str().unwrap()
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A read-only member cannot apply.
    let (status, comment) = add_suggestion_comment(&f, &f.token, "nope\n").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number,
            comment["id"].as_str().unwrap()
        ),
        Some(&reader_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A writer (non-author) can apply.
    let (status, comment) = add_suggestion_comment(&f, &f.token, "writer\n").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number,
            comment["id"].as_str().unwrap()
        ),
        Some(&writer_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn apply_rejects_non_suggestions_and_old_side_comments() {
    let f = build_fixture().await;

    // A general comment has no suggestion to apply.
    let (status, general) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.token),
        json!({"body": "no suggestion here"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number,
            general["id"].as_str().unwrap()
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A suggestion on the base (old) side cannot apply.
    let (status, old_side) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.token),
        json!({
            "body": "```suggestion\nx\n```",
            "commit_id": f.feature_head,
            "path": "feat.txt",
            "line": 1,
            "side": "old",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number,
            old_side["id"].as_str().unwrap()
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn applying_an_outdated_suggestion_is_refused_with_head_moved() {
    let f = build_fixture().await;

    let (status, comment) = add_suggestion_comment(&f, &f.token, "after\n").await;
    assert_eq!(status, StatusCode::CREATED);
    let comment_id = comment["id"].as_str().unwrap().to_owned();

    // Move the head without deleting the target line. The line still exists,
    // but the comment is now outdated and must not silently rewrite it.
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("feat.txt"), "before\nchanged\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "move head",
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
        "feature",
    ]))
    .await;

    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{comment_id}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn writer_cannot_apply_to_a_protected_head_branch() {
    let f = build_fixture().await;

    // A write-role member (who could otherwise push) authors their own pull
    // request, so the refusal below isolates the protected-branch check.
    let author = add_member(&f.pool, f.org_id, "write").await;
    let author_token = fixture_session(&f.pool, author).await;

    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&[
        "-C",
        tw,
        "checkout",
        "-q",
        "-b",
        "writer-branch",
    ]))
    .await;
    std::fs::write(f.work.join("writer.txt"), "writer\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "writer.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "writer",
    ]))
    .await;
    let writer_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "writer-branch",
    ]))
    .await;

    let (status, pull) = request(
        &f.app,
        "POST",
        &repo_href(&f.org_slug, &f.repo_name),
        Some(&author_token),
        json!({"title": "writer pull", "source_branch": "writer-branch", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();

    // Protect the head branch so a push would be refused.
    let (status, rule) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/protection",
            f.org_slug, f.repo_name
        ),
        Some(&f.token),
        json!({"pattern": "writer-branch", "require_pull_request": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{rule}");

    // The author adds a suggestion and tries to apply it.
    let (status, comment) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments",
            repo_href(&f.org_slug, &f.repo_name),
            number
        ),
        Some(&author_token),
        json!({
            "body": "```suggestion\nchanged\n```",
            "commit_id": writer_head,
            "path": "writer.txt",
            "line": 1,
            "side": "new",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{comment}");

    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/comments/{}/apply",
            repo_href(&f.org_slug, &f.repo_name),
            number,
            comment["id"].as_str().unwrap()
        ),
        Some(&author_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    finish(f).await;
}
