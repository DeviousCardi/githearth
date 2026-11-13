//! Draft pull request contract tests (PostgreSQL-backed).
//!
//! A draft pull request is open but not ready for review: it refuses merges,
//! auto-merge and review requests, and it does not notify code owners until it
//! is marked ready. These tests cover the API and the server-rendered UI,
//! including the author-or-triage authorization denial paths (ADR 0009).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_git::journal::{Kind, Record, Update};
use githearth_server::merge::{MergeOutcome, execute_intent};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, pushes, router};
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

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
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

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    api_with_key(app, method, path, token, body, None).await
}

async fn api_with_key(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"));
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    let (status, _, text) = send(app, builder.body(Body::from(body.to_string())).unwrap()).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, _, body) = send(app, builder.body(Body::empty()).unwrap()).await;
    (status, body)
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
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

async fn post_form(
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
    send(
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

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    work: PathBuf,
    base: String,
    api_base: String,
    token: String,
    cookie: String,
    org_id: Uuid,
    repo_id: Uuid,
    feature_head: String,
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

async fn fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-drafts-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 8,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let author_id = fixture_account(&pool, &tag("author")).await;
    let token = fixture_session(&pool, author_id).await;

    let org_id = Uuid::new_v4();
    let org = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(org_id)
        .bind(&org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(author_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(author_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let app_state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(app_state.clone());

    let repo_name = tag("repo");
    let (status, body) = api(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org}/repositories"),
        &token,
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);

    let work = std::env::temp_dir().join(format!("git-drafts-work-{}", Uuid::new_v4()));
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

    Fixture {
        app,
        state: app_state,
        pool,
        storage_root,
        work,
        base: format!("/organizations/{org}/repositories/{repo_name}"),
        api_base: format!("/api/v1/organizations/{org}/repositories/{repo_name}"),
        cookie: format!("githearth_session={token}"),
        token,
        org_id,
        repo_id,
        feature_head,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn add_member(pool: &PgPool, org_id: Uuid, role: &str) -> (Uuid, String, String) {
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
    let token = fixture_session(pool, account).await;
    (account, username, token)
}

fn pulls_href(api_base: &str) -> String {
    format!("{api_base}/pulls")
}

async fn outbox_count(pool: &PgPool, org_id: Uuid, kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE organization_id = $1 AND kind = $2",
    )
    .bind(org_id)
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn codeowners_jobs(pool: &PgPool, pull_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'codeowners.request_reviews' \
         AND payload->>'pull_request_id' = $1",
    )
    .bind(pull_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_intent_executing(pool: &PgPool, operation_id: Uuid) {
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(operation_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn main_tip(f: &Fixture) -> String {
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    git_output(git_cmd(&[
        "--git-dir",
        repo_path.to_str().unwrap(),
        "rev-parse",
        "main",
    ]))
    .await
}

/// Runs the shared job queue long enough to drain the fixture's jobs, then stops
/// it. Registers the same handlers the server does for the paths under test.
async fn run_jobs(state: &AppState) {
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    githearth_server::codeowners::register(&mut registry);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_as_draft_reports_draft_and_skips_code_owners() {
    let f = fixture().await;

    let (status, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    assert_eq!(pull["draft"], true);
    assert_eq!(pull["state"], "open");
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();

    // A draft create does not request code-owner reviews.
    assert_eq!(codeowners_jobs(&f.pool, pull_id).await, 0);

    // The pull request is reported as a draft on read.
    let (status, read) = api(
        &f.app,
        "GET",
        &format!("{}/{}", pulls_href(&f.api_base), pull["number"]),
        &f.token,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["draft"], true);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn draft_refuses_merge_auto_merge_and_review_requests() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();

    let (_writer, writer_username, _token) = add_member(&f.pool, f.org_id, "write").await;

    // Merge is refused.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/merge", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"method": "merge", "expected_head_oid": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pull request is a draft");

    // Auto-merge is refused.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!(
            "{pulls}/{number}/auto_merge",
            pulls = pulls_href(&f.api_base)
        ),
        &f.token,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pull request is a draft");

    // Review requests are refused.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!(
            "{pulls}/{number}/requested_reviewers",
            pulls = pulls_href(&f.api_base)
        ),
        &f.token,
        json!({"reviewer": writer_username}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pull request is a draft");

    // None of those refusals recorded a mutation event.
    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.merge_intent_created").await,
        0
    );
    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.auto_merge_enabled").await,
        0
    );
    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.review_requested").await,
        0
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn mark_ready_emits_event_and_requests_code_owners() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();

    let (status, ready) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"draft": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["draft"], false);

    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.ready_for_review").await,
        1
    );
    // Marking ready requests code-owner reviews (the job is enqueued; the
    // fixture has no CODEOWNERS, so no individual request is created).
    assert_eq!(codeowners_jobs(&f.pool, pull_id).await, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn convert_to_draft_cancels_queued_intent_and_auto_merge() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "ready", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();

    // Enable auto-merge; because the pull request is mergeable, the trigger
    // also raises a queued merge intent.
    let (status, _) = api(
        &f.app,
        "POST",
        &format!(
            "{pulls}/{number}/auto_merge",
            pulls = pulls_href(&f.api_base)
        ),
        &f.token,
        json!({"method": "merge"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let auto_merges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auto_merges WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(auto_merges, 1);

    // The mergeable pull request raised a live intent before the conversion, so
    // the cancellation below is not vacuous.
    let live_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_intents WHERE pull_request_id = $1 \
         AND state IN ('queued', 'validating')",
    )
    .bind(pull_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        live_before, 1,
        "auto-merge raised a live intent before the conversion"
    );

    let (status, draft) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"draft": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["draft"], true);

    // The auto-merge row is gone and no live merge intent remains.
    let auto_merges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auto_merges WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(auto_merges, 0);
    let live_intents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_intents WHERE pull_request_id = $1 \
         AND state IN ('queued', 'validating')",
    )
    .bind(pull_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(live_intents, 0);

    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.converted_to_draft").await,
        1
    );
    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.auto_merge_cancelled").await,
        1
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn draft_refuses_merge_queue_entry() {
    let f = fixture().await;

    // Require the merge queue on main, so a merge would normally be enqueued.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{}/protection", f.api_base),
        &f.token,
        json!({"pattern": "main", "require_merge_queue": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/merge", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"method": "merge", "expected_head_oid": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "pull request is a draft");

    // The draft was refused before the queue, so no intent was enqueued.
    let intents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_intents WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intents, 0, "a draft is never enqueued");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn convert_to_draft_serializes_with_the_merge_executor() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "ready", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();

    // Raise a merge intent and drive it to `executing`, as A's worker does.
    let (status, intent) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/merge", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"method": "merge", "expected_head_oid": f.feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    // Hold the repository lock exactly as `execute_intent` does for its whole
    // operation, then start a convert-to-draft. It must wait for the lock
    // instead of committing while the executor is between its draft re-read
    // and the target ref compare-and-swap.
    let guard = f
        .state
        .storage
        .repository_lock(f.repo_id)
        .lock_owned()
        .await;
    let app = f.app.clone();
    let token = f.token.clone();
    let href = format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base));
    let convert =
        tokio::spawn(async move { api(&app, "POST", &href, &token, json!({"draft": true})).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !convert.is_finished(),
        "convert-to-draft must wait for the repository lock"
    );
    drop(guard);
    let (status, draft) = convert.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["draft"], true);

    // The executor now re-reads the draft and refuses to merge.
    let before = main_tip(&f).await;
    let outcome = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Stale {
            reason: "pull request is a draft".into()
        }
    );
    assert_eq!(main_tip(&f).await, before, "the draft did not merge");
    let intent_state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_ne!(intent_state, "merged");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_push_to_a_draft_does_not_request_code_owners() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();
    let author_id: Uuid = pull["author_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(codeowners_jobs(&f.pool, pull_id).await, 0);

    // A committed push to the draft's source branch. A ready pull would enqueue
    // the code-owner request job; a draft must not.
    let mut conn = f.pool.acquire().await.unwrap();
    let record = Record {
        sequence: 1,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(author_id),
        origin: None,
        updates: vec![Update {
            old: f.feature_head.clone(),
            new: "1".repeat(40),
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
    drop(conn);

    assert_eq!(
        codeowners_jobs(&f.pool, pull_id).await,
        0,
        "a push to a draft requests no code owners"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn code_owner_job_enqueued_while_ready_requests_nobody_after_convert_to_draft() {
    let f = fixture().await;

    // A write member owns the changed file (`feat.txt`), so the job would
    // request them if it ran while the pull request were ready.
    let (_writer, writer_username, _writer_token) = add_member(&f.pool, f.org_id, "write").await;
    let repo_path = state_repo_path(&f.storage_root, f.repo_id);
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(
        f.work.join("CODEOWNERS"),
        format!("feat.txt @{writer_username}\n"),
    )
    .unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "CODEOWNERS"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "owners",
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

    // A ready create enqueues the code-owner request job.
    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "ready", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let pull_id: Uuid = pull["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(codeowners_jobs(&f.pool, pull_id).await, 1);

    // Convert to a draft before the queued job runs.
    let (status, body) = api(
        &f.app,
        "POST",
        &format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base)),
        &f.token,
        json!({"draft": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The job re-reads the draft and requests nobody.
    run_jobs(&f.state).await;
    let requested: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pull_request_review_requests WHERE pull_request_id = $1",
    )
    .bind(pull_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        requested, 0,
        "the job must not notify a reviewer of a draft"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn draft_toggle_authorization_denies_non_writers() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let draft_href = format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base));

    // A read-only member (not the author) cannot mark it ready.
    let (_reader, _, reader_token) = add_member(&f.pool, f.org_id, "read").await;
    let (status, body) = api(
        &f.app,
        "POST",
        &draft_href,
        &reader_token,
        json!({"draft": false}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A write member can.
    let (_writer, _, writer_token) = add_member(&f.pool, f.org_id, "write").await;
    let (status, body) = api(
        &f.app,
        "POST",
        &draft_href,
        &writer_token,
        json!({"draft": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["draft"], false);

    // An unrelated account gets 404, not 403.
    let stranger = fixture_account(&f.pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&f.pool, stranger).await;
    let (status, _) = api(
        &f.app,
        "POST",
        &draft_href,
        &stranger_token,
        json!({"draft": true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn draft_toggle_is_idempotent() {
    let f = fixture().await;

    let (_, pull) = api(
        &f.app,
        "POST",
        &pulls_href(&f.api_base),
        &f.token,
        json!({"title": "wip", "source_branch": "feature", "target_branch": "main", "draft": true}),
    )
    .await;
    let number = pull["number"].as_i64().unwrap();
    let draft_href = format!("{pulls}/{number}/draft", pulls = pulls_href(&f.api_base));

    let key = tag("idem");
    let body = json!({"draft": false});
    let (status, first) = api_with_key(
        &f.app,
        "POST",
        &draft_href,
        &f.token,
        body.clone(),
        Some(&key),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Replaying the same key returns the same body and no second event.
    let (status, second) =
        api_with_key(&f.app, "POST", &draft_href, &f.token, body, Some(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second);

    assert_eq!(
        outbox_count(&f.pool, f.org_id, "pull_request.ready_for_review").await,
        1
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ui_shows_draft_badge_and_ready_controls() {
    let f = fixture().await;

    // Create a draft through the browser form.
    let (status, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let csrf_token = hidden(&form, "csrf_token");
    let idem_key = hidden(&form, "idempotency_key");
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &csrf_token),
            ("idempotency_key", &idem_key),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "draft pull"),
            ("body", ""),
            ("draft", "on"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(location, format!("{}/pulls/1", f.base));

    // The pull page shows the draft badge and the ready-for-review control.
    let (status, page) = get(&f.app, &location, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(">Draft</span>"),
        "page should show the draft badge: {page}"
    );
    assert!(
        page.contains("Ready for review"),
        "page should offer ready-for-review: {page}"
    );

    // Mark ready through the form.
    let csrf_token = hidden(&page, "csrf_token");
    let idem_key = hidden(&page, "idempotency_key");
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/pulls/1/ready_for_review", f.base),
        &f.cookie,
        &[("csrf_token", &csrf_token), ("idempotency_key", &idem_key)],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(location, format!("{}/pulls/1", f.base));

    // Now the page offers convert-to-draft and no longer shows the draft badge.
    let (status, page) = get(&f.app, &location, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !page.contains(">Draft</span>"),
        "draft badge should be gone: {page}"
    );
    assert!(
        page.contains("Convert to draft"),
        "page should offer convert-to-draft: {page}"
    );

    finish(f).await;
}
