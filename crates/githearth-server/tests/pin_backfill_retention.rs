//! Migration 0058: repair the 0045 pull-request head backfill window.
//!
//! The installation is built the way U19's was: start at the schema before the
//! pin migrations (`0044`), populate pull requests and reviews through rows
//! that 0045 reads, then upgrade with the current migrator. 0045 writes its
//! fixed 30-day pins; 0058 must extend the live ones to the documented
//! retention and leave an already-expired pin expired.

use githearth_server::{MIGRATOR, pins};
use sqlx::PgPool;
use std::path::Path;
use uuid::Uuid;

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let maintenance = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maintenance)
        .await;
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maintenance)
        .await
        .unwrap();
    maintenance.close().await;
    format!("{origin}/{name}")
}

/// A migration history that stops at `max_version`. Copying the real files
/// keeps their checksums identical, so the full migrator continues from here.
fn migrations_through(max_version: i64) -> tempfile::TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let dir = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let version: i64 = name[..4].parse().unwrap();
        if version <= max_version {
            std::fs::copy(&path, dir.path().join(name)).unwrap();
        }
    }
    dir
}

async fn run_migrations(pool: &PgPool, dir: &Path) {
    sqlx::migrate::Migrator::new(dir)
        .await
        .unwrap()
        .run(pool)
        .await
        .unwrap();
}

fn oid(n: u8) -> String {
    format!("{n:040x}")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn upgraded_installation_keeps_backfilled_pins_for_the_documented_retention() {
    let database = fresh_database("githearth_pin_backfill").await;
    let pool = PgPool::connect(&database).await.unwrap();

    // 1. The installation before the pin migrations.
    let old = migrations_through(44);
    run_migrations(&pool, old.path()).await;
    let pins_exist: bool =
        sqlx::query_scalar("SELECT to_regclass('public.pinned_commits') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!pins_exist, "the old installation has no pin table");

    let account = Uuid::new_v4();
    let organization = Uuid::new_v4();
    let repository = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash) VALUES ($1, 'backfill-owner', 'x')",
    )
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organizations (id, slug, name) VALUES ($1, 'backfill-org', 'Backfill Org')",
    )
    .bind(organization)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO repositories \
         (id, organization_id, name, status, operation_id, created_by) \
         VALUES ($1, $2, 'backfill-repo', 'active', $3, $4)",
    )
    .bind(repository)
    .bind(organization)
    .bind(Uuid::new_v4())
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();

    // Three requests, one per state, each with a review at a distinct head.
    // 0045's lateral join reads the review commit and writes one backfill pin.
    let mut requests = Vec::new();
    for (number, state, closed_days_ago) in [
        (1_i32, "open", None),
        (2, "closed", Some(10_i32)),
        (3, "closed", Some(100)),
    ] {
        let pull = Uuid::new_v4();
        let head = oid(number as u8);
        sqlx::query(
            "INSERT INTO pull_requests \
             (id, repository_id, source_repository_id, number, title, author_id, \
              source_branch, target_branch, state, closed_at) \
             VALUES ($1, $2, $2, $3, $4, $5, $6, 'main', $7, \
                     CASE WHEN $8::int IS NULL THEN NULL \
                          ELSE now() - make_interval(days => $8::int) END)",
        )
        .bind(pull)
        .bind(repository)
        .bind(number)
        .bind(format!("request {number}"))
        .bind(account)
        .bind(format!("feature-{number}"))
        .bind(state)
        .bind(closed_days_ago)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pull_request_reviews \
             (id, pull_request_id, reviewer_id, verdict, commit_id) \
             VALUES ($1, $2, $3, 'commented', $4)",
        )
        .bind(Uuid::new_v4())
        .bind(pull)
        .bind(account)
        .bind(&head)
        .execute(&pool)
        .await
        .unwrap();
        requests.push((pull, head));
    }

    // 2. Upgrade through 0045..0057: the old backfill writes 30-day pins.
    let through_57 = migrations_through(57);
    run_migrations(&pool, through_57.path()).await;
    let backfilled: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pinned_commits WHERE reason = 'backfill'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(backfilled, 3, "0045 backfilled a head pin per request");
    let short: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pinned_commits \
         WHERE reason = 'backfill' \
           AND retain_until < created_at + interval '89 days'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(short, 3, "0045's pins carry the fixed 30-day window");

    // An already-expired backfill pin and a normal head pin, both written
    // after 0045: only the live backfill rows may move.
    let expired_head = oid(90);
    sqlx::query(
        "INSERT INTO pinned_commits \
         (repository_id, pull_request_id, commit_oid, reason, retain_until, created_at) \
         VALUES ($1, $2, $3, 'backfill', now() - interval '1 day', now() - interval '31 days')",
    )
    .bind(repository)
    .bind(requests[0].0)
    .bind(&expired_head)
    .execute(&pool)
    .await
    .unwrap();
    let normal_head = oid(91);
    sqlx::query(
        "INSERT INTO pinned_commits \
         (repository_id, pull_request_id, commit_oid, reason, retain_until) \
         VALUES ($1, $2, $3, 'pull_request_head', now() + interval '30 days')",
    )
    .bind(repository)
    .bind(requests[0].0)
    .bind(&normal_head)
    .execute(&pool)
    .await
    .unwrap();

    // 3. The current migrator applies 0058.
    MIGRATOR.run(&pool).await.unwrap();

    let repaired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pinned_commits \
         WHERE reason = 'backfill' AND pull_request_id IS NOT NULL \
           AND retain_until IS NOT NULL AND retain_until > now() \
           AND retain_until >= created_at + interval '89 days 23 hours'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        repaired, 3,
        "every still-live 0045 backfill got the documented 90-day window"
    );
    let (expired_until, expired_still_past): (String, bool) = sqlx::query_as(
        "SELECT retain_until::text, retain_until <= now() FROM pinned_commits \
         WHERE commit_oid = $1",
    )
    .bind(&expired_head)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        expired_still_past,
        "an already-expired pin is not resurrected (retain_until {expired_until})"
    );
    let normal_untouched: bool = sqlx::query_scalar(
        "SELECT retain_until = created_at + interval '30 days' FROM pinned_commits \
         WHERE commit_oid = $1",
    )
    .bind(&normal_head)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(normal_untouched, "non-backfill pins are untouched");

    // 4. The configured-retention pass extends a larger window and never
    // shortens the repaired one.
    let changed = pins::extend_backfilled_pins(&pool, 120).await.unwrap();
    assert_eq!(changed, 3, "the three repaired pins move to 120 days");
    let at_120: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pinned_commits \
         WHERE reason = 'backfill' AND pull_request_id IS NOT NULL \
           AND retain_until > now() \
           AND retain_until >= created_at + interval '119 days 23 hours'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(at_120, 3);
    let expired_after: bool = sqlx::query_scalar(
        "SELECT retain_until <= now() FROM pinned_commits WHERE commit_oid = $1",
    )
    .bind(&expired_head)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(expired_after, "the expired pin stays expired");
    let changed = pins::extend_backfilled_pins(&pool, 1).await.unwrap();
    assert_eq!(changed, 0, "a shorter configured window shortens nothing");

    pool.close().await;
}
