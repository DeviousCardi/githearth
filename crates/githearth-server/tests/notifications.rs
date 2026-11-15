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

    // The built-in consumer's first registration. On this fixture's fresh
    // database it draws nothing: there are no position rows, and the fan-out
    // reads a missing position as zero, so every event the test writes is
    // delivered anyway. `0050` already wrote the same row on an upgraded
    // database. Kept because a test is the only caller that registers a
    // consumer before its handler exists (ADR 0006, revised for M4.1).
    githearth_server::outbox::register_builtin(&state)
        .await
        .unwrap();

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

/// Runs the relay and the runner as `serve` runs them: the relay fans the
/// event out to the consumer, the runner drains the delivery jobs.
///
/// After the drain, every event that existed before this call must have a
/// `delivered` delivery row: a relay that never enqueued the pair leaves its
/// delivery `pending`, and a quiet deadline `break` would swallow exactly that.
/// Both fail the test (review F4). Tests that write their own events also call
/// [`await_deliveries`] for the ones they wrote, which the relay only fans out
/// during this call.
async fn run_jobs(state: &AppState, org_id: Option<Uuid>) {
    // Scoped to the fixture's organization when the test has one: this keeps
    // the report about the test's own events. `None` is only for the raw
    // fixtures, which run on their own per-test database.
    let before: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM outbox_events \
         WHERE kind = ANY($1) AND ($2::uuid IS NULL OR organization_id = $2)",
    )
    .bind(githearth_server::notifications::OUTBOX_EVENT_KINDS)
    .bind(org_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    githearth_server::outbox::fan_out_batch(state, 10_000)
        .await
        .expect("outbox fan-out");
    let mut registry = githearth_server::jobs::Registry::new();
    githearth_server::notifications::register(&mut registry);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let runner = githearth_server::jobs::spawn(state.clone(), registry, shutdown.clone());
    // The runner has to be started before the first look, or it waits a full
    // poll interval with nothing pending.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Two consecutive empty looks before the drain ends: a job the runner
    // has not claimed yet already shows as `pending`, and an enqueue can
    // land between two looks under load. A quiet `break` at the deadline
    // would hide a relay that never enqueued the job, so it panics.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut quiet = 0;
    loop {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE state IN ('pending','running') \
             AND kind = 'notification.outbox'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if pending == 0 {
            quiet += 1;
            if quiet == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }
        quiet = 0;
        if tokio::time::Instant::now() >= deadline {
            panic!("the notification jobs did not drain within 60 s ({pending} left)");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), runner).await;
    // No handler of this suite writes its own event, so every event in `before`
    // is owed a delivery that ran. Counting the rows still `pending` fails when
    // the relay skipped a pair: the delivery is created and never enqueued.
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_deliveries d \
         WHERE d.event_id = ANY($1) AND d.state = 'pending'",
    )
    .bind(&before)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(open, 0, "a delivery never ran after the drain (review F4)");
    assert_every_event_reached_a_finished_delivery(state, org_id).await;
}

/// Every event the built-in consumer's filter selects for this test's
/// organization — not only those that existed before the drain — has a
/// `delivered` or `dead` delivery for it.
///
/// `before` cannot see an event the test wrote after its last `run_jobs`, so a
/// helper that only checks `before` passes a relay that silently drops that
/// event (review F4 follow-up). A skipped pair leaves the delivery missing; a
/// pair whose job was never enqueued leaves it `pending`; both fail here. The
/// query is scoped to the fixture's organization, so it reads only this test's
/// rows, and the filter is part of it, so an event the consumer is not owed
/// (`repository.created`) is not reported as lost.
async fn assert_every_event_reached_a_finished_delivery(state: &AppState, org_id: Option<Uuid>) {
    let lost: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT e.kind, e.id FROM outbox_events e \
         JOIN outbox_consumers c ON c.name = 'notification.outbox' \
         WHERE ($1::uuid IS NULL OR e.organization_id = $1) \
           AND (c.event_filter IS NULL OR c.event_filter = 'null'::jsonb \
                OR c.event_filter ? e.kind) \
           AND NOT EXISTS (SELECT 1 FROM outbox_deliveries d \
                           WHERE d.event_id = e.id AND d.consumer_id = c.id \
                             AND d.state IN ('delivered', 'dead')) \
         ORDER BY e.chain, e.seq",
    )
    .bind(org_id)
    .fetch_all(&state.pool)
    .await
    .unwrap();
    assert!(
        lost.is_empty(),
        "{} event(s) never reached a finished delivery: {lost:?} (review F4)",
        lost.len()
    );
}

/// Asserts that every event of `kind` in `chain`'s backlog reached a
/// `delivered` delivery row for the built-in consumer. A relay that skips a
/// pair never enqueues its job, so the drain has nothing to wait for and only
/// this row is missing.
async fn deliveries_delivered(state: &AppState, kind: &str) -> i64 {
    // Every delivery the fan-out created for `kind` is finished, and at least
    // one exists: a relay that skipped the pair creates none, which a plain
    // "no row is unfinished" check would read as success.
    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_deliveries d \
         JOIN outbox_events e ON e.id = d.event_id \
         WHERE e.kind = $1 AND d.consumer_id = \
               (SELECT id FROM outbox_consumers WHERE name = 'notification.outbox')",
    )
    .bind(kind)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_deliveries d \
         JOIN outbox_events e ON e.id = d.event_id \
         WHERE e.kind = $1 AND d.state = 'delivered' AND d.consumer_id = \
               (SELECT id FROM outbox_consumers WHERE name = 'notification.outbox')",
    )
    .bind(kind)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert!(created > 0, "no {kind} delivery was created at all");
    assert_eq!(
        created, delivered,
        "a {kind} event never reached a `delivered` delivery row"
    );
    delivered
}

/// Waits until every one of `events` has a delivery row for the built-in
/// consumer and that row is `delivered` or `dead`. A relay that skipped a pair
/// leaves a delivery missing and this times out, naming the lost event.
async fn await_deliveries(state: &AppState, events: &[Uuid]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let unfinished: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM unnest($1::uuid[]) AS e(id) \
             WHERE NOT EXISTS (SELECT 1 FROM outbox_deliveries d \
                               JOIN outbox_consumers c ON c.id = d.consumer_id \
                               WHERE d.event_id = e.id AND c.name = 'notification.outbox' \
                                 AND d.state IN ('delivered', 'dead'))",
        )
        .bind(events)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if unfinished == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{unfinished} of the test's events never reached a finished delivery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

    // At the path that writes the event: no handler enqueued a job beside it,
    // and after the relay there is exactly one notification job for it.
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE kind = 'pull_request.merged' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let direct: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind LIKE 'notification.%'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(direct, 0, "the merge path writes the event, not a job");
    run_jobs(&f.state, Some(f.org_id)).await;
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind LIKE 'notification.%' \
         AND payload->>'event_id' = $1",
    )
    .bind(event.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(jobs, 1, "one merged event, one notification job");

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

    run_jobs(&f.state, Some(f.org_id)).await;

    // The merge event itself reached a finished delivery: "no notification" is
    // then a decision of the handler, not a relay that never ran (review,
    // condition 2). Keyed on the merge intent, so it is this test's event.
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE kind = 'pull_request.merged' \
         AND payload->>'operation_id' = $1",
    )
    .bind(op.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    await_deliveries(&f.state, &[event]).await;
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

    // At the review path: one event, no job beside it, then one job from the
    // relay (not one direct plus one from the consumer).
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE kind = 'pull_request.approved' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let direct: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind LIKE 'notification.%'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(direct, 0, "the review path writes the event, not a job");
    run_jobs(&f.state, Some(f.org_id)).await;
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind LIKE 'notification.%' \
         AND payload->>'event_id' = $1",
    )
    .bind(event.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(jobs, 1, "one review event, one notification job");

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

    run_jobs(&f.state, Some(f.org_id)).await;

    // The self-review's event reached a finished delivery: an empty table is
    // then the handler's decision, not a relay that never ran (review,
    // condition 2).
    let event: Uuid = sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE kind = 'pull_request.reviewed' \
         ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    await_deliveries(&f.state, &[event]).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "self-review produces no notification");

    finish(f).await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// One notification per source row, however many events name it (review F1)
// ---------------------------------------------------------------------------

/// Records an outbox event for `kind` and returns its id, the way a writer
/// does: the chain row, then the audit row, so `outbox_assign` gives the event
/// its `(chain, seq)`. Each call is one event, whatever it names.
async fn write_event(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    actor_id: Uuid,
    kind: &str,
    payload: Value,
) -> Uuid {
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO audit_chain (chain, last_seq, last_hash) \
         VALUES (COALESCE($1, '00000000-0000-0000-0000-000000000000'::uuid), 0, sha256('')) \
         ON CONFLICT DO NOTHING",
    )
    .bind(organization_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_events (id, organization_id, actor_id, kind) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(actor_id)
    .bind(kind)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO outbox_events (id, organization_id, kind, payload) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(kind)
    .bind(payload)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_second_event_for_the_same_source_row_notifies_once() {
    let db = TestDb::new().await;
    let state = sql_state(&db.pool).await;
    let (author, actor, pull_request_id, review_id) = insert_reviewed_rows(&db.pool).await;

    // One merged event per intent: the payload every `pull_request.merged`
    // event carries. A replayed journal entry writes a second one under a new
    // event id, which is the shape the direct job never had.
    let organization_id: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM repositories LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let operation_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
             source_oid, target_ref, expected_target_oid, method, policy_generation, \
             checks_approvals_digest, actor_id, state) \
         SELECT $1, r.id, $2, 'refs/heads/feature', fixed_oid, 'refs/heads/main', fixed_oid, \
                'merge', 0, '', $3, 'merged' \
         FROM repositories r, LATERAL (SELECT repeat('a', 40) AS fixed_oid) o",
    )
    .bind(operation_id)
    .bind(pull_request_id)
    .bind(actor)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE pull_requests SET state = 'merged', merged_by = $2 WHERE id = $1")
        .bind(pull_request_id)
        .bind(actor)
        .execute(&db.pool)
        .await
        .unwrap();
    let merged_payload = json!({
        "repository_id": sqlx::query_scalar::<_, Uuid>("SELECT id FROM repositories LIMIT 1")
            .fetch_one(&db.pool).await.unwrap(),
        "operation_id": operation_id,
        "actor_id": actor,
    });
    let merged_events = vec![
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.merged",
            merged_payload.clone(),
        )
        .await,
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.merged",
            merged_payload,
        )
        .await,
    ];

    // One reviewed event per review.
    let reviewed_payload = json!({ "review_id": review_id, "pull_request_id": pull_request_id });
    let review_events = vec![
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.approved",
            reviewed_payload.clone(),
        )
        .await,
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.approved",
            reviewed_payload,
        )
        .await,
    ];

    // One review-requested event per request. The reviewer is a third account:
    // a request the author made of themself notifies nobody.
    let reviewer =
        fixture_account(&db.pool, &tag("reviewer"), "correct horse battery staple").await;
    let request_id: Uuid = sqlx::query_scalar(
        "INSERT INTO pull_request_review_requests (pull_request_id, reviewer_id, requested_by) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(pull_request_id)
    .bind(reviewer)
    .bind(actor)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let request_payload = json!({ "request_id": request_id, "pull_request_id": pull_request_id });
    let request_events = vec![
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.review_requested",
            request_payload.clone(),
        )
        .await,
        write_event(
            &db.pool,
            organization_id,
            actor,
            "pull_request.review_requested",
            request_payload,
        )
        .await,
    ];

    // The relay creates one delivery per event; the runner applies the handler.
    let mut all = merged_events.clone();
    all.extend(&review_events);
    all.extend(&request_events);
    run_jobs(&state, organization_id).await;
    await_deliveries(&state, &all).await;
    for (kind, events) in [
        ("pull_request.merged", &merged_events),
        ("pull_request.approved", &review_events),
        ("pull_request.review_requested", &request_events),
    ] {
        assert_eq!(
            deliveries_delivered(&state, kind).await,
            events.len() as i64,
            "every {kind} event was delivered"
        );
    }

    // Sorted on both sides: the database's collation decides whether
    // `review_requested` sorts before `reviewed`, and the assertion is about
    // the rows, not their order.
    let mut rows: Vec<(String, Uuid, Uuid)> =
        sqlx::query_as("SELECT kind, account_id, event_id FROM notifications")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let mut expected = vec![
        ("pull_request.merged".to_string(), author, operation_id),
        ("pull_request.reviewed".to_string(), author, review_id),
        (
            "pull_request.review_requested".to_string(),
            reviewer,
            request_id,
        ),
    ];
    rows.sort();
    expected.sort();
    assert_eq!(
        rows, expected,
        "one notification per source row, keyed on the row and not the event"
    );

    db.drop_db().await;
}

/// Builds an `AppState` on an already-migrated pool.
async fn sql_state(pool: &PgPool) -> AppState {
    // The state shares the pool the test writes through; nothing here opens a
    // second connection, so the runner drains the rows the test created.
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
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch,source_repository_id) \
         VALUES ($1,$2,1,'t','',$3,'src','dst',$2)",
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
        run_jobs(&state, None).await;
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

    run_jobs(&state, None).await;

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

    run_jobs(&state, None).await;

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
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch,source_repository_id) \
         VALUES ($1,$2,1,'t','',$3,'src','dst',$2)",
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
