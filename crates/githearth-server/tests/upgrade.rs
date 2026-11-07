//! Upgrade drill integration tests — `#[ignore]`.
//! Requires a disposable PostgreSQL database and the compiled binary.
use sqlx::PgPool;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use uuid::Uuid;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn nonce() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_githearth-server"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GITHEARTH_") {
            command.env_remove(name);
        }
    }
    command
}

fn run(mut command: Command) -> std::process::Output {
    command.stdin(Stdio::null());
    command.stderr(Stdio::piped());
    command.stdout(Stdio::piped());
    command.output().unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn temp_storage_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "githearth-upgrade-storage-{}-{}",
        std::process::id(),
        nonce()
    ))
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "githearth-upgrade-{}-{}-{name}",
        std::process::id(),
        nonce()
    ))
}

fn write_config(path: &PathBuf, database_url: &str, storage_root: &str) {
    let content = format!(
        "database_url = \"{database_url}\"\nstorage_root = \"{storage_root}\"\ndatabase_timeout_seconds = 5\n"
    );
    std::fs::write(path, content).unwrap();
}

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maint)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maint)
        .await
        .unwrap();
    format!("{origin}/{name}")
}

async fn bootstrap_admin(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let hash = githearth_server::hash_password("twelve-chars12").unwrap();
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin) VALUES ($1,$2,$3,true)",
    )
    .bind(id)
    .bind("admin")
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn query_admin_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE is_admin")
        .fetch_one(pool)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn previous_release_schema_upgrades_and_keeps_data() {
    let db_name = format!(
        "githearth_tupg_prev_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage_root = temp_storage_root();
    fs::create_dir_all(&storage_root).unwrap();
    let root_str = storage_root.to_str().unwrap();

    let config_path = temp_file("prev.toml");
    write_config(&config_path, &database_url, root_str);

    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();

    // Build "previous release" migrator: all but the newest migration
    let all_but_last: Vec<sqlx::migrate::Migration> = githearth_server::MIGRATOR
        .iter()
        .take(githearth_server::MIGRATOR.iter().count().saturating_sub(1))
        .cloned()
        .collect();
    let prev_migrator = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(all_but_last),
        ..githearth_server::MIGRATOR
    };
    prev_migrator.run(&pool).await.unwrap();

    // Create representative data at that schema
    bootstrap_admin(&pool).await;
    assert_eq!(query_admin_count(&pool).await, 1);

    pool.close().await;

    // serve must exit 1 with mismatch
    let mut serve_cmd = server();
    serve_cmd.args(["--config", config_path.to_str().unwrap(), "serve"]);
    serve_cmd.env("GITHEARTH_DATABASE_URL", &database_url);
    serve_cmd.env("GITHEARTH_STORAGE_ROOT", root_str);
    serve_cmd.stdin(Stdio::null());
    serve_cmd.stderr(Stdio::piped());
    serve_cmd.stdout(Stdio::piped());
    let serve_output = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::task::spawn_blocking(move || serve_cmd.output().unwrap())
            .await
            .unwrap()
    })
    .await
    .expect("serve did not exit within timeout");
    assert!(!serve_output.status.success());
    let serve_stderr = String::from_utf8(serve_output.stderr).unwrap();
    assert!(
        serve_stderr.contains("database schema does not match this binary"),
        "stderr: {serve_stderr}"
    );
    assert!(serve_stderr.contains("pending"), "stderr: {serve_stderr}");

    // doctor reports fail schema: 1 pending migrations
    let mut doctor_cmd = server();
    doctor_cmd.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    doctor_cmd.env("GITHEARTH_DATABASE_URL", &database_url);
    doctor_cmd.env("GITHEARTH_STORAGE_ROOT", root_str);
    let doctor_output = run(doctor_cmd);
    let doctor_out = stdout(&doctor_output);
    assert!(
        doctor_out.contains("fail schema: 1 pending migrations"),
        "doctor stdout: {doctor_out}"
    );

    // migrate
    let mut migrate_cmd = server();
    migrate_cmd.args(["--config", config_path.to_str().unwrap(), "migrate"]);
    migrate_cmd.env("GITHEARTH_DATABASE_URL", &database_url);
    migrate_cmd.env("GITHEARTH_STORAGE_ROOT", root_str);
    let migrate_output = run(migrate_cmd);
    assert!(
        migrate_output.status.success(),
        "migrate failed: {}",
        stderr(&migrate_output)
    );

    // doctor schema is ok
    let mut doctor2_cmd = server();
    doctor2_cmd.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    doctor2_cmd.env("GITHEARTH_DATABASE_URL", &database_url);
    doctor2_cmd.env("GITHEARTH_STORAGE_ROOT", root_str);
    let doctor2_output = run(doctor2_cmd);
    let doctor2_out = stdout(&doctor2_output);
    assert!(
        doctor2_out.contains("ok   schema:"),
        "doctor2 stdout: {doctor2_out}"
    );

    // data created before is still there
    let pool2 = sqlx::PgPool::connect(&database_url).await.unwrap();
    assert_eq!(query_admin_count(&pool2).await, 1);
    pool2.close().await;

    let _ = fs::remove_file(&config_path);
    let _ = fs::remove_dir_all(&storage_root);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn newer_schema_is_refused_by_older_binary() {
    let db_name = format!(
        "githearth_tupg_newer_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage_root = temp_storage_root();
    fs::create_dir_all(&storage_root).unwrap();
    let root_str = storage_root.to_str().unwrap();

    let config_path = temp_file("newer.toml");
    write_config(&config_path, &database_url, root_str);

    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    githearth_server::MIGRATOR.run(&pool).await.unwrap();

    // insert a fake row with a version above every embedded one
    let max_version: i64 = githearth_server::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(0);
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES ($1, 'future', now(), true, $2, 0)",
    )
    .bind(max_version + 1)
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let mut serve_cmd = server();
    serve_cmd.args(["--config", config_path.to_str().unwrap(), "serve"]);
    serve_cmd.env("GITHEARTH_DATABASE_URL", &database_url);
    serve_cmd.env("GITHEARTH_STORAGE_ROOT", root_str);
    serve_cmd.stdin(Stdio::null());
    serve_cmd.stderr(Stdio::piped());
    serve_cmd.stdout(Stdio::piped());
    let serve_output = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::task::spawn_blocking(move || serve_cmd.output().unwrap())
            .await
            .unwrap()
    })
    .await
    .expect("serve did not exit within timeout");
    assert!(!serve_output.status.success());
    let serve_stderr = String::from_utf8(serve_output.stderr).unwrap();
    assert!(
        serve_stderr.contains("database schema does not match this binary"),
        "stderr: {serve_stderr}"
    );
    assert!(
        serve_stderr.contains("newer than this binary"),
        "stderr: {serve_stderr}"
    );

    let _ = fs::remove_file(&config_path);
    let _ = fs::remove_dir_all(&storage_root);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn serve_starts_on_a_matching_schema() {
    let db_name = format!(
        "githearth_tupg_match_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let storage_root = temp_storage_root();
    fs::create_dir_all(&storage_root).unwrap();
    let root_str = storage_root.to_str().unwrap();

    let config_path = temp_file("match.toml");
    write_config(&config_path, &database_url, root_str);

    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    githearth_server::MIGRATOR.run(&pool).await.unwrap();
    pool.close().await;

    let exe = env!("CARGO_BIN_EXE_githearth-server");
    let mut child = tokio::process::Command::new(exe)
        .args(["--config", config_path.to_str().unwrap(), "serve"])
        .env("GITHEARTH_DATABASE_URL", &database_url)
        .env("GITHEARTH_STORAGE_ROOT", root_str)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let child_exit = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;

    match child_exit {
        Err(_elapsed) => {
            // serve is still running — good. Kill it.
            child.kill().await.ok();
            let _ = child.wait().await;
        }
        Ok(Ok(status)) => {
            panic!("serve exited unexpectedly with status {status:?} — schema should match");
        }
        Ok(Err(e)) => {
            panic!("serve process error: {e}");
        }
    }

    let _ = fs::remove_file(&config_path);
    let _ = fs::remove_dir_all(&storage_root);
}
