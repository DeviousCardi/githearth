//! Outbox fan-out integration tests — `#[ignore]`.
//!
//! Acceptance case U12 (ADR 0006 "Revision for M4.1"): every event committed
//! after the cutover reaches every consumer after a crash and restart, one
//! consumer's failure never delays another, duplicates are harmless, a consumer
//! whose handler fails for 30 minutes still receives every event afterwards
//! (asserted through the next attempt time, not only the final state), a
//! consumer registered after the cutover receives nothing older, an upgrade
//! delivers nothing from before the cutover, and one event produces one
//! notification job while `codeowners.request_reviews` still fires directly.
//!
//! The tests drive the public interfaces: the relay (`outbox::fan_out_batch`),
//! the durable runner (`jobs::spawn`), the notification handler, `doctor`, and
//! the router for the paths a user takes. Direct SQL only reads what those
//! interfaces wrote, seeds events the way the domain writers do when the
//! fixture for that path lives in another file, or stages a pre-upgrade
//! database.
//!
//! Requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL.
//! Each test creates and drops its own database; set
//! `GITHEARTH_KEEP_TEST_DATABASE=1` to keep it for inspection.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{
    AppState, MIGRATOR,
    config::Config,
    jobs::{self, Job, JobOutcome, Registry},
    outbox::{self, Consumer, RequeueOutcome},
    router,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, Connection, PgPool, postgres::PgConnectOptions};
use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Db {
    name: String,
    url: String,
    admin: PgConnectOptions,
}

impl Db {
    /// A fresh database; `migrate` applies the whole schema, `false` leaves it
    /// empty so a test can stage the pre-upgrade schema itself.
    async fn new(migrate: bool) -> Self {
        let base =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let name = format!("githearth_outbox_{}", Uuid::new_v4().simple());
        let admin = PgConnectOptions::from_str(&base)
            .unwrap()
            .disable_statement_logging();
        let mut conn = admin.clone().connect().await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let (origin, _) = base.rsplit_once('/').unwrap();
        let db = Self {
            url: format!("{origin}/{name}"),
            name,
            admin,
        };
        if migrate {
            let pool = db.pool().await;
            MIGRATOR.run(&pool).await.unwrap();
            pool.close().await;
        }
        db
    }

    async fn pool(&self) -> PgPool {
        sqlx::PgPool::connect(&self.url).await.unwrap()
    }

    async fn drop_db(self) {
        if std::env::var("GITHEARTH_KEEP_TEST_DATABASE").is_ok() {
            eprintln!("keeping test database {}", self.name);
            return;
        }
        let mut conn = self.admin.connect().await.unwrap();
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&mut conn)
        .await;
        let _ = conn.close().await;
    }
}

fn fast_config(url: &str) -> Config {
    let storage_root = std::env::temp_dir().join(format!(
        "githearth-outbox-test-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&storage_root).unwrap();
    Config {
        database_url: url.to_string(),
        database_max_connections: 8,
        database_timeout_seconds: 5,
        job_timeout_seconds: 5,
        job_poll_interval_ms: 50,
        outbox_poll_interval_ms: 50,
        storage_root,
        ..Config::default()
    }
}

async fn state_for(config: &Config) -> (AppState, PgPool) {
    let pool = githearth_server::connect(config).await.unwrap();
    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    (state, pool)
}

/// A registered account and a session token for it. The token paths are other
/// files' subject; this one needs an authenticated caller.
async fn account(pool: &PgPool, username: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let hash = githearth_server::hash_password("twelve-chars12").unwrap();
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(username)
        .bind(&hash)
        .execute(pool)
        .await
        .unwrap();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash, account_id, expires_at) \
         VALUES (sha256($1::bytea), $2, now() + interval '1 hour')",
    )
    .bind(&token)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    (id, token)
}

async fn organization(pool: &PgPool, owner: Uuid, slug: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1,$2,$2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1,$2,'owner')",
    )
    .bind(id)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Writes one committed event exactly as the domain writers do: the chain row
/// if this is the chain's first event, the audit row (which assigns
/// `(chain, seq)` through the trigger) and the outbox row, in one transaction.
async fn commit_event(
    pool: &PgPool,
    org: Option<Uuid>,
    actor: Uuid,
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
    .bind(org)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_events (id, organization_id, actor_id, kind) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(org)
    .bind(actor)
    .bind(kind)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO outbox_events (id, organization_id, kind, payload) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(org)
    .bind(kind)
    .bind(payload)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

// ---------------------------------------------------------------------------
// Two probe consumers: they count what they are handed and can be made to fail
// ---------------------------------------------------------------------------

const CONSUMER_A: &str = "test.consume-a";
const CONSUMER_B: &str = "test.consume-b";
const JOB_A: &str = "test.consume-a.job";
const JOB_B: &str = "test.consume-b.job";

struct Probes {
    a_done: AtomicI64,
    a_fail: AtomicBool,
    b_done: AtomicI64,
    b_fail: AtomicBool,
}

impl Probes {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            a_done: AtomicI64::new(0),
            a_fail: AtomicBool::new(false),
            b_done: AtomicI64::new(0),
            b_fail: AtomicBool::new(false),
        })
    }

    fn a(&self) -> i64 {
        self.a_done.load(Ordering::SeqCst)
    }

    fn b(&self) -> i64 {
        self.b_done.load(Ordering::SeqCst)
    }
}

fn probe_handler(
    probes: Arc<Probes>,
    done: fn(&Probes) -> &AtomicI64,
    fail: fn(&Probes) -> &AtomicBool,
) -> jobs::Handler {
    Arc::new(move |_state: AppState, _job: Job| {
        let probes = probes.clone();
        Box::pin(async move {
            if fail(&probes).load(Ordering::SeqCst) {
                return JobOutcome::Retry {
                    after: Duration::ZERO,
                    reason: "dependency down (test)".into(),
                };
            }
            done(&probes).fetch_add(1, Ordering::SeqCst);
            JobOutcome::Done
        })
    })
}

fn consume_a() -> Consumer {
    Consumer::new(CONSUMER_A, JOB_A)
}

fn consume_b() -> Consumer {
    Consumer::new(CONSUMER_B, JOB_B)
}

/// The runner registry: the real notification handler plus the two probes.
/// `fast: true` caps every retry at 20 ms so a test reaches the attempt cap
/// without waiting; production has no such cap (the job's own applies).
fn registry(probes: &Arc<Probes>, fast: bool) -> Registry {
    let mut registry = Registry::new();
    githearth_server::notifications::register(&mut registry);
    registry.mark_outbox_delivery(githearth_server::notifications::OUTBOX_JOB_KIND);
    registry.register(
        JOB_A,
        probe_handler(probes.clone(), |p| &p.a_done, |p| &p.a_fail),
    );
    registry.register(
        JOB_B,
        probe_handler(probes.clone(), |p| &p.b_done, |p| &p.b_fail),
    );
    if fast {
        registry.backoff_override = Some(Duration::from_millis(20));
    }
    registry
}

async fn relay_once(state: &AppState) -> u64 {
    outbox::fan_out_batch(state, 1000).await.unwrap()
}

/// Runs the runner for `ms` and returns whether any probe job is still pending.
async fn run_for(state: &AppState, probes: &Arc<Probes>, fast: bool, ms: u64) {
    let shutdown = CancellationToken::new();
    let handle = jobs::spawn(state.clone(), registry(probes, fast), shutdown.clone());
    tokio::time::sleep(Duration::from_millis(ms)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

/// Runs the runner until no probe or notification job is pending, then stops.
async fn run_until_quiet(state: &AppState, probes: &Arc<Probes>) {
    let shutdown = CancellationToken::new();
    let handle = jobs::spawn(state.clone(), registry(probes, true), shutdown.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE state IN ('pending','running') \
             AND kind IN ($1, $2, $3)",
        )
        .bind(JOB_A)
        .bind(JOB_B)
        .bind(githearth_server::notifications::OUTBOX_JOB_KIND)
        .fetch_one(&state.pool)
        .await
        .unwrap();
        if pending == 0 || tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
}

async fn delivery_count(pool: &PgPool, consumer: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1",
    )
    .bind(consumer)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn delivery_states(pool: &PgPool, consumer: &str) -> Vec<(String, i32)> {
    sqlx::query_as(
        "SELECT d.state, d.attempts FROM outbox_deliveries d \
         JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1 ORDER BY d.chain, d.seq",
    )
    .bind(consumer)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn finish(config: &Config, pool: PgPool, db: Db) {
    let _ = std::fs::remove_dir_all(&config.storage_root);
    pool.close().await;
    db.drop_db().await;
}

// ---------------------------------------------------------------------------
// 1. Every event after the cutover reaches every consumer across a crash.
//    Revert that makes it fail: drop the per-consumer delivery rows (fan the
//    event out only for the first consumer) or remove the `NOT EXISTS` guard in
//    `fan_out_batch`, and the second consumer's deliveries never appear.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn every_event_after_the_cutover_reaches_every_consumer_across_a_crash() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "fanout_actor").await;
    let org = organization(&pool, actor, "fanout").await;
    // One audited write on the site chain, so its chain row exists as it does
    // in a running server (the audit trigger creates it inside the writer's
    // transaction; nothing else ever creates it).
    commit_event(&pool, None, actor, "test.site.warmup", json!({})).await;
    outbox::register_builtin(&state).await.unwrap();
    // The two probe consumers register before the events, as consumers do in a
    // running server; their rows and positions are durable.
    outbox::register(&pool, &consume_a()).await.unwrap();
    outbox::register(&pool, &consume_b()).await.unwrap();
    let probes = Probes::new();

    // Committed, then the process dies before the relay ever runs. The kinds
    // are ones the notification consumer takes, so all three consumers are due.
    for i in 0..5 {
        commit_event(
            &pool,
            Some(org),
            actor,
            "pull_request.approved",
            json!({"i": i}),
        )
        .await;
    }
    let undelivered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE delivered_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        undelivered, 6,
        "committed and undelivered, as after a crash"
    );

    // Restart: the relay and the runner are new processes; the consumers'
    // rows and positions are what survives.
    let fanned = relay_once(&state).await;
    assert_eq!(
        fanned, 15,
        "five events times three consumers (the site warm-up has no consumer)"
    );
    run_until_quiet(&state, &probes).await;

    for consumer in [CONSUMER_A, CONSUMER_B, "notification.outbox"] {
        let states = delivery_states(&pool, consumer).await;
        assert_eq!(states.len(), 5, "{consumer}: one delivery per event");
        assert!(
            states.iter().all(|(s, _)| s != "pending" && s != "dead"),
            "{consumer}: {states:?}"
        );
    }
    assert_eq!(probes.a(), 5, "consumer A ran every event");
    assert_eq!(probes.b(), 5, "consumer B ran every event");
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE delivered_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "every event's fan-out committed");
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 2. A consumer that fails for 30 minutes still receives every later event,
//    asserted through the next attempt time at every step of the outage.
//    Revert: make the delivery job inherit the generic 5-attempt budget (drop
//    `max_attempts` from `enqueue_with` in the relay) and the outage exhausts
//    it before 30 minutes, so the delivery is `dead` and never recovers.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_consumer_down_for_thirty_minutes_keeps_its_budget_and_recovers() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "outage_actor").await;
    let org = organization(&pool, actor, "outage").await;
    outbox::register(&pool, &consume_a()).await.unwrap();
    let probes = Probes::new();
    probes.a_fail.store(true, Ordering::SeqCst);

    commit_event(&pool, Some(org), actor, "test.outage.first", json!({})).await;
    relay_once(&state).await;

    // Four rounds standing in for the 30-minute outage: the job retries with
    // its real budget (30 attempts, 1 s doubling, 10-minute cap) and the test
    // walks it rather than sleeping for half an hour. Each round makes the
    // next attempt due, as the poller would when the time comes. No `fast`
    // override: this clause is about the stored cap, and a 20 ms override would
    // let a schedule of seconds pass it (review F5).
    let mut last_attempts = 0;
    for round in 1..=4 {
        run_for(&state, &probes, false, 300).await;
        let (state_col, attempts, next_in_seconds, max_attempts): (String, i32, f64, i32) =
            sqlx::query_as(
                "SELECT d.state, d.attempts, \
                        EXTRACT(EPOCH FROM (d.next_attempt_at - now()))::float8, d.max_attempts \
                 FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
                 WHERE c.name = $1",
            )
            .bind(CONSUMER_A)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(
            state_col, "dead",
            "round {round}: the budget covers the outage"
        );
        assert_eq!(
            max_attempts, 30,
            "the delivery carries the consumer's budget"
        );
        // The next attempt time is the runner's, not the row's creation time:
        // it moved forward with the attempt that just failed.
        assert!(
            next_in_seconds < 600.0 * 1.25,
            "round {round}: the next attempt is at most the cap away (was {next_in_seconds})"
        );
        assert!(
            attempts > last_attempts,
            "round {round}: attempts advance ({attempts} > {last_attempts})"
        );
        last_attempts = attempts;
        let job_pending: String = sqlx::query_scalar(
            "SELECT state FROM jobs WHERE kind = $1 ORDER BY created_at LIMIT 1",
        )
        .bind(JOB_A)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            job_pending, "pending",
            "round {round}: waiting for its next attempt"
        );
        jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string(), JOB_B.to_string()]).await;
    }
    assert!(
        last_attempts < 30,
        "the outage used {last_attempts} of 30 attempts"
    );
    assert_eq!(probes.a(), 0, "nothing was delivered while down");

    // The outage ends. The SAME job finishes, and every later event arrives.
    probes.a_fail.store(false, Ordering::SeqCst);
    jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string(), JOB_B.to_string()]).await;
    run_until_quiet(&state, &probes).await;
    assert_eq!(probes.a(), 1, "the held event is delivered on recovery");
    let first = delivery_states(&pool, CONSUMER_A).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, "delivered");
    assert!(
        first[0].1 >= last_attempts && first[0].1 < 30,
        "the same job finished on an attempt after the outage ({} then, {} now)",
        last_attempts,
        first[0].1
    );

    for i in 0..2 {
        commit_event(
            &pool,
            Some(org),
            actor,
            &format!("test.after.{i}"),
            json!({}),
        )
        .await;
    }
    relay_once(&state).await;
    run_until_quiet(&state, &probes).await;
    assert_eq!(
        probes.a(),
        3,
        "every event after the outage reached the consumer"
    );
    let states = delivery_states(&pool, CONSUMER_A).await;
    assert!(states.iter().all(|(s, _)| s == "delivered"), "{states:?}");
    let span = outbox::retry_span_seconds(30, 600);
    assert!(
        span > 3 * 3600,
        "the budget spans {span} s, more than three hours"
    );
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 3. One consumer's failure never delays another's delivery, and the failing
//    consumer still receives every later event.
//    Revert: enqueue one job per event instead of one per (event, consumer):
//    B then waits behind A's retries and its delivery never arrives while A is
//    failing. Per-consumer isolation is visible in A's next attempt time.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn one_consumers_failure_does_not_delay_another() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "isolation_actor").await;
    let org = organization(&pool, actor, "isolation").await;
    // A's budget is small enough to exhaust while B keeps delivering, so the
    // test reaches "A is dead and B is unharmed" by the real mechanism rather
    // than by writing A's state.
    let struggling = Consumer {
        max_attempts: 2,
        ..consume_a()
    };
    outbox::register(&pool, &struggling).await.unwrap();
    outbox::register(&pool, &consume_b()).await.unwrap();
    let probes = Probes::new();
    probes.a_fail.store(true, Ordering::SeqCst);

    commit_event(&pool, Some(org), actor, "test.isolation.1", json!({})).await;
    relay_once(&state).await;
    run_for(&state, &probes, false, 400).await;

    // A is stuck retrying; B finished regardless.
    assert_eq!(probes.b(), 1, "consumer B delivered while A was failing");
    assert_eq!(probes.a(), 0);
    assert_eq!(
        delivery_states(&pool, CONSUMER_B).await,
        vec![("delivered".to_string(), 1)]
    );
    assert_ne!(
        delivery_states(&pool, CONSUMER_A).await[0].0,
        "delivered",
        "A is still in its own retry"
    );
    // Isolation is visible in A's next attempt time, not only in B's state:
    // A's own job moved forward on its backoff while B's was delivered.
    let (a_state, a_next_in): (String, f64) = sqlx::query_as(
        "SELECT d.state, EXTRACT(EPOCH FROM (d.next_attempt_at - now()))::float8 \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_state, "pending", "A waits for its next attempt");
    assert!(
        a_next_in > 0.0 && a_next_in < 5.0,
        "A's next attempt is its own backoff, not B's delivery: {a_next_in}s"
    );
    let b_next_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (d.next_attempt_at - now()))::float8 \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1",
    )
    .bind(CONSUMER_B)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        b_next_in <= a_next_in,
        "B's schedule is not behind A's: B {b_next_in}s, A {a_next_in}s"
    );

    // A later event reaches B at once even while A is still failing.
    commit_event(&pool, Some(org), actor, "test.isolation.2", json!({})).await;
    assert_eq!(
        relay_once(&state).await,
        2,
        "one delivery for each consumer"
    );
    run_for(&state, &probes, true, 300).await;
    assert_eq!(probes.b(), 2, "B is never held back by A");

    // A exhausts its two-attempt budget on the real mechanism: its first
    // delivery goes dead while B is delivered.
    jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string(), JOB_B.to_string()]).await;
    run_for(&state, &probes, true, 400).await;
    let a_states = delivery_states(&pool, CONSUMER_A).await;
    assert!(
        a_states.iter().any(|(s, _)| s == "dead"),
        "A exhausted its budget: {a_states:?}"
    );
    assert!(
        delivery_states(&pool, CONSUMER_B)
            .await
            .iter()
            .all(|(s, _)| s == "delivered"),
        "B is unaffected by A's dead letters"
    );

    // The failing consumer recovers and receives every event it was still owed,
    // including the one whose delivery went dead before it recovered.
    probes.a_fail.store(false, Ordering::SeqCst);
    let results = outbox::requeue(&pool, Some(CONSUMER_A), None, false)
        .await
        .unwrap();
    assert!(
        !results.is_empty(),
        "the dead delivery is recoverable through `jobs requeue`"
    );
    for id in &results {
        assert!(matches!(&id.2, RequeueOutcome::Requeued { .. }), "{id:?}");
    }
    jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string(), JOB_B.to_string()]).await;
    run_for(&state, &probes, true, 400).await;
    jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string(), JOB_B.to_string()]).await;
    run_for(&state, &probes, true, 400).await;
    assert_eq!(
        probes.a(),
        2,
        "the failing consumer receives every later event once it recovers"
    );
    assert_eq!(
        probes.b(),
        2,
        "a dead consumer does not stop another from delivering"
    );
    assert_eq!(
        delivery_states(&pool, CONSUMER_B).await.len(),
        2,
        "B has one delivery per event"
    );
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 4. Duplicates are harmless.
//    Revert: drop the unique `(event_id, consumer_id)` on `outbox_deliveries`
//    (or the `ON CONFLICT DO NOTHING` writers): three relay passes then make
//    three deliveries and three jobs for one event.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn duplicates_are_harmless() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "dup_actor").await;
    let org = organization(&pool, actor, "dup").await;
    outbox::register(&pool, &consume_a()).await.unwrap();
    let probes = Probes::new();

    // One committed event, relayed three times before the runner runs (a crash
    // after enqueue, a concurrent relay, a restart that re-reads the row).
    let event = commit_event(&pool, Some(org), actor, "test.dup", json!({})).await;
    for _ in 0..3 {
        relay_once(&state).await;
    }
    assert_eq!(
        delivery_count(&pool, CONSUMER_A).await,
        1,
        "(event, consumer) deduped"
    );
    let jobs_for_event: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'event_id' = $2",
    )
    .bind(JOB_A)
    .bind(event.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_for_event, 1, "one job, however often the relay ran");

    run_until_quiet(&state, &probes).await;
    assert_eq!(probes.a(), 1);

    // A redelivery: the relay runs again after the first job is done. The
    // deterministic key (`outbox-deliver:<event>:<consumer>`) is reused, and the pair
    // already has its delivery row, so nothing new is created.
    relay_once(&state).await;
    assert_eq!(
        delivery_count(&pool, CONSUMER_A).await,
        1,
        "no second delivery row"
    );
    let jobs_after: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(JOB_A)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs_after, 1, "no second job");
    assert_eq!(probes.a(), 1, "no second run");
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 4b. A crash between enqueue and the fan-out mark delivers the same job.
//     Revert: make the job key per event only and give two consumers the same
//     job kind (the second enqueue is absorbed), or drop the delivery-row
//     `ON CONFLICT DO NOTHING` and a restart creates a second delivery/job.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_crash_between_enqueue_and_the_mark_delivers_the_same_job() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "crash_actor").await;
    let org = organization(&pool, actor, "crash").await;
    outbox::register(&pool, &consume_a()).await.unwrap();
    outbox::register(&pool, &consume_b()).await.unwrap();
    let probes = Probes::new();

    let event = commit_event(&pool, Some(org), actor, "test.crash", json!({})).await;

    // The relay commits the first consumer's delivery and job, then the process
    // dies before it reaches the second consumer (limit 1 is that crash point).
    assert_eq!(
        outbox::fan_out_batch(&state, 1).await.unwrap(),
        1,
        "one delivery committed before the crash"
    );
    let first: (Uuid, Uuid, String, String) = sqlx::query_as(
        "SELECT d.consumer_id, d.job_id, d.job_key, c.name \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE d.event_id = $1",
    )
    .bind(event)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first.3, CONSUMER_A);
    let marked: bool =
        sqlx::query_scalar("SELECT delivered_at IS NOT NULL FROM outbox_events WHERE id = $1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!marked, "the fan-out mark did not commit before the crash");

    // Restart: the second consumer is delivered, the first is not duplicated.
    outbox::fan_out_batch(&state, 1000).await.unwrap();
    let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT d.consumer_id, d.job_id, d.job_key, c.name \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE d.event_id = $1 ORDER BY c.name",
    )
    .bind(event)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "one delivery per consumer, no duplicates");
    assert_eq!(
        rows[0], first,
        "the first consumer keeps the same delivery and job"
    );
    assert!(
        first.2.contains(&event.to_string()) && first.2.contains(&first.0.to_string()),
        "the key names the event and the consumer: {}",
        first.2
    );
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE payload->>'event_id' = $1 AND kind IN ($2, $3)",
    )
    .bind(event.to_string())
    .bind(JOB_A)
    .bind(JOB_B)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs, 2, "one job per (event, consumer)");

    run_until_quiet(&state, &probes).await;
    assert_eq!((probes.a(), probes.b()), (1, 1), "both consumers ran once");

    // A second crash, after the deliveries committed but before the event was
    // marked: the mark is lost, the relay re-reads the event, and no second
    // delivery or job appears. The event still reaches each consumer once.
    sqlx::query("UPDATE outbox_events SET delivered_at = NULL WHERE id = $1")
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(outbox::fan_out_batch(&state, 1000).await.unwrap(), 0);
    let jobs_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE payload->>'event_id' = $1 AND kind IN ($2, $3)",
    )
    .bind(event.to_string())
    .bind(JOB_A)
    .bind(JOB_B)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_after, 2, "the re-read created no second job");
    run_until_quiet(&state, &probes).await;
    assert_eq!(
        (probes.a(), probes.b()),
        (1, 1),
        "the re-read re-ran nothing"
    );
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 5. A consumer registered after the cutover receives nothing older.
//    Revert: drop the position join from `fan_out_batch` (or register the
//    consumer with `from_seq = 0`): it then receives the three older events.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_consumer_registered_later_receives_nothing_older() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "late_actor").await;
    let org = organization(&pool, actor, "late").await;
    outbox::register_builtin(&state).await.unwrap();

    let mut old = Vec::new();
    for i in 0..3 {
        old.push(commit_event(&pool, Some(org), actor, &format!("test.old.{i}"), json!({})).await);
    }
    // Whether the relay ran before or after registration, the late consumer
    // must not receive them.
    relay_once(&state).await;

    outbox::register(&pool, &consume_a()).await.unwrap();
    let position: i64 = sqlx::query_scalar(
        "SELECT p.from_seq FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id \
         WHERE c.name = $1 AND p.chain = $2",
    )
    .bind(CONSUMER_A)
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        position, 3,
        "registration position = the head at registration"
    );

    let new = commit_event(&pool, Some(org), actor, "test.new", json!({})).await;
    relay_once(&state).await;

    let received: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.event_id FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1 ORDER BY d.chain, d.seq",
    )
    .bind(CONSUMER_A)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(received, vec![new], "only the event after registration");
    for id in &old {
        assert!(
            !received.contains(id),
            "nothing older than the registration position"
        );
    }
    // The notification consumer was registered at the cutover, so it got all
    // four (none of the kinds is in its filter, so they have no deliveries:
    // it is the position, not the filter, that is being proved here).
    assert_eq!(delivery_count(&pool, "notification.outbox").await, 0);
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 5b. A chain created after registration is delivered from its own beginning.
//     Revert: let every `register` call draw a position for chains it has not
//     seen (the old INSERT ... ON CONFLICT DO NOTHING loop): re-registering
//     after the first event pins the new chain at its head and skips the
//     backlog, or an empty chain would replay from the start instead of its
//     own first event.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_chain_created_after_registration_starts_at_its_own_beginning() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "newchain_actor").await;
    // Register before any chain exists: there is nothing to draw a position on.
    outbox::register(&pool, &consume_a()).await.unwrap();
    let probes = Probes::new();

    let org = organization(&pool, actor, "newchain").await;
    let first = commit_event(&pool, Some(org), actor, "test.newchain.1", json!({})).await;
    let second = commit_event(&pool, Some(org), actor, "test.newchain.2", json!({})).await;

    // The relay re-registers on every pass. That must not pin the new chain at
    // its current head; it has no position, so the fan-out reads zero and
    // starts at the chain's own first event.
    outbox::register(&pool, &consume_a()).await.unwrap();
    let positions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id \
         WHERE c.name = $1 AND p.chain = $2",
    )
    .bind(CONSUMER_A)
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(positions, 0, "a new chain has no registration position");

    relay_once(&state).await;
    let received: Vec<(i64, Uuid)> = sqlx::query_as(
        "SELECT d.seq, d.event_id FROM outbox_deliveries d \
         JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1 ORDER BY d.seq",
    )
    .bind(CONSUMER_A)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        received,
        vec![(1, first), (2, second)],
        "the first two events of the new chain, not a truncated backlog"
    );
    run_until_quiet(&state, &probes).await;
    assert_eq!(probes.a(), 2);
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 6. An upgrade with an old backlog delivers nothing from before the cutover.
//    Revert: drop the watermark insert from `0050` (the consumers keep their
//    row but no position) and the relay starts at 0: the old backlog is
//    delivered.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_upgrade_with_an_old_backlog_delivers_nothing_from_before() {
    // The pre-upgrade schema: every migration before the fan-out ones, through
    // a migrator whose history holds exactly those.
    let db = Db::new(false).await;
    let pool = db.pool().await;
    let all = sqlx::migrate::Migrator::new(std::path::Path::new("./migrations"))
        .await
        .unwrap();
    let pre_upgrade = sqlx::migrate::Migrator {
        migrations: all.iter().filter(|m| m.version < 50).cloned().collect(),
        ..all
    };
    pre_upgrade.run(&pool).await.unwrap();
    let (actor, _) = account(&pool, "upgrade_actor").await;
    let org = organization(&pool, actor, "upgrade").await;

    // An old backlog: events the previous release wrote and nobody delivered.
    let mut backlog = Vec::new();
    for i in 0..3 {
        backlog.push(
            commit_event(
                &pool,
                Some(org),
                actor,
                "pull_request.merged",
                json!({"i": i}),
            )
            .await,
        );
    }
    let site = commit_event(&pool, None, actor, "pull_request.reviewed", json!({})).await;

    // Upgrade: the fan-out migrations, applied as `migrate` applies them.
    MIGRATOR.run(&pool).await.unwrap();
    let watermarks: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT p.chain, p.from_seq FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id \
         WHERE c.name = 'notification.outbox' ORDER BY p.chain",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        watermarks,
        vec![(Uuid::nil(), 1), (org, 3),],
        "one watermark per chain, at each chain's old head"
    );
    // The watermark is the chain's maximum assigned seq, not a count or a
    // global head: one row per chain, each at that chain's own head.
    let heads: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), \
                max(seq) \
         FROM audit_events GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        watermarks, heads,
        "the cutover watermark is each chain's maximum seq"
    );

    // The new release starts and relays: nothing from the backlog is fanned
    // out, even though all of it is a kind the consumer takes.
    let config = fast_config(&db.url);
    let (state, _) = state_for(&config).await;
    outbox::register_builtin(&state).await.unwrap();
    let after_builtin: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id WHERE c.name = 'notification.outbox'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        after_builtin, 2,
        "registration keeps the migration's positions"
    );
    // The built-in consumer's position is max(watermark, registration
    // position); see the assertion below, which makes a fresh head fail.
    let probes = Probes::new();
    assert_eq!(
        relay_once(&state).await,
        0,
        "the old backlog fans out to nobody"
    );
    assert_eq!(delivery_count(&pool, "notification.outbox").await, 0);
    run_until_quiet(&state, &probes).await;

    // An event after the upgrade does reach the consumer.
    let fresh = commit_event(&pool, Some(org), actor, "pull_request.merged", json!({})).await;
    // The server re-registers the built-in consumer every time it starts. Its
    // position is max(watermark, registration position): re-registering must
    // not draw a fresh head over the migration's watermark. Were it to, the
    // position would move to 4 and the event just committed at 4 would be
    // skipped, so this is asserted before and after an event that must arrive.
    outbox::register_builtin(&state).await.unwrap();
    let after_head: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT p.chain, p.from_seq FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id \
         WHERE c.name = 'notification.outbox' ORDER BY p.chain",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        after_head, watermarks,
        "re-registration keeps the watermark, not a fresh head"
    );
    relay_once(&state).await;
    let received: Vec<Uuid> = sqlx::query_scalar(
        "SELECT event_id FROM outbox_deliveries WHERE consumer_id = \
         (SELECT id FROM outbox_consumers WHERE name = 'notification.outbox')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(received, vec![fresh]);
    for id in backlog.iter().chain([&site]) {
        assert!(
            !received.contains(id),
            "pre-upgrade event {id} was never delivered"
        );
    }

    // A consumer registered after the upgrade starts where the chain is then,
    // not at 0: it receives only what is committed after it and nothing from
    // the old backlog, even though the backlog is all a kind it takes.
    outbox::register(&pool, &consume_a()).await.unwrap();
    let late_position: i64 = sqlx::query_scalar(
        "SELECT p.from_seq FROM outbox_consumer_positions p \
         JOIN outbox_consumers c ON c.id = p.consumer_id WHERE c.name = $1 AND p.chain = $2",
    )
    .bind(CONSUMER_A)
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        late_position, 4,
        "at the head the post-upgrade event took, not 0"
    );
    let after_late =
        commit_event(&pool, Some(org), actor, "pull_request.reviewed", json!({})).await;
    relay_once(&state).await;
    let late: Vec<Uuid> = sqlx::query_scalar(
        "SELECT d.event_id FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        late,
        vec![after_late],
        "the event after its registration and nothing from the old backlog"
    );
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 7. One event, one notification job; code owners stay a direct enqueue.
//    Revert: restore any of the three direct `jobs::enqueue` calls removed in
//    this slice (`pulls.rs` review and review-requested, `merge_intents.rs`
//    merged): the request path then writes a job beside the outbox row and the
//    "no job beside the outbox write" assertions fail.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn one_event_produces_one_notification_job_and_codeowners_stay_direct() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let app: Router = router(state.clone());
    let (author, author_token) = account(&pool, "notif_author").await;
    let (reviewer, _) = account(&pool, "notif_reviewer").await;
    let org = organization(&pool, author, "notif").await;
    // The owner's and the reviewer's grant rows, as creating an organization
    // and adding a member write them (the membership alone is not a grant;
    // `facts::snapshot` reads `grants`).
    for (account, role) in [(author, "owner"), (reviewer, "write")] {
        if role == "write" {
            sqlx::query(
                "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, 'member')",
            )
            .bind(org)
            .bind(account)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by) \
             VALUES ($1, $2, 'account', $3, 'organization', $4, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    }
    // A repository and an open pull request, written as the repository API
    // writes them (the Git-facing fixture lives in the other test files). One
    // audit row is the chain's first, so the chain row exists with a head; the
    // lock is released before the consumer registers, as it is in a server.
    let repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, \
                                   status, visibility) \
         VALUES ($1, $2, 'notif', $3, $4, 'active', 'private')",
    )
    .bind(repo_id)
    .bind(org)
    .bind(author)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    commit_event(
        &pool,
        Some(org),
        author,
        "repository.created",
        json!({"repository_id": repo_id}),
    )
    .await;
    let pull_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id, repository_id, number, author_id, title, body, \
             source_branch, target_branch, source_repository_id, state) \
         VALUES ($1, $2, 1, $3, 'one event', '', 'feature', 'main', $2, 'open')",
    )
    .bind(pull_id)
    .bind(repo_id)
    .bind(author)
    .execute(&pool)
    .await
    .unwrap();
    outbox::register_builtin(&state).await.unwrap();
    let probes = Probes::new();

    // 1. `pull_request.review_requested` through the real endpoint.
    let requests = "/api/v1/organizations/notif/repositories/notif/pulls/1/requested_reviewers";
    let (status, body) = call(
        &app,
        "POST",
        requests,
        &author_token,
        json!({"reviewer": "notif_reviewer"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM outbox_events WHERE kind = 'pull_request.review_requested' ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 1, "one event per request");
    relay_once(&state).await;
    relay_once(&state).await;
    let for_event: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'event_id' = $2",
    )
    .bind(githearth_server::notifications::OUTBOX_JOB_KIND)
    .bind(events[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(for_event, 1, "one event, one notification job");
    run_until_quiet(&state, &probes).await;
    let notified: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE account_id = $1")
            .bind(reviewer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notified, 1, "the reviewer is notified once");

    // 2. `pull_request.merged` through `record_merge_result`, the function
    // that used to enqueue the notification itself.
    let operation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
             source_oid, target_ref, expected_target_oid, method, policy_generation, \
             checks_approvals_digest, actor_id, state) \
         VALUES ($1, $2, $3, 'refs/heads/feature', $4, 'refs/heads/main', $4, 'merge', 0, '', $5, \
                 'executing')",
    )
    .bind(operation)
    .bind(repo_id)
    .bind(pull_id)
    .bind("a".repeat(40))
    .bind(reviewer)
    .execute(&pool)
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    githearth_server::merge_intents::record_merge_result(
        &mut conn,
        repo_id,
        1,
        operation,
        reviewer,
        &"a".repeat(40),
        &"b".repeat(40),
        "refs/heads/main",
        90,
    )
    .await
    .unwrap();
    drop(conn);
    let merged: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM outbox_events WHERE kind = 'pull_request.merged'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(merged.len(), 1);
    let merged_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE payload->>'event_id' = $1 AND kind <> 'codeowners.request_reviews'",
    )
    .bind(merged[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(merged_jobs, 0, "the merge path writes the event, not a job");
    relay_once(&state).await;
    let merged_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload->>'event_id' = $2",
    )
    .bind(githearth_server::notifications::OUTBOX_JOB_KIND)
    .bind(merged[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(merged_jobs, 1, "one merged event, one notification job");
    run_until_quiet(&state, &probes).await;
    let author_notified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND kind = 'pull_request.merged'",
    )
    .bind(author)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(author_notified, 1, "the author is told about the merge");
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 9. Dead letter and requeue.
//    Revert: drop the `sync_delivery` call after the outcome, or make `requeue`
//    reuse the old job key: the row never goes `dead`, or the requeued job is
//    absorbed by the finished job and the held event is never delivered.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_exhausted_delivery_is_dead_and_requeue_recovers_it() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "dead_actor").await;
    let org = organization(&pool, actor, "dead").await;
    // A two-attempt consumer, so the budget is exhausted quickly.
    let consumer = Consumer {
        max_attempts: 2,
        ..consume_a()
    };
    outbox::register(&pool, &consumer).await.unwrap();
    let probes = Probes::new();
    probes.a_fail.store(true, Ordering::SeqCst);

    let event = commit_event(&pool, Some(org), actor, "test.dead", json!({})).await;
    relay_once(&state).await;
    let (first_job, max_attempts): (Uuid, i32) =
        sqlx::query_as("SELECT id, max_attempts FROM jobs WHERE kind = $1")
            .bind(JOB_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        max_attempts, 2,
        "the job is enqueued with the delivery's budget"
    );

    for _ in 0..2 {
        jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string()]).await;
        run_for(&state, &probes, true, 400).await;
    }
    let (delivery_state, attempts, last_error): (String, i32, Option<String>) = sqlx::query_as(
        "SELECT d.state, d.attempts, d.last_error FROM outbox_deliveries d \
         JOIN outbox_consumers c ON c.id = d.consumer_id WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(delivery_state, "dead", "the budget is exhausted");
    assert_eq!(attempts, 2);
    assert!(last_error.is_some(), "the reason is kept");

    // `doctor` warns about it (read-only).
    let report = githearth_server::doctor::report(&config).await;
    let check = report
        .iter()
        .find(|c| c.name == "outbox")
        .expect("the outbox check exists");
    assert!(
        matches!(check.status, githearth_server::doctor::Status::Warn),
        "doctor warns: {}",
        check.detail
    );
    assert!(check.detail.contains("jobs requeue"), "{}", check.detail);

    // `jobs requeue`: a new job key, the row's own budget, the row pending.
    let before_key: String = sqlx::query_scalar(
        "SELECT d.job_key FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    let results = outbox::requeue(&pool, Some(CONSUMER_A), None, false)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(&results[0].2, RequeueOutcome::Requeued { .. }));
    let (after_key, requeued_job, requeued_max, requeued_state, requeued_attempts): (
        String,
        Uuid,
        i32,
        String,
        i32,
    ) = sqlx::query_as(
        "SELECT d.job_key, d.job_id, j.max_attempts, d.state, d.attempts \
         FROM outbox_deliveries d JOIN outbox_consumers c ON c.id = d.consumer_id \
         JOIN jobs j ON j.id = d.job_id WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(after_key, before_key, "a new job key");
    assert!(after_key.ends_with(":r1"), "{after_key}");
    assert_ne!(requeued_job, first_job, "a new job");
    assert_eq!(requeued_max, 2, "the delivery row's own budget");
    assert_eq!(requeued_state, "pending");
    assert_eq!(requeued_attempts, 0, "the budget starts again");

    probes.a_fail.store(false, Ordering::SeqCst);
    run_until_quiet(&state, &probes).await;
    assert_eq!(probes.a(), 1, "the held event is delivered once");
    let final_state: Vec<(String, i32)> = sqlx::query_as(
        "SELECT d.state, d.attempts FROM outbox_deliveries d \
         JOIN outbox_consumers c ON c.id = d.consumer_id WHERE c.name = $1 AND d.event_id = $2",
    )
    .bind(CONSUMER_A)
    .bind(event)
    .fetch_one(&pool)
    .await
    .map(|row| vec![row])
    .unwrap();
    assert_eq!(final_state, vec![("delivered".to_string(), 1)]);
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// 10. The cap the runner honours is the consumer's, not 600 s.
//     Revert: read a constant cap in `apply_outcome` instead of the job's
//     `backoff_cap_seconds` and the next attempt is 600 s away.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_runner_honours_the_consumers_backoff_cap() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let consumer = Consumer {
        backoff_cap: Duration::from_secs(7),
        ..consume_a()
    };
    outbox::register(&pool, &consumer).await.unwrap();
    let probes = Probes::new();
    probes.a_fail.store(true, Ordering::SeqCst);
    let (actor, _) = account(&pool, "cap_actor").await;
    let org = organization(&pool, actor, "cap").await;
    commit_event(&pool, Some(org), actor, "test.cap", json!({})).await;
    relay_once(&state).await;

    // Real backoff (no override), so the delay is the stored cap's.
    for _ in 0..4 {
        run_for(&state, &probes, false, 250).await;
        jobs::mark_due_outbox_jobs(&pool, &[JOB_A.to_string()]).await;
    }
    let next_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (d.next_attempt_at - now()))::float8 FROM outbox_deliveries d \
         JOIN outbox_consumers c ON c.id = d.consumer_id WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        next_in > 0.0 && next_in <= 7.0,
        "the consumer's 7 s cap, not the 600 s default: {next_in}"
    );
    finish(&config, pool, db).await;
}

// ---------------------------------------------------------------------------
// A consumer needs a handler (review F9). A consumer with no job kind would
// create deliveries that nothing enqueues, so its queue would grow unbounded
// and its events would never reach `delivered_at`. `register` refuses it, and
// the event is delivered once the consumer has a handler.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_consumer_without_a_handler_is_refused() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "nohandler_actor").await;
    let org = organization(&pool, actor, "nohandler").await;

    // `Consumer::new` requires a kind, so this is the only way to ask for one
    // without a handler.
    let mut bare = consume_a();
    bare.job_kind = None;
    let refused = outbox::register(&pool, &bare).await;
    assert!(
        refused.is_err(),
        "a consumer with no job kind must be refused"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_consumers WHERE name = $1")
        .bind(CONSUMER_A)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "the refused consumer was not written");

    // With its handler the same consumer registers and the event reaches a
    // delivered delivery row.
    outbox::register(&pool, &consume_a()).await.unwrap();
    commit_event(&pool, Some(org), actor, "test.handler", json!({})).await;
    relay_once(&state).await;
    let probes = Probes::new();
    run_until_quiet(&state, &probes).await;
    assert_eq!(probes.a(), 1, "the event reached the consumer's handler");
    let (state_col, delivered_at): (String, Option<String>) = sqlx::query_as(
        "SELECT d.state, to_char(e.delivered_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SSZ') \
         FROM outbox_deliveries d \
         JOIN outbox_events e ON e.id = d.event_id \
         JOIN outbox_consumers c ON c.id = d.consumer_id WHERE c.name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state_col, "delivered");
    assert!(delivered_at.is_some(), "the event's fan-out committed");
    finish(&config, pool, db).await;
}

/// The guard also has to hold for an **update**: `register` upserts, and a
/// consumer that is already registered with a handler must not be able to
/// null its own kind through `ON CONFLICT … job_kind = EXCLUDED.job_kind`.
/// Without the `job_kind.is_none()` refusal the row would keep its name and
/// budget but lose the handler, and its events would stop being enqueued.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_registered_consumer_cannot_lose_its_handler() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (_state, pool) = state_for(&config).await;

    outbox::register(&pool, &consume_a()).await.unwrap();
    let before: (Option<String>, i32, i32) = sqlx::query_as(
        "SELECT job_kind, max_attempts, backoff_cap_seconds FROM outbox_consumers WHERE name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before.0.as_deref(), Some(JOB_A));

    let mut bare = Consumer {
        kinds: Some(vec!["test.some-kind".to_string()]),
        max_attempts: 3,
        backoff_cap: Duration::from_secs(7),
        ..consume_a()
    };
    bare.job_kind = None;
    let refused = outbox::register(&pool, &bare).await;
    assert!(
        refused.is_err(),
        "a consumer already registered cannot drop its handler"
    );

    let after: (Option<String>, i32, i32) = sqlx::query_as(
        "SELECT job_kind, max_attempts, backoff_cap_seconds FROM outbox_consumers WHERE name = $1",
    )
    .bind(CONSUMER_A)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "the refused update wrote nothing: the live consumer keeps its kind and budget"
    );
    finish(&config, pool, db).await;
}

/// `doctor` must count a `pending` delivery. The count query used to carry
/// `WHERE state <> 'pending'`, so the `FILTER (WHERE state = 'pending')` count
/// was always zero: a stalled relay printed `0 pending` and the check passed
/// over the fault it exists to detect. Revert the filter and this test fails
/// with `0 pending`.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn doctor_counts_a_pending_delivery() {
    let db = Db::new(true).await;
    let config = fast_config(&db.url);
    let (state, pool) = state_for(&config).await;
    let (actor, _) = account(&pool, "pending_actor").await;
    let org = organization(&pool, actor, "pending").await;
    outbox::register(&pool, &consume_a()).await.unwrap();
    commit_event(&pool, Some(org), actor, "test.pending", json!({})).await;
    relay_once(&state).await;

    assert_eq!(
        delivery_states(&pool, CONSUMER_A).await,
        vec![("pending".to_string(), 0)],
        "the fan-out left the delivery pending"
    );

    // Read-only: no runner is started, so the row never leaves `pending`.
    let report = githearth_server::doctor::report(&config).await;
    let check = report
        .iter()
        .find(|c| c.name == "outbox")
        .expect("the outbox check exists");
    assert!(
        matches!(check.status, githearth_server::doctor::Status::Ok),
        "doctor passes and reports the backlog: {}",
        check.detail
    );
    assert!(
        check.detail.contains("1 pending"),
        "doctor counts the pending delivery: {}",
        check.detail
    );

    finish(&config, pool, db).await;
}
