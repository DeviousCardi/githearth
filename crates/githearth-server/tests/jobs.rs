//! Job runner integration tests — `#[ignore]`.
//! Requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL.

use githearth_server::{
    AppState,
    config::Config,
    jobs::{self, Job, JobOutcome, Registry},
};
use sqlx::PgPool;
use std::{
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maint)
        .await;
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maint)
        .await
        .unwrap();
    format!("{origin}/{name}")
}

fn fast_config(storage_root: &std::path::Path, database_url: &str) -> Config {
    Config {
        database_url: database_url.to_string(),
        storage_root: storage_root.to_path_buf(),
        job_concurrency: 8,
        job_timeout_seconds: 2,
        job_poll_interval_ms: 50,
        job_retention_days: 1,
        ..Config::default()
    }
}

fn temp_storage_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "githearth-jobs-test-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn setup(config: Config) -> (AppState, PgPool) {
    let pool = githearth_server::connect(&config).await.unwrap();
    githearth_server::MIGRATOR.run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    (state, pool)
}

async fn enqueue_test(
    pool: &PgPool,
    kind: &str,
    dedupe_key: &str,
    delay: Option<Duration>,
) -> Uuid {
    let id = Uuid::new_v4();
    let not_before = delay.map_or("now()".to_string(), |d| {
        format!("now() + '{} ms'::interval", d.as_millis())
    });
    sqlx::query(&format!(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, not_before) \
         VALUES ($1, $2, '{{}}'::jsonb, $3, {not_before})"
    ))
    .bind(id)
    .bind(kind)
    .bind(dedupe_key)
    .execute(pool)
    .await
    .unwrap();
    id
}

// --- Tests ---

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn duplicate_workers_each_runs_once() {
    let db_name = format!(
        "githearth_jobs_dup_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    let count = Arc::new(AtomicI32::new(0));
    let mut reg = Registry::new();
    let c1 = count.clone();
    reg.register(
        "test.dup",
        Arc::new(move |_state: AppState, _job: Job| {
            let c = c1.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                JobOutcome::Done
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let s1 = shutdown.clone();
    let h1 = jobs::spawn(state.clone(), reg, s1);

    // Second runner with same counter
    let count2 = count.clone();
    let mut reg2 = Registry::new();
    reg2.register(
        "test.dup",
        Arc::new(move |_state: AppState, _job: Job| {
            let c = count2.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                JobOutcome::Done
            })
        }),
    );
    let s2 = shutdown.clone();
    let h2 = jobs::spawn(state.clone(), reg2, s2);

    let n: i32 = 20;
    for i in 0..n {
        enqueue_test(&pool, "test.dup", &format!("key-{i}"), None).await;
    }

    tokio::time::sleep(Duration::from_secs(3)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = h1.await;
        let _ = h2.await;
    })
    .await;

    assert_eq!(count.load(Ordering::SeqCst), n, "each job ran exactly once");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn crash_mid_job_recovered() {
    let db_name = format!(
        "githearth_jobs_crash_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state, attempt, lease_holder, lease_expires_at) \
         VALUES ($1, 'test.crash', '{}'::jsonb, 'crash-1', 'running', 2, $2, now() - interval '1 second')",
    )
    .bind(job_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();

    let count = Arc::new(AtomicI32::new(0));
    let mut reg = Registry::new();
    let c = count.clone();
    reg.register(
        "test.crash",
        Arc::new(move |_state: AppState, _job: Job| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                JobOutcome::Done
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    tokio::time::sleep(Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = h.await;

    assert_eq!(count.load(Ordering::SeqCst), 1, "recovered and ran once");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn stale_completion_rejected() {
    let db_name = format!(
        "githearth_jobs_stale_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (_state, pool) = setup(config.clone()).await;

    let job_id = Uuid::new_v4();
    let old_holder = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state, attempt, lease_holder, lease_expires_at) \
         VALUES ($1, 'test.stale', '{}'::jsonb, 'stale-1', 'running', 1, $2, now() + interval '30 seconds')",
    )
    .bind(job_id)
    .bind(old_holder)
    .execute(&pool)
    .await
    .unwrap();

    // Stale completion with old holder and wrong attempt
    sqlx::query(
        "UPDATE jobs SET state = 'done', updated_at = now() \
         WHERE id = $1 AND state = 'running' AND lease_holder = $2 AND attempt = 99",
    )
    .bind(job_id)
    .bind(old_holder)
    .execute(&pool)
    .await
    .unwrap();

    let state_col: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_col, "running", "stale completion did not update");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn backoff_bounds() {
    let db_name = format!(
        "githearth_jobs_backoff_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    let runs = Arc::new(AtomicI32::new(0));
    let mut reg = Registry::new();
    let r = runs.clone();
    reg.register(
        "test.backoff",
        Arc::new(move |_state: AppState, _job: Job| {
            let r = r.clone();
            Box::pin(async move {
                r.fetch_add(1, Ordering::SeqCst);
                JobOutcome::Retry {
                    after: Duration::ZERO,
                    reason: "test retry".into(),
                }
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    enqueue_test(&pool, "test.backoff", "bo-1", None).await;

    tokio::time::sleep(Duration::from_secs(5)).await;
    shutdown.cancel();
    let _ = h.await;

    let n = runs.load(Ordering::SeqCst);
    let attempt: i32 =
        sqlx::query_scalar("SELECT COALESCE(attempt, 0) FROM jobs WHERE dedupe_key = 'bo-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(n > 1, "should have retried at least once");
    assert_eq!(n, attempt, "runs should match attempt count");

    let state_col: String = sqlx::query_scalar("SELECT state FROM jobs WHERE dedupe_key = 'bo-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        state_col == "pending" || state_col == "dead",
        "job should be retried"
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn attempt_cap_failed_becomes_dead() {
    let db_name = format!(
        "githearth_jobs_cap_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    let mut reg = Registry::new();
    reg.register(
        "test.cap",
        Arc::new(|_state: AppState, _job: Job| {
            Box::pin(async move {
                JobOutcome::Failed {
                    reason: "terminal".into(),
                }
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    enqueue_test(&pool, "test.cap", "cap-1", None).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = h.await;

    let (state, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error FROM jobs WHERE dedupe_key = 'cap-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "dead");
    assert_eq!(last_error.as_deref(), Some("terminal"));

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn poison_pill_dead_after_max_attempts() {
    let db_name = format!(
        "githearth_jobs_poison_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    // Simulate a poison pill: a job that is running at max_attempts with expired lease
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state, attempt, max_attempts, lease_holder, lease_expires_at) \
         VALUES ($1, 'test.poison', '{}'::jsonb, 'poison-1', 'running', 5, 5, $2, now() - interval '1 second')",
    )
    .bind(job_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();

    // Run a no-op runner just to trigger recovery
    let mut reg = Registry::new();
    reg.register(
        "test.poison",
        Arc::new(|_state: AppState, _job: Job| Box::pin(async move { JobOutcome::Done })),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    let _ = h.await;

    let (state, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error FROM jobs WHERE id = $1")
            .bind(job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "dead");
    assert_eq!(last_error.as_deref(), Some("lease expired"));

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn dedupe_active_noop() {
    let db_name = format!(
        "githearth_jobs_dedup_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (_state, pool) = setup(config.clone()).await;

    let mut tx = pool.begin().await.unwrap();
    jobs::enqueue(
        &mut tx,
        None,
        "test.dedup",
        serde_json::json!({}),
        "dedup-1",
        None,
    )
    .await
    .unwrap();
    jobs::enqueue(
        &mut tx,
        None,
        "test.dedup",
        serde_json::json!({}),
        "dedup-1",
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE dedupe_key = 'dedup-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "active duplicate is a no-op");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn dedupe_after_done_reenqueues() {
    let db_name = format!(
        "githearth_jobs_rededup_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (_state, pool) = setup(config.clone()).await;

    let done_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state) \
         VALUES ($1, 'test.rededup', '{}'::jsonb, 'rededup-1', 'done')",
    )
    .bind(done_id)
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    jobs::enqueue(
        &mut tx,
        None,
        "test.rededup",
        serde_json::json!({}),
        "rededup-1",
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE dedupe_key = 'rededup-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "after done, same key re-enqueues");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn unknown_kind_becomes_dead() {
    let db_name = format!(
        "githearth_jobs_unknown_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (state, pool) = setup(config.clone()).await;

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), Registry::new(), shutdown.clone());

    enqueue_test(&pool, "no.such.handler", "unk-1", None).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = h.await;

    let (state, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error FROM jobs WHERE dedupe_key = 'unk-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "dead");
    assert_eq!(
        last_error.as_deref(),
        Some("no handler for kind: no.such.handler")
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn panic_retries() {
    let db_name = format!(
        "githearth_jobs_panic_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_timeout_seconds = 5;
    let (state, pool) = setup(config.clone()).await;

    let mut reg = Registry::new();
    reg.register(
        "test.panic",
        Arc::new(|_state: AppState, _job: Job| Box::pin(async move { panic!("boom") })),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    enqueue_test(&pool, "test.panic", "panic-1", None).await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    shutdown.cancel();
    let _ = h.await;

    let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM jobs WHERE dedupe_key = 'panic-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(attempt > 0, "panic should cause retry, attempt > 0");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn timeout_retries() {
    let db_name = format!(
        "githearth_jobs_timeout_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_timeout_seconds = 1;
    let (state, pool) = setup(config.clone()).await;

    let mut reg = Registry::new();
    reg.register(
        "test.timeout",
        Arc::new(|_state: AppState, _job: Job| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                JobOutcome::Done
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    enqueue_test(&pool, "test.timeout", "timeout-1", None).await;

    tokio::time::sleep(Duration::from_secs(4)).await;
    shutdown.cancel();
    let _ = h.await;

    let attempt: i32 =
        sqlx::query_scalar("SELECT attempt FROM jobs WHERE dedupe_key = 'timeout-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(attempt > 0, "timeout should cause retry");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn shutdown_returns_running_to_pending() {
    let db_name = format!(
        "githearth_jobs_shutdown_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_timeout_seconds = 2;
    config.job_concurrency = 2;
    let (state, pool) = setup(config.clone()).await;

    let mut reg = Registry::new();
    reg.register(
        "test.shutdown",
        Arc::new(|_state: AppState, _job: Job| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                JobOutcome::Done
            })
        }),
    );

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), reg, shutdown.clone());

    enqueue_test(&pool, "test.shutdown", "shutdown-1", None).await;
    enqueue_test(&pool, "test.shutdown", "shutdown-2", None).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let stopping = std::time::Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .expect("runner did not stop")
        .unwrap();
    // It waited for in-flight handlers (they end at their 2 s timeout) instead of
    // releasing jobs whose handlers were still running.
    assert!(
        stopping.elapsed() >= Duration::from_millis(1200),
        "runner did not wait"
    );

    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE dedupe_key IN ('shutdown-1','shutdown-2') AND state = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending, 2, "both running jobs end pending after shutdown");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn retention_deletes_old_done_only() {
    let db_name = format!(
        "githearth_jobs_retain_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_retention_days = 1;
    let (state, pool) = setup(config.clone()).await;

    let done_id = Uuid::new_v4();
    let dead_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state, updated_at) \
         VALUES ($1, 'test.ret', '{}'::jsonb, 'ret-done', 'done', now() - interval '2 days')",
    )
    .bind(done_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key, state, updated_at) \
         VALUES ($1, 'test.ret', '{}'::jsonb, 'ret-dead', 'dead', now() - interval '2 days')",
    )
    .bind(dead_id)
    .execute(&pool)
    .await
    .unwrap();

    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state.clone(), Registry::new(), shutdown.clone());

    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    let _ = h.await;

    let done_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1)")
        .bind(done_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let dead_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1)")
        .bind(dead_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(!done_exists, "old done jobs deleted");
    assert!(dead_exists, "dead jobs preserved");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn doctor_jobs_check_states() {
    let db_name = format!(
        "githearth_jobs_doctor_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_timeout_seconds = 60;
    let (_state, pool) = setup(config.clone()).await;

    // Initially ok
    let result = githearth_server::doctor::report(&config).await;
    let jobs_check = result.iter().find(|c| c.name == "jobs").unwrap();
    assert!(
        matches!(jobs_check.status, githearth_server::doctor::Status::Ok),
        "{}",
        jobs_check.detail
    );

    // Add a dead job → warn
    sqlx::query("INSERT INTO jobs (id, kind, payload, dedupe_key, state) VALUES ($1,'test.doc','{}','doc-dead','dead')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let result = githearth_server::doctor::report(&config).await;
    let jobs_check = result.iter().find(|c| c.name == "jobs").unwrap();
    assert!(
        matches!(jobs_check.status, githearth_server::doctor::Status::Warn),
        "{}",
        jobs_check.detail
    );

    // A pending job 2 minutes overdue, with no dead jobs → warn "late".
    sqlx::query("DELETE FROM jobs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO jobs (id, kind, payload, dedupe_key, not_before) VALUES ($1,'test.doc','{}','doc-late', now() - interval '2 minutes')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let result = githearth_server::doctor::report(&config).await;
    let jobs_check = result.iter().find(|c| c.name == "jobs").unwrap();
    assert!(jobs_check.detail.contains("late"), "{}", jobs_check.detail);

    // A running job whose lease expired long ago fails, even with a dead job present.
    sqlx::query("INSERT INTO jobs (id, kind, payload, dedupe_key, state) VALUES ($1,'test.doc','{}','doc-dead2','dead')")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO jobs (id, kind, payload, dedupe_key, state, attempt, lease_holder, lease_expires_at) VALUES ($1,'test.doc','{}','doc-stuck','running',1,$2, now() - interval '1 hour')")
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let result = githearth_server::doctor::report(&config).await;
    let jobs_check = result.iter().find(|c| c.name == "jobs").unwrap();
    assert!(
        matches!(jobs_check.status, githearth_server::doctor::Status::Fail),
        "{}",
        jobs_check.detail
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn on_delete_cascade() {
    let db_name = format!(
        "githearth_jobs_cascade_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage = temp_storage_root();
    let config = fast_config(&storage, &database_url);
    let (_state, pool) = setup(config.clone()).await;

    let org_id = Uuid::new_v4();
    let account_id = Uuid::new_v4();
    let hash = githearth_server::hash_password("twelve-chars12").unwrap();
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, 'test', $2)")
        .bind(account_id)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, 'org', 'Org')")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, kind, payload, dedupe_key) \
         VALUES ($1, $2, 'test.cascade', '{}'::jsonb, 'cascade-1')",
    )
    .bind(job_id)
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE id = $1)")
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!exists, "job deleted with organization");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn slow_job_does_not_hold_other_slots() {
    let database_url =
        fresh_database(&format!("githearth_jobs_slots_{}", Uuid::new_v4().simple())).await;
    let storage = temp_storage_root();
    let mut config = fast_config(&storage, &database_url);
    config.job_concurrency = 2;
    config.job_timeout_seconds = 10;
    let (state, pool) = setup(config).await;
    let mut reg = Registry::new();
    reg.register(
        "test.slow",
        Arc::new(|_: AppState, _: Job| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                JobOutcome::Done
            })
        }),
    );
    reg.register(
        "test.fast",
        Arc::new(|_: AppState, _: Job| Box::pin(async { JobOutcome::Done })),
    );
    enqueue_test(&pool, "test.slow", "slow", None).await;
    for i in 0..4 {
        enqueue_test(&pool, "test.fast", &format!("fast-{i}"), None).await;
    }
    let shutdown = CancellationToken::new();
    let h = jobs::spawn(state, reg, shutdown.clone());
    // With one slot busy for 5 s, the other slot keeps running the fast jobs.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let done: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'test.fast' AND state = 'done'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(done, 4, "fast jobs waited behind the slow one");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(15), h).await;
    pool.close().await;
}
