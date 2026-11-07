use githearth_core::RepositoryStatus;
use githearth_server::config::Config;
use githearth_server::reconcile::{self, Summary};
use githearth_server::{AppState, MIGRATOR, repositories};
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::Duration;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

/// Create a dedicated test database, run migrations, and return it together
/// with a cleanup handle.  Each test gets its own database so assertions on
/// global Summary counts are deterministic.
struct TestDb {
    db_name: String,
    pool: PgPool,
    /// Full connection URL for the test database.
    url: String,
    /// Connection options for the admin database (used for DROP DATABASE).
    admin_opts: PgConnectOptions,
}

impl TestDb {
    async fn with_max_connections(max_connections: u32) -> Self {
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let db_name = format!("githearth_test_{}", Uuid::new_v4().simple());

        // Connect to the base database to CREATE DATABASE.
        let admin_opts = PgConnectOptions::from_str(&base_url)
            .unwrap()
            .disable_statement_logging();
        let mut admin_conn = admin_opts.clone().connect().await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{}\"", db_name))
            .execute(&mut admin_conn)
            .await
            .unwrap();
        admin_conn.close().await.unwrap();

        // Build the URL for the new database by replacing the db name in the
        // base URL (everything after the last '/').
        let test_url = replace_db_name(&base_url, &db_name);

        let test_opts = PgConnectOptions::from_str(&test_url)
            .unwrap()
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect_with(test_opts)
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        Self {
            db_name,
            pool,
            url: test_url,
            admin_opts,
        }
    }

    async fn drop_db(self) {
        // close() waits for checked-out connections, and some tests still hold
        // one here; DROP ... WITH (FORCE) disconnects whatever remains.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.pool.close()).await;
        let mut admin_conn = self.admin_opts.connect().await.unwrap();
        let _ = sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.db_name))
            .execute(&mut admin_conn)
            .await;
    }
}

/// Replace the database name in a PostgreSQL URL (the segment after the last '/').
fn replace_db_name(url: &str, new_db: &str) -> String {
    if let Some(pos) = url.rfind('/') {
        let (base, _old_db) = url.split_at(pos);
        format!("{base}/{new_db}")
    } else {
        format!("{url}/{new_db}")
    }
}

struct TestEnv {
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    owner: Uuid,
    org: Uuid,
    db: TestDb,
}

/// Build a Config with the given grace and a unique storage root.
fn test_config_with_pool(
    db_url: &str,
    storage_root: &Path,
    grace_seconds: u64,
    max_connections: u32,
) -> Config {
    Config {
        database_url: db_url.to_owned(),
        database_max_connections: max_connections,
        database_timeout_seconds: 1,
        storage_root: storage_root.to_path_buf(),
        reconcile_interval_seconds: 60,
        reconcile_grace_seconds: grace_seconds,
        trash_retention_seconds: 604_800,
        ..Config::default()
    }
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

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Org')")
        .bind(id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// Backdate `updated_at` so the row is older than the grace period.
async fn backdate(pool: &PgPool, repo_id: Uuid) {
    sqlx::query("UPDATE repositories SET updated_at = now() - interval '1 hour' WHERE id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn setup(grace_seconds: u64) -> TestEnv {
    setup_with_pool(grace_seconds, 4).await
}

async fn setup_with_pool(grace_seconds: u64, max_connections: u32) -> TestEnv {
    let db = TestDb::with_max_connections(max_connections).await;
    let db_url = db.url.clone();
    let storage_root = std::env::temp_dir().join(format!("githearth-reconcile-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let canonical = std::fs::canonicalize(&storage_root).unwrap();
    let config = test_config_with_pool(&db_url, &canonical, grace_seconds, max_connections);
    let pool = db.pool.clone();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    TestEnv {
        state,
        pool,
        storage_root: canonical,
        owner,
        org,
        db,
    }
}

/// Build a [`TestEnv`] with a single-connection pool and a long DB timeout
/// so a lock-ordering deadlock shows as the test timeout (not a pool error).
async fn setup_single_conn(grace_seconds: u64, db_timeout_seconds: u64) -> TestEnv {
    let db = TestDb::with_max_connections(1).await;
    let db_url = db.url.clone();
    let storage_root = std::env::temp_dir().join(format!("githearth-reconcile-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let canonical = std::fs::canonicalize(&storage_root).unwrap();
    let config = Config {
        database_url: db_url.clone(),
        database_max_connections: 1,
        database_timeout_seconds: db_timeout_seconds,
        storage_root: canonical.clone(),
        reconcile_interval_seconds: 60,
        reconcile_grace_seconds: grace_seconds,
        trash_retention_seconds: 604_800,
        ..Config::default()
    };
    let pool = db.pool.clone();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    TestEnv {
        state,
        pool,
        storage_root: canonical,
        owner,
        org,
        db,
    }
}

async fn teardown(env: TestEnv) {
    let _ = std::fs::remove_dir_all(&env.storage_root);
    env.db.drop_db().await;
}

fn repo_path(storage_root: &Path, id: Uuid) -> PathBuf {
    let id_simple = id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_row_no_directory_becomes_active_and_repo_exists() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "reconcile-create",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    assert_eq!(row.status, RepositoryStatus::Creating);
    backdate(&env.pool, row.id).await;
    drop(conn);

    assert!(!repo_path(&env.storage_root, row.id).exists());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.activated, 1);
    assert_eq!(summary.errors, 0);

    let path = repo_path(&env.storage_root, row.id);
    assert!(path.is_dir());
    assert!(path.join("HEAD").is_file());

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Active);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_row_with_existing_directory_becomes_active() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "reconcile-existing",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();

    let storage = env.state.storage.clone();
    let repo_id = row.id;
    tokio::task::spawn_blocking(move || {
        storage.create(repo_id, op_id, githearth_git::layout::ObjectFormat::Sha1)
    })
    .await
    .unwrap()
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    assert!(repo_path(&env.storage_root, row.id).is_dir());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.activated, 1);

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Active);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleting_row_with_directory_trashed_and_deleted() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "reconcile-delete",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();

    let storage = env.state.storage.clone();
    let repo_id = row.id;
    tokio::task::spawn_blocking(move || {
        storage.create(repo_id, op_id, githearth_git::layout::ObjectFormat::Sha1)
    })
    .await
    .unwrap()
    .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Creating,
        RepositoryStatus::Active,
    )
    .await
    .unwrap();
    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Active,
        RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    assert!(repo_path(&env.storage_root, row.id).is_dir());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.deleted, 1);

    assert!(!repo_path(&env.storage_root, row.id).exists());

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Deleted);

    teardown(env).await;
}

/// A delete finished by the reconciler must revoke still-active grants, with
/// `revoked_by` naming the repository creator (the deleting actor is not
/// stored). Grants revoked before this run keep their original timestamps.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleting_row_with_active_grant_revoked_by_reconciler() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "reconcile-delete-grant",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    let repo_id = row.id;

    repositories::set_status(
        &mut conn,
        repo_id,
        RepositoryStatus::Creating,
        RepositoryStatus::Active,
    )
    .await
    .unwrap();

    // A subject the creator grants access to.
    let member = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
        .bind(member)
        .bind(tag("member"))
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(env.org)
    .bind(member)
    .execute(&mut *conn)
    .await
    .unwrap();

    // An active grant that the reconciler must revoke.
    let active_grant = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) \
         VALUES ($1,'read','account',$2,'repository',$3,$4,$5)",
    )
    .bind(active_grant)
    .bind(member)
    .bind(env.org)
    .bind(repo_id)
    .bind(env.owner)
    .execute(&mut *conn)
    .await
    .unwrap();

    // A grant already revoked before this run: its timestamps must be untouched.
    let revoked_grant = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by,revoked_at,revoked_by) \
         VALUES ($1,'read','account',$2,'repository',$3,$4,$5,clock_timestamp() - interval '1 hour',$6)",
    )
    .bind(revoked_grant)
    .bind(member)
    .bind(env.org)
    .bind(repo_id)
    .bind(env.owner)
    .bind(env.owner)
    .execute(&mut *conn)
    .await
    .unwrap();
    let revoked_before: String = sqlx::query_scalar(
        "SELECT to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM grants WHERE id = $1",
    )
    .bind(revoked_grant)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let revoked_by_before = env.owner;

    repositories::set_status(
        &mut conn,
        repo_id,
        RepositoryStatus::Active,
        RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    backdate(&env.pool, repo_id).await;
    drop(conn);

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.deleted, 1);

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, repo_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, RepositoryStatus::Deleted);

    let active_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE repository_id = $1 AND revoked_at IS NULL",
    )
    .bind(repo_id)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(active_count, 0, "no grant may stay active after deletion");

    let revoker: Uuid = sqlx::query_scalar("SELECT revoked_by FROM grants WHERE id = $1")
        .bind(active_grant)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(revoker, env.owner, "revoked_by must name the creator");

    let revoked_at_now: String = sqlx::query_scalar(
        "SELECT to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM grants WHERE id = $1",
    )
    .bind(revoked_grant)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let revoked_by_now: Uuid = sqlx::query_scalar("SELECT revoked_by FROM grants WHERE id = $1")
        .bind(revoked_grant)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(revoked_at_now, revoked_before);
    assert_eq!(revoked_by_now, revoked_by_before);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleting_row_without_directory_becomes_deleted() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "reconcile-deleting-nodir",
        env.owner,
        Uuid::new_v4(),
        "private",
        "",
    )
    .await
    .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Creating,
        RepositoryStatus::Active,
    )
    .await
    .unwrap();
    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Active,
        RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    assert!(!repo_path(&env.storage_root, row.id).exists());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.deleted, 1);

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Deleted);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn directory_with_no_row_moved_to_orphans() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it

    let orphan_id = Uuid::new_v4();
    let storage = env.state.storage.clone();
    let op_id = Uuid::new_v4();
    tokio::task::spawn_blocking(move || {
        storage.create(orphan_id, op_id, githearth_git::layout::ObjectFormat::Sha1)
    })
    .await
    .unwrap()
    .unwrap();

    assert!(repo_path(&env.storage_root, orphan_id).is_dir());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.orphaned, 1);

    assert!(!repo_path(&env.storage_root, orphan_id).exists());

    let orphans_dir = env.storage_root.join("orphans");
    let mut found = false;
    for entry in std::fs::read_dir(&orphans_dir).unwrap() {
        let name = entry.unwrap().file_name();
        if name
            .to_string_lossy()
            .starts_with(&orphan_id.simple().to_string())
        {
            found = true;
        }
    }
    assert!(found, "orphaned directory not found in orphans/");

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn directory_for_deleted_row_is_orphaned() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "deleted-orphan",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();

    let storage = env.state.storage.clone();
    let repo_id = row.id;
    tokio::task::spawn_blocking(move || {
        storage.create(repo_id, op_id, githearth_git::layout::ObjectFormat::Sha1)
    })
    .await
    .unwrap()
    .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Creating,
        RepositoryStatus::Active,
    )
    .await
    .unwrap();
    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Active,
        RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    repositories::set_status(
        &mut conn,
        row.id,
        RepositoryStatus::Deleting,
        RepositoryStatus::Deleted,
    )
    .await
    .unwrap();
    drop(conn);

    assert!(repo_path(&env.storage_root, row.id).is_dir());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.orphaned, 1);

    assert!(!repo_path(&env.storage_root, row.id).exists());

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_row_directory_not_orphaned() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "creating-not-orphan",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();

    let storage = env.state.storage.clone();
    let repo_id = row.id;
    tokio::task::spawn_blocking(move || {
        storage.create(repo_id, op_id, githearth_git::layout::ObjectFormat::Sha1)
    })
    .await
    .unwrap()
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    assert!(repo_path(&env.storage_root, row.id).is_dir());

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.activated, 1);
    assert_eq!(summary.orphaned, 0);

    assert!(repo_path(&env.storage_root, row.id).is_dir());

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_tmp_removed_live_tmp_kept() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let live_op = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn, env.org, "live-tmp", env.owner, live_op, "private", "",
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    // Manually create a tmp directory for the live operation.
    let tmp_dir = env
        .storage_root
        .join("tmp")
        .join(live_op.simple().to_string());
    std::fs::create_dir_all(&tmp_dir).unwrap();

    // Manually create a stale tmp directory (no corresponding creating row).
    let stale_op = Uuid::new_v4();
    let stale_dir = env
        .storage_root
        .join("tmp")
        .join(stale_op.simple().to_string());
    std::fs::create_dir_all(&stale_dir).unwrap();

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    // `storage.create` consumes the live tmp dir; only the stale one is cleaned.
    assert_eq!(summary.tmp_cleaned, 1);
    assert!(!stale_dir.exists(), "stale tmp entry must be removed");

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn two_phase_tmp_cleanup_does_not_race_with_concurrent_create() {
    // Deterministic regression test for the TOCTOU race in tmp cleanup.
    // The fix uses two-phase cleanup: list candidates FIRST, THEN query the
    // live set, THEN clean.  A directory created after listing must survive
    // even if the live-set query misses its row.
    let env = setup(30).await;

    // Phase 1: list tmp candidates (before any creating row exists).
    let storage = env.state.storage.clone();
    let candidates = tokio::task::spawn_blocking(move || storage.list_tmp())
        .await
        .unwrap()
        .unwrap();
    assert!(candidates.is_empty());

    // Now simulate the API create flow: insert a creating row, THEN create
    // tmp/<operation>.  This ordering matches the real API.
    let mut conn = env.pool.acquire().await.unwrap();
    let op_id = Uuid::new_v4();
    let _row = repositories::insert_creating(
        &mut conn,
        env.org,
        "race-test",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    drop(conn);

    // Create tmp/<operation> AFTER the candidate listing.
    let tmp_dir = env
        .storage_root
        .join("tmp")
        .join(op_id.simple().to_string());
    std::fs::create_dir_all(&tmp_dir).unwrap();
    assert!(tmp_dir.is_dir());

    // Phase 2: run the cleanup step with the *earlier* candidates (which did
    // NOT include this tmp entry).  The live-set query will see the creating
    // row, but the key invariant is that the directory is not a candidate.
    let mut conn = env.pool.acquire().await.unwrap();
    let cleaned = reconcile::clean_tmp_step(&mut conn, &env.state.storage, candidates)
        .await
        .unwrap();
    assert_eq!(cleaned, 0);
    assert!(
        tmp_dir.is_dir(),
        "tmp entry created after list_tmp must never be removed"
    );

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reconcile_pass_completes_with_single_connection_pool() {
    // Regression test: clean_tmp_step must not acquire a second pool
    // connection while reconcile_once already holds one.  With a pool of
    // size 1 any second acquire would time out.  We bound the whole pass
    // with tokio::time::timeout so a regression fails fast.
    let env = setup_with_pool(30, 1).await;

    // Create a stale tmp entry (no corresponding creating row → not live).
    let stale_op = Uuid::new_v4();
    let stale_tmp = env
        .storage_root
        .join("tmp")
        .join(stale_op.simple().to_string());
    std::fs::create_dir_all(&stale_tmp).unwrap();

    // Create a trash entry with timestamp 0 (epoch) so it is always expired.
    let trash_id = Uuid::new_v4();
    let stale_trash = env
        .storage_root
        .join("trash")
        .join(format!("{}.0.git", trash_id.simple()));
    std::fs::create_dir_all(&stale_trash).unwrap();

    let mut attempts = HashMap::new();
    let summary = tokio::time::timeout(
        Duration::from_secs(30),
        reconcile::reconcile_once(&env.state, &mut attempts),
    )
    .await
    .expect("reconcile_once timed out — likely a second pool acquire on a pool of size 1")
    .unwrap();

    assert_eq!(summary.tmp_cleaned, 1, "stale tmp entry must be cleaned");
    assert_eq!(
        summary.trash_purged, 1,
        "expired trash entry must be purged"
    );
    assert!(!stale_tmp.exists(), "stale tmp entry must be removed");
    assert!(!stale_trash.exists(), "expired trash entry must be removed");

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repeated_create_failure_becomes_failed_after_max_attempts() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "fail-create",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    // Place a regular file at the final path so `create` fails.
    let final_path = repo_path(&env.storage_root, row.id);
    std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    std::fs::write(&final_path, b"blocked").unwrap();

    let mut attempts: HashMap<Uuid, u32> = HashMap::new();

    // Passes 1–4: failures, not yet failed.
    for i in 1..=4 {
        let summary = reconcile::reconcile_once(&env.state, &mut attempts)
            .await
            .unwrap();
        assert_eq!(
            summary.failed, 0,
            "pass {i} should not mark failed, got {summary:?}"
        );
        assert_eq!(attempts.get(&row.id), Some(&i));
    }

    // Pass 5: marked failed, entry removed from map.
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(summary.failed, 1);
    assert!(
        !attempts.contains_key(&row.id),
        "attempts map must be empty after marking failed"
    );

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Failed);

    std::fs::remove_file(&final_path).unwrap();
    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn second_pass_is_noop() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "noop-repo",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    // First pass: activates the repo.
    let mut attempts = HashMap::new();
    let s1 = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(s1.activated, 1);

    // Second pass: nothing to do.
    let s2 = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert_eq!(s2, Summary::default());

    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Active);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn malformed_scan_entries_are_reported_but_not_touched() {
    let env = setup(30).await; // minimum valid grace; rows are backdated past it

    let malformed_dir = env.storage_root.join("repositories").join("ZZ");
    std::fs::create_dir_all(&malformed_dir).unwrap();
    std::fs::write(malformed_dir.join("not-a-repo.git"), b"junk").unwrap();

    let mut attempts = HashMap::new();
    let _summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();
    assert!(
        malformed_dir.exists(),
        "malformed entry must not be removed"
    );

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_row_inside_grace_period_not_touched() {
    // Large grace so the fresh row is not eligible.
    let env = setup(3600).await;
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "grace-create",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    // Do NOT backdate – updated_at is now, inside the 3600s grace.
    drop(conn);

    // Create a tmp directory for this operation.
    let tmp_dir = env
        .storage_root
        .join("tmp")
        .join(op_id.simple().to_string());
    std::fs::create_dir_all(&tmp_dir).unwrap();

    let mut attempts = HashMap::new();
    let summary = reconcile::reconcile_once(&env.state, &mut attempts)
        .await
        .unwrap();

    // Nothing should have happened.
    assert_eq!(summary.activated, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.tmp_cleaned, 0);
    assert_eq!(summary, Summary::default());

    // Row is still creating.
    let mut conn = env.pool.acquire().await.unwrap();
    let updated = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(updated.status, RepositoryStatus::Creating);

    // tmp directory for the in-flight operation survives clean_tmp.
    assert!(tmp_dir.is_dir(), "in-flight tmp directory must be kept");

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reconciler_skips_creating_when_row_flipped_under_lock() {
    // While the reconciler is blocked on the per-repository lock for a
    // `creating` row, another request flips the row to `deleting`.  When the
    // lock is released the reconciler must re-read the row under the lock and
    // skip the create — the directory must not exist afterwards.
    let env = setup(30).await;
    let mut conn = env.pool.acquire().await.unwrap();

    let op_id = Uuid::new_v4();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "flip-under-lock",
        env.owner,
        op_id,
        "private",
        "",
    )
    .await
    .unwrap();
    let repo_id = row.id;
    backdate(&env.pool, repo_id).await;
    drop(conn);

    let lock = env.state.storage.repository_lock(repo_id);

    // Hold the per-repo lock in a std::thread so the reconciler blocks.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (held_tx, held_rx) = mpsc::channel::<()>();

    let lock_clone = lock.clone();
    let holder = std::thread::spawn(move || {
        let _guard = lock_clone.blocking_lock_owned();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });

    // Wait until the lock is held.
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("lock holder did not acquire the lock");

    // Spawn the reconciler — it will block on `repository_lock(id).lock_owned().await`.
    let state = env.state.clone();
    let reconciler = tokio::spawn(async move {
        let mut attempts = HashMap::new();
        reconcile::reconcile_once(&state, &mut attempts).await
    });

    // Give the reconciler time to reach the lock acquisition point.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Flip the row to `deleting` while the reconciler is blocked.
    // The status trigger requires Creating → Active → Deleting.
    let mut conn = env.pool.acquire().await.unwrap();
    repositories::set_status(
        &mut conn,
        repo_id,
        RepositoryStatus::Creating,
        RepositoryStatus::Active,
    )
    .await
    .unwrap();
    repositories::set_status(
        &mut conn,
        repo_id,
        RepositoryStatus::Active,
        RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    drop(conn);

    // Release the lock so the reconciler can proceed.
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    // Wait for the reconciler to finish (with a bounded timeout — it must not
    // deadlock).
    let summary = tokio::time::timeout(Duration::from_secs(10), reconciler)
        .await
        .expect("reconciler timed out — likely deadlocked on the repository lock")
        .unwrap()
        .unwrap();

    // The reconciler must NOT have created the directory.
    assert!(
        !repo_path(&env.storage_root, repo_id).exists(),
        "reconciler must not create the directory when the row was flipped to deleting under the lock"
    );
    assert_eq!(
        summary.activated, 0,
        "reconciler must not activate a row that was flipped to deleting"
    );

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn single_connection_pool_no_lock_order_deadlock() {
    // With one pool connection, an API request that holds a repository lock
    // and then needs a connection must not wait on a reconciler pass that
    // holds the connection while waiting for that same lock.
    // The DB timeout (30 s) exceeds the test bound (10 s), so a deadlock
    // shows as a timeout rather than a pool error.
    let env = setup_single_conn(30, 30).await;
    let mut conn = env.pool.acquire().await.unwrap();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "lock-order",
        env.owner,
        Uuid::new_v4(),
        "private",
        "",
    )
    .await
    .unwrap();
    let repo_id = row.id;
    drop(conn);
    backdate(&env.pool, repo_id).await;

    // Play the API: hold the repository lock first.
    let api_guard = env
        .state
        .storage
        .repository_lock(repo_id)
        .lock_owned()
        .await;

    let state = env.state.clone();
    let reconciler = tokio::spawn(async move {
        let mut attempts = HashMap::new();
        reconcile::reconcile_once(&state, &mut attempts).await
    });

    // Wait until the pass has fetched its rows and is parked on the lock.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !reconciler.is_finished(),
        "pass must be waiting on the lock"
    );

    // Still holding the lock, the API now needs a connection.
    // A deadlock surfaces as this timeout or as the pool's own acquire timeout.
    let api_conn = tokio::time::timeout(Duration::from_secs(10), env.pool.acquire()).await;
    assert!(
        matches!(api_conn, Ok(Ok(_))),
        "lock-order deadlock: the pass holds the only connection while waiting for the lock"
    );
    drop(api_conn);
    drop(api_guard);

    let summary = tokio::time::timeout(Duration::from_secs(10), reconciler)
        .await
        .expect("reconciler did not finish after the lock was released")
        .unwrap()
        .unwrap();
    assert_eq!(summary.activated, 1);

    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn spawn_runs_initial_pass_and_logs_changes() {
    let env = setup(30).await;
    let mut conn = env.pool.acquire().await.unwrap();
    let row = repositories::insert_creating(
        &mut conn,
        env.org,
        "spawn-pass",
        env.owner,
        Uuid::new_v4(),
        "private",
        "",
    )
    .await
    .unwrap();
    backdate(&env.pool, row.id).await;
    drop(conn);

    let handle = reconcile::spawn(env.state.clone());
    // The initial pass activates the row; the interval tick then parks the loop.
    let activated = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut conn = env.pool.acquire().await.unwrap();
            let status: String = sqlx::query_scalar("SELECT status FROM repositories WHERE id=$1")
                .bind(row.id)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            drop(conn);
            if status == "active" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    handle.abort();
    let _ = handle.await;
    assert!(activated.is_ok(), "reconciler did not activate the row");
    teardown(env).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn spawn_logs_reconcile_pass_error() {
    // A lazy pool that never connects: the initial pass fails and is logged,
    // and the loop keeps going (aborted before the next tick).
    let storage_root =
        std::env::temp_dir().join(format!("githearth-reconcile-err-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: "postgres://localhost:1/unused".into(),
        database_max_connections: 1,
        database_timeout_seconds: 1,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy("postgres://localhost:1/unused")
        .unwrap();
    let state = AppState::new(pool, config).await.unwrap();
    let handle = reconcile::spawn(state);
    // The pass waits out the pool acquire timeout before it errors and logs.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    handle.abort();
    let _ = handle.await;
    let _ = std::fs::remove_dir_all(&storage_root);
}
