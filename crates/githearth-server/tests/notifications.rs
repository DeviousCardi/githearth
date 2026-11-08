//! In-app notifications contract tests (PostgreSQL-backed).
//!
//! Merged and reviewed notifications flow from a real Git fixture: the author
//! opens a pull request, someone else (or the author themself) merges or
//! reviews it, the handler job runs through the durable runner, and the author
//! receives exactly the right number of notifications. The API tests exercise
//! listing, filtering, mark-read, isolation and token rejection.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{
    AppState, MIGRATOR, config::Config, connect, jobs, merge::execute_intent, router,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use sqlx::{
    Connection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
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

async fn fixture_account(pool: &PgPool, username: &str, password: &str) -> Uuid {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .bind(hash)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture_session(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')",
    )
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
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,'owner','account',$2,'organization',$3,$2)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    (id, slug)
}

/// Adds a second owner account and session.
async fn add_owner(pool: &PgPool, org_id: Uuid) -> (Uuid, String) {
    let account = fixture_account(pool, &tag("merger"), "correct horse battery staple").await;
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
         VALUES ($1,'owner','account',$2,'organization',$3,$2)",
    )
    .bind(Uuid::new_v4())
    .bind(account)
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
    let token = fixture_session(pool, account).await;
    (account, token)
}

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

struct Fixture {
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    work: PathBuf,
    org_slug: String,
    org_id: Uuid,
    author_token: String,
    author_id: Uuid,
    repo_name: String,
    feature_head: String,
    number: i64,
    app: Router,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

fn merge_href(org: &str, repo: &str, number: i64) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls/{number}/merge")
}

/// A real repository with `main` and `feature` and an open pull request
/// `feature -> main`, authored by the fixture owner.
async fn build_fixture(database_url: &str) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-notif-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: database_url.to_owned(),
        database_max_connections: 8,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        job_poll_interval_ms: 50,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let author = fixture_account(&pool, &tag("author"), "correct horse battery staple").await;
    let author_token = fixture_session(&pool, author).await;
    let (org_id, org_slug) = fixture_organization(&pool, author).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&author_token),
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

    let work = std::env::temp_dir().join(format!("git-notif-work-{}", Uuid::new_v4()));
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
        Some(&author_token),
        json!({"title": "pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();

    Fixture {
        state,
        pool,
        storage_root,
        work,
        org_slug,
        org_id,
        author_token,
        author_id: author,
        repo_name,
        feature_head,
        number,
        app,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn create_merge_intent(f: &Fixture, head: &str, token: &str) -> Uuid {
    let (status, intent) = request(
        &f.app,
        "POST",
        &merge_href(&f.org_slug, &f.repo_name, f.number),
        Some(token),
        json!({"method": "merge", "expected_head_oid": head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    intent["operation_id"].as_str().unwrap().parse().unwrap()
}

async fn approve(f: &Fixture, verdict: &str, commit: &str, token: &str) {
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(token),
        json!({"verdict": verdict, "commit_id": commit}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Runs the durable runner with the notification registry until the pending job
/// queue is drained, then stops it.
async fn run_jobs(state: &AppState) {
    let mut registry = jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = jobs::spawn(state.clone(), registry, shutdown.clone());
    tokio::time::sleep(Duration::from_millis(800)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

fn replace_db_name(url: &str, new_db: &str) -> String {
    if let Some(pos) = url.rfind('/') {
        let (base, _) = url.split_at(pos);
        format!("{base}/{new_db}")
    } else {
        format!("{url}/{new_db}")
    }
}

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
        let db_name = format!("githearth_notif_{}", Uuid::new_v4().simple());
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
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(
                PgConnectOptions::from_str(&url)
                    .unwrap()
                    .disable_statement_logging(),
            )
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

// ---------------------------------------------------------------------------
// Merged notifications
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn merge_by_someone_else_notifies_author_once() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url).await;

    let (merger, merger_token) = add_owner(&f.pool, f.org_id).await;
    let op = create_merge_intent(&f, &f.feature_head, &merger_token).await;
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(op)
        .execute(&f.pool)
        .await
        .unwrap();
    githearth_server::merge::execute_intent(&f.state, op)
        .await
        .unwrap();

    run_jobs(&f.state).await;

    let rows: Vec<(Uuid, String, Option<Uuid>, Uuid)> =
        sqlx::query_as("SELECT account_id, kind, actor_id, event_id FROM notifications")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, f.author_id);
    assert_eq!(rows[0].1, "pull_request.merged");
    assert_eq!(rows[0].2, Some(merger));

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn self_merge_notifies_nobody() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url).await;

    let op = create_merge_intent(&f, &f.feature_head, &f.author_token).await;
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(op)
        .execute(&f.pool)
        .await
        .unwrap();
    execute_intent(&f.state, op).await.unwrap();

    run_jobs(&f.state).await;

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "self-merge produces no notification");

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Reviewed notifications
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn review_by_someone_else_notifies_author_once() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url).await;

    let (_reviewer, reviewer_token) = add_owner(&f.pool, f.org_id).await;
    approve(&f, "approved", &f.feature_head, &reviewer_token).await;

    run_jobs(&f.state).await;

    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT account_id, kind FROM notifications")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, f.author_id);
    assert_eq!(rows[0].1, "pull_request.reviewed");

    finish(f).await;
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn self_review_notifies_nobody() {
    let db = TestDb::new().await;
    let f = build_fixture(&db.url).await;

    // The author comments on their own pull request (approving is forbidden).
    let (status, body) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.org_slug, &f.repo_name),
            f.number
        ),
        Some(&f.author_token),
        json!({"verdict": "commented", "commit_id": f.feature_head, "body": "looks good"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    run_jobs(&f.state).await;

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "self-review produces no notification");

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// Handler idempotency and deleted targets (no Git fixture required)
// ---------------------------------------------------------------------------

/// Builds an `AppState` on an already-migrated pool.
async fn sql_state(pool: &PgPool) -> AppState {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        storage_root: std::env::temp_dir().join(format!("githearth-notif-{}", Uuid::new_v4())),
        job_poll_interval_ms: 50,
        ..Config::default()
    };
    AppState::new(pool.clone(), config).await.unwrap()
}

/// Inserts an org/repo/pull request (authored by `author`) and a review by
/// `actor`, returning `(author_id, actor_id, pull_request_id, review_id)`.
async fn insert_reviewed_rows(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid) {
    let author = fixture_account(pool, &tag("author"), "correct horse battery staple").await;
    let actor = fixture_account(pool, &tag("actor"), "correct horse battery staple").await;
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    let repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
         VALUES ($1,$2,'r',$3,$4,'active','private')",
    )
    .bind(repo_id)
    .bind(org_id)
    .bind(author)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    let pull_request_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
         VALUES ($1,$2,1,'t','',$3,'src','dst')",
    )
    .bind(pull_request_id)
    .bind(repo_id)
    .bind(author)
    .execute(pool)
    .await
    .unwrap();
    let review_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_request_reviews (id,pull_request_id,reviewer_id,verdict,commit_id,body) \
         VALUES ($1,$2,$3,'approved','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','')",
    )
    .bind(review_id)
    .bind(pull_request_id)
    .bind(actor)
    .execute(pool)
    .await
    .unwrap();
    (author, actor, pull_request_id, review_id)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn handler_is_idempotent_across_redelivery() {
    let db = TestDb::new().await;
    let state = sql_state(&db.pool).await;
    let (_author, _actor, _pull_request_id, review_id) = insert_reviewed_rows(&db.pool).await;

    // Enqueue the reviewed job twice (the second is a redelivery with a fresh
    // dedupe key, as after a crash or retry), and run the runner both times.
    for i in 0..2 {
        let mut tx = db.pool.begin().await.unwrap();
        jobs::enqueue(
            &mut tx,
            None,
            "notification.pull_request_reviewed",
            json!({"review_id": review_id}),
            &format!("redeliver-{i}"),
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        run_jobs(&state).await;
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "the same review must produce exactly one notification"
    );

    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleted_pull_request_yields_done_without_a_row() {
    let db = TestDb::new().await;
    let state = sql_state(&db.pool).await;

    // A merged job pointing at a pull request that does not exist.
    let missing_pr = Uuid::new_v4();
    let mut tx = db.pool.begin().await.unwrap();
    jobs::enqueue(
        &mut tx,
        None,
        "notification.pull_request_merged",
        json!({"event_id": Uuid::new_v4(), "pull_request_id": missing_pr}),
        "deleted-pr",
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    run_jobs(&state).await;

    // The handler returned Done: the job is done and no notification was written.
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM jobs WHERE dedupe_key = 'deleted-pr'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(job_state, "done");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn malformed_payload_fails_the_job() {
    let db = TestDb::new().await;
    let state = sql_state(&db.pool).await;

    // Three jobs, each with a payload missing a required id, must fail (go
    // `dead`) rather than retry or silently insert.
    let cases: Vec<(&str, &str, Value)> = vec![
        (
            "notification.pull_request_merged",
            "merged-missing-event",
            json!({"pull_request_id": Uuid::new_v4()}),
        ),
        (
            "notification.pull_request_merged",
            "merged-missing-pr",
            json!({"event_id": Uuid::new_v4()}),
        ),
        (
            "notification.pull_request_merged",
            "merged-invalid-pr",
            json!({"event_id": Uuid::new_v4(), "pull_request_id": "not-a-uuid"}),
        ),
        (
            "notification.pull_request_reviewed",
            "reviewed-missing-review",
            json!({}),
        ),
        (
            "notification.pull_request_reviewed",
            "reviewed-invalid-review",
            json!({"review_id": "not-a-uuid"}),
        ),
    ];

    let mut tx = db.pool.begin().await.unwrap();
    for (kind, key, payload) in &cases {
        jobs::enqueue(&mut tx, None, kind, payload.clone(), key, None)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    run_jobs(&state).await;

    for (_, key, _) in &cases {
        let state_col: String = sqlx::query_scalar("SELECT state FROM jobs WHERE dedupe_key = $1")
            .bind(key)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            state_col, "dead",
            "malformed payload for {key} must be dead"
        );
    }

    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// API tests (shared database, no Git fixture)
// ---------------------------------------------------------------------------

async fn api_pool() -> (PgPool, Router) {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: std::env::temp_dir().join(format!("githearth-notif-api-{}", Uuid::new_v4())),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    (pool, app)
}

/// Builds accounts A and B in their own organization with a real pull request,
/// then inserts a read notification and an unread notification for A.
async fn notify_fixture(pool: &PgPool) -> (Uuid, Uuid, String, Uuid, Uuid) {
    let a = fixture_account(pool, &tag("a"), "correct horse battery staple").await;
    let b = fixture_account(pool, &tag("b"), "correct horse battery staple").await;
    let actor_username = tag("actor");
    let actor = fixture_account(pool, &actor_username, "correct horse battery staple").await;
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    let repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
         VALUES ($1,$2,'r',$3,$4,'active','private')",
    )
    .bind(repo_id)
    .bind(org_id)
    .bind(a)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    let pr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch) \
         VALUES ($1,$2,1,'t','',$3,'src','dst')",
    )
    .bind(pr_id)
    .bind(repo_id)
    .bind(a)
    .execute(pool)
    .await
    .unwrap();

    let read_id = Uuid::new_v4();
    let unread_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO notifications (id,account_id,kind,event_id,pull_request_id,actor_id,read_at) \
         VALUES ($1,$2,'pull_request.merged',$3,$4,$5, now() - interval '1 hour')",
    )
    .bind(read_id)
    .bind(a)
    .bind(Uuid::new_v4())
    .bind(pr_id)
    .bind(actor)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO notifications (id,account_id,kind,event_id,pull_request_id,actor_id) \
         VALUES ($1,$2,'pull_request.reviewed',$3,$4,$5)",
    )
    .bind(unread_id)
    .bind(a)
    .bind(Uuid::new_v4())
    .bind(pr_id)
    .bind(actor)
    .execute(pool)
    .await
    .unwrap();

    (a, b, actor_username, read_id, unread_id)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn notifications_api_listing_and_mark_read() {
    let (pool, app) = api_pool().await;
    let (a, _b, actor_username, read_id, unread_id) = notify_fixture(&pool).await;
    let session = fixture_session(&pool, a).await;

    // Full listing: newest first, at most 50, both notifications for A.
    let (status, list) = request(
        &app,
        "GET",
        "/api/v1/user/notifications",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], unread_id.to_string());
    assert_eq!(items[0]["kind"], "pull_request.reviewed");
    assert_eq!(items[0]["actor_username"], actor_username);
    assert_eq!(items[0]["read_at"], Value::Null);
    assert_eq!(items[1]["id"], read_id.to_string());
    assert!(items[1]["read_at"].is_string());

    // Filtering to unread only.
    let (status, list) = request(
        &app,
        "GET",
        "/api/v1/user/notifications?unread=true",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], unread_id.to_string());

    // Mark read returns 204 and sets read_at; it is idempotent.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/user/notifications/{unread_id}/read"),
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, list) = request(
        &app,
        "GET",
        "/api/v1/user/notifications?unread=true",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);

    // unread=false is no filter: the read notification is still listed.
    let (status, list) = request(
        &app,
        "GET",
        "/api/v1/user/notifications?unread=false",
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == unread_id.to_string()),
        "{list}"
    );

    let read_at = || {
        sqlx::query_scalar::<_, String>("SELECT read_at::text FROM notifications WHERE id = $1")
            .bind(unread_id)
            .fetch_one(&pool)
    };
    let first_read_at = read_at().await.unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/user/notifications/{unread_id}/read"),
        Some(&session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "mark-read is idempotent");
    assert_eq!(
        read_at().await.unwrap(),
        first_read_at,
        "the first read time is kept"
    );

    // Another account's notification id is 404 for B.
    let b_session = fixture_session(&pool, _b).await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/user/notifications/{read_id}/read"),
        Some(&b_session),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn notifications_api_rejects_access_tokens() {
    let (pool, app) = api_pool().await;
    let (a, _b, _actor, _read_id, _unread_id) = notify_fixture(&pool).await;
    let session = fixture_session(&pool, a).await;

    // Create a real access token through the session API.
    let (status, value) = request(
        &app,
        "POST",
        "/api/v1/user/tokens",
        Some(&session),
        json!({"name": "n", "scopes": ["repo:read", "repo:write"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{value}");
    let token = value["token"].as_str().unwrap().to_owned();

    // A valid access token is refused 403 on both read and write.
    let (status, _) = request(
        &app,
        "GET",
        "/api/v1/user/notifications",
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(
        &app,
        "POST",
        &format!("/api/v1/user/notifications/{_unread_id}/read"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    pool.close().await;
}
