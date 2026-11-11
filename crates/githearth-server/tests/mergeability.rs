//! Mergeability (M3 conflicts) contract tests (PostgreSQL-backed).
//!
//! A repository is created through the API and filled by a plain `git` CLI in
//! a temporary work tree pushing `main` and `feature` directly to the storage
//! path — never through HTTP. These tests drive the mergeability cache and the
//! conflict refusal: a conflicting pull request cannot merge by any method, a
//! push to the head that resolves the conflict makes it mergeable again, and a
//! target move that introduces a conflict flips the status (recomputing the
//! cache for the new target rather than serving a stale answer).

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
    token: String,
    repo_name: String,
    repo_id: Uuid,
    work: PathBuf,
    head: String,
    feature_head: String,
    number: i64,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

/// A repository whose `main` (target) and `feature` (source) branch both
/// diverge from a shared root, with an open pull request `feature -> main`.
///
/// The root commit writes `notes&tips.md` as "hello\n"; `feature` changes it to
/// "feature\n" while `main` stays at the root. The pull request therefore
/// merges cleanly until `main` independently edits the same file, which flips
/// the mergeability to conflicted. The work tree is left checked out on
/// `feature`.
async fn build_fixture() -> Fixture {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-mergeability-{}", Uuid::new_v4()));
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
    let (_, org_slug) = fixture_organization(&pool, owner).await;

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

    let work = std::env::temp_dir().join(format!("git-mergeability-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("notes&tips.md"), "hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "notes&tips.md"])).await;
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
    std::fs::write(work.join("notes&tips.md"), "feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "notes&tips.md"])).await;
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
    let number = pull["number"].as_i64().unwrap();

    Fixture {
        app,
        pool,
        storage_root,
        org_slug,
        token,
        repo_name,
        repo_id,
        work,
        head,
        feature_head,
        number,
    }
}

/// Commits `content` to `notes&tips.md` on `main` and pushes it, returning the new
/// target tip. This is what makes the pull request conflict.
async fn advance_main(f: &Fixture, content: &str) -> String {
    let tw = f.work.to_str().unwrap();
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(f.work.join("notes&tips.md"), content).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "notes&tips.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "main edit",
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
    git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await
}

/// Resolves the conflict by rewriting the head onto the current target tip: the
/// `feature` branch is reset to `main` and a single commit applying the resolved
/// file content is pushed. The head is now a descendant of the target, so every
/// merge method (including rebase, which replays the lone head commit) merges
/// cleanly again.
async fn reset_feature_onto_main(f: &Fixture, content: &str) -> String {
    let tw = f.work.to_str().unwrap();
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    git_ok(git_cmd(&["-C", tw, "reset", "--hard", "main"])).await;
    std::fs::write(f.work.join("notes&tips.md"), content).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "notes&tips.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "resolve",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--force",
        "--",
        repo_path.to_str().unwrap(),
        "feature",
    ]))
    .await;
    git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

/// Fetches the browser pull request page with the owner's session cookie.
async fn pull_page(f: &Fixture) -> String {
    let path = format!(
        "/organizations/{}/repositories/{}/pulls/{}",
        f.org_slug, f.repo_name, f.number
    );
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("cookie", format!("githearth_session={}", f.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn pull_detail(f: &Fixture) -> Value {
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/{}", repo_href(&f.org_slug, &f.repo_name), f.number),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn mergeability_of<'a>(detail: &'a Value, method: &str) -> &'a Value {
    detail["mergeability"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["method"] == method)
        .unwrap_or_else(|| panic!("no mergeability entry for {method}: {detail}"))
}

async fn create_merge_intent(f: &Fixture, method: &str, head: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(&f.token),
        json!({"method": method, "expected_head_oid": head}),
    )
    .await
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn conflicting_pull_cannot_merge_by_any_method_and_lists_paths() {
    let f = build_fixture().await;
    let main_head = advance_main(&f, "main\n").await;

    // Every enabled method reports the conflict with the escaped path.
    let detail = pull_detail(&f).await;
    for method in ["merge", "squash", "rebase"] {
        let check = mergeability_of(&detail, method);
        assert_eq!(check["status"], "conflicts", "{method}: {detail}");
        assert_eq!(check["conflicts"][0], "notes&tips.md", "{method}: {detail}");
    }

    // Each merge attempt is refused cleanly with 409 and never moves the ref.
    for method in ["merge", "squash", "rebase"] {
        let (status, body) = create_merge_intent(&f, method, &f.feature_head).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method}: {body}");
        assert_eq!(body["error"], "merge conflicts", "{method}: {body}");
    }

    // The refusal is recorded on a conflicted intent with the reason.
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT state, failure_reason FROM merge_intents \
         WHERE pull_request_id = (SELECT id FROM pull_requests WHERE repository_id = $1 AND number = $2) \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(f.repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "conflicted");
    assert!(row.1.unwrap().contains("notes&tips.md"));

    // No queued intent remains and the target tip is untouched.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_intents WHERE state <> 'conflicted' AND repository_id = $1",
    )
    .bind(f.repo_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(queued, 0);
    let detail = pull_detail(&f).await;
    assert_eq!(detail["base_id"], main_head.as_str());

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn conflict_section_lists_escaped_paths_and_disables_the_merge_button() {
    let f = build_fixture().await;
    advance_main(&f, "main\n").await;

    let page = pull_page(&f).await;
    assert!(
        page.contains("This branch has conflicts that must be resolved"),
        "{page}"
    );
    // The conflicting path is HTML-escaped.
    assert!(page.contains("notes&amp;tips.md"), "{page}");
    assert!(!page.contains("notes&tips.md"), "{page}");
    // Local resolution instructions for the PR's branches.
    assert!(page.contains("git fetch origin"), "{page}");
    assert!(page.contains("git checkout feature"), "{page}");
    assert!(page.contains("git merge origin/main"), "{page}");
    // The merge form is replaced by a disabled button carrying the reason.
    assert!(
        page.contains("<button type=\"submit\" disabled>Merge pull request</button>"),
        "{page}"
    );
    assert!(
        page.contains("This branch has conflicts that must be resolved."),
        "{page}"
    );
    assert!(!page.contains("<select id=\"method\""), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resolving_the_conflict_by_pushing_to_the_head_makes_it_mergeable() {
    let f = build_fixture().await;
    advance_main(&f, "main\n").await;
    assert_eq!(
        mergeability_of(&pull_detail(&f).await, "merge")["status"],
        "conflicts"
    );

    // Resolve by rewriting the head onto the target tip and pushing it.
    let resolved_head = reset_feature_onto_main(&f, "resolved\n").await;
    let detail = pull_detail(&f).await;
    assert_eq!(detail["head_id"], resolved_head.as_str());
    assert_eq!(mergeability_of(&detail, "merge")["status"], "clean");
    assert_eq!(mergeability_of(&detail, "squash")["status"], "clean");
    assert_eq!(mergeability_of(&detail, "rebase")["status"], "clean");

    // The merge now queues an intent instead of being refused.
    let (status, intent) = create_merge_intent(&f, "merge", &resolved_head).await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    assert_eq!(intent["state"], "queued");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_target_move_that_introduces_a_conflict_flips_the_status_and_recomputes_the_cache() {
    let f = build_fixture().await;

    // Initially clean: the head's edit is the only change since the root.
    let detail = pull_detail(&f).await;
    for method in ["merge", "squash", "rebase"] {
        assert_eq!(
            mergeability_of(&detail, method)["status"],
            "clean",
            "{detail}"
        );
    }
    let cached_clean: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_checks WHERE repository_id = $1 AND head_oid = $2 AND target_oid = $3 AND status = 'clean'",
    )
    .bind(f.repo_id)
    .bind(&f.feature_head)
    .bind(&f.head)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(cached_clean, 3, "clean result cached for every method");

    // A target move editing the same file flips the status: the new target oid
    // is a cache miss, so the answer recomputes instead of serving stale.
    let main_head = advance_main(&f, "main\n").await;
    let detail = pull_detail(&f).await;
    assert_eq!(detail["base_id"], main_head.as_str());
    for method in ["merge", "squash", "rebase"] {
        assert_eq!(
            mergeability_of(&detail, method)["status"],
            "conflicts",
            "{method}: {detail}"
        );
    }
    let conflict_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_checks WHERE repository_id = $1 AND head_oid = $2 AND target_oid = $3 AND status = 'conflicts'",
    )
    .bind(f.repo_id)
    .bind(&f.feature_head)
    .bind(&main_head)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        conflict_rows, 3,
        "conflict result cached for the new target"
    );
    // The stale clean entry for the old target is untouched but never served.
    let stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_checks WHERE repository_id = $1 AND head_oid = $2 AND target_oid = $3 AND status = 'clean'",
    )
    .bind(f.repo_id)
    .bind(&f.feature_head)
    .bind(&f.head)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        stale, 3,
        "the old (head, target) row is still keyed correctly"
    );

    finish(f).await;
}
