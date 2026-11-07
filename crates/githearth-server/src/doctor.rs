use crate::config::Config;
use sqlx::PgPool;
use std::path::Path;

pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

const OK: &str = "ok  ";
const WARN: &str = "warn";
const FAIL: &str = "fail";
const SKIP: &str = "skip";

pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl std::fmt::Display) -> Self {
        let s = format!("{OK} {name}: {detail}");
        Self {
            status: Status::Ok,
            name: name.into(),
            detail: s,
        }
    }
    fn warn(name: &str, detail: impl std::fmt::Display) -> Self {
        let s = format!("{WARN} {name}: {detail}");
        Self {
            status: Status::Warn,
            name: name.into(),
            detail: s,
        }
    }
    fn fail(name: &str, detail: impl std::fmt::Display) -> Self {
        let s = format!("{FAIL} {name}: {detail}");
        Self {
            status: Status::Fail,
            name: name.into(),
            detail: s,
        }
    }
    fn skip(name: &str) -> Self {
        let s = format!("{SKIP} {name}: database unavailable");
        Self {
            status: Status::Skip,
            name: name.into(),
            detail: s,
        }
    }
}

pub async fn report(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();

    check_git(&mut checks);

    check_storage(config, &mut checks);

    check_disk(config, &mut checks);

    match db_connect(config).await {
        Ok(pool) => {
            checks.push(Check::ok("database", "connected"));
            let schema = schema_check(&pool).await;
            checks.push(match schema {
                Ok(d) => Check::ok("schema", d),
                Err(d) => Check::fail("schema", d),
            });
            checks.push(
                match lifecycle_check(&pool, config.reconcile_grace_seconds).await {
                    Ok(0) => Check::ok("lifecycle", "no stuck repositories"),
                    Ok(n) => Check::warn(
                        "lifecycle",
                        format_args!("reconciler will retry {} repo(s)", n),
                    ),
                    Err(d) => Check::fail("lifecycle", d),
                },
            );
            pool.close().await;
        }
        Err(e) => {
            checks.push(Check::fail("database", e));
            checks.push(Check::skip("schema"));
            checks.push(Check::skip("lifecycle"));
        }
    }

    checks
}

fn check_git(checks: &mut Vec<Check>) {
    match githearth_git::layout::check_git_version() {
        Ok(version) => {
            checks.push(Check::ok(
                "git",
                format!("{}.{}", version.major, version.minor),
            ));
        }
        Err(e) => {
            checks.push(Check::fail("git", &e));
        }
    }
}

fn check_storage(config: &Config, checks: &mut Vec<Check>) {
    use std::os::unix::fs::PermissionsExt;
    let root = &config.storage_root;
    let shown = root.display();
    checks.push(match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check::warn(
            "storage",
            format_args!("{shown} does not exist; will be created by serve"),
        ),
        Err(e) => Check::fail("storage", format_args!("{shown}: {e}")),
        Ok(m) if m.file_type().is_symlink() => {
            Check::fail("storage", format_args!("{shown} is a symlink"))
        }
        Ok(m) if !m.is_dir() => Check::fail("storage", format_args!("{shown} is not a directory")),
        Ok(m) if m.permissions().mode() & 0o002 != 0 => {
            Check::fail("storage", format_args!("{shown} is world-writable"))
        }
        Ok(_) => match rustix::fs::access(root, rustix::fs::Access::WRITE_OK) {
            Ok(()) => Check::ok("storage", format_args!("{shown} writable")),
            Err(_) => Check::fail(
                "storage",
                format_args!("{shown} not writable by this process"),
            ),
        },
    });

    // serve creates these subdirectories; when the root exists, check each that also exists.
    for sub in ["repositories", "tmp", "trash", "orphans"] {
        let subpath = root.join(sub);
        let shown = subpath.display();
        let check = match std::fs::symlink_metadata(&subpath) {
            // Missing subdirectories are created by serve.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => Check::fail("storage", format_args!("{shown}: {e}")),
            Ok(m) if m.file_type().is_symlink() => {
                Check::fail("storage", format_args!("{shown} is a symlink"))
            }
            Ok(m) if !m.is_dir() => {
                Check::fail("storage", format_args!("{shown} is not a directory"))
            }
            Ok(_) => match rustix::fs::access(&subpath, rustix::fs::Access::WRITE_OK) {
                Ok(()) => continue,
                Err(_) => Check::fail(
                    "storage",
                    format_args!("{shown} not writable by this process"),
                ),
            },
        };
        checks.push(check);
    }
}

/// Returns `(warn, free_bytes)`. Uses f_frsize as the unit; falls back to
/// f_bsize only when f_frsize is 0.  `push_limit` is `git_push_limit_bytes`.
fn disk_status(frsize: u64, bsize: u64, bavail: u64, blocks: u64, push_limit: u64) -> (bool, u64) {
    let unit = if frsize == 0 { bsize } else { frsize };
    let free = bavail * unit;
    let total = blocks * unit;
    let warn = free * 10 < total || free < 2 * push_limit;
    (warn, free)
}

fn check_disk(config: &Config, checks: &mut Vec<Check>) {
    let path = config
        .storage_root
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("."));
    checks.push(match rustix::fs::statvfs(path) {
        Ok(stat) => {
            let (warn, free) = disk_status(
                stat.f_frsize,
                stat.f_bsize,
                stat.f_bavail,
                stat.f_blocks,
                config.git_push_limit_bytes as u64,
            );
            let free_gib = format!("{:.2} GiB free", free as f64 / f64::from(1u32 << 30));
            if warn {
                Check::warn("disk", free_gib)
            } else {
                Check::ok("disk", free_gib)
            }
        }
        Err(e) => Check::fail("disk", format_args!("{}: {e}", path.display())),
    });
}

async fn db_connect(config: &Config) -> anyhow::Result<PgPool> {
    let mut c = config.clone();
    c.database_max_connections = 1;
    c.database_timeout_seconds = c.database_timeout_seconds.min(5);
    crate::connect(&c).await
}

async fn schema_check(pool: &PgPool) -> Result<String, String> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await
    .map_err(|_| "cannot read migration history".to_string())?;

    if !table_exists {
        return Err("database not initialized; run migrate".to_string());
    }

    let rows: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(|_| "cannot read migration history".to_string())?;

    let migrations: Vec<_> = crate::MIGRATOR.iter().collect();

    for (version, success, _) in &rows {
        if !success {
            return Err(format!("migration {version} failed"));
        }
    }

    let binary_versions: std::collections::BTreeSet<i64> =
        migrations.iter().map(|m| m.version).collect();
    for (version, _, _) in &rows {
        if !binary_versions.contains(version) {
            return Err(format!(
                "database schema is newer than this binary (version {version})"
            ));
        }
    }

    let applied_versions: std::collections::BTreeSet<i64> =
        rows.iter().map(|(v, _, _)| *v).collect();
    let pending = migrations
        .iter()
        .filter(|m| !applied_versions.contains(&m.version))
        .count();
    if pending > 0 {
        return Err(format!(
            "{pending} pending migrations; run `githearth-server migrate`"
        ));
    }

    let checksum_map: std::collections::BTreeMap<i64, &[u8]> =
        rows.iter().map(|(v, _, c)| (*v, c.as_slice())).collect();
    for m in &migrations {
        if let Some(applied_checksum) = checksum_map.get(&m.version)
            && *applied_checksum != m.checksum.as_ref()
        {
            return Err(format!(
                "migration {} was changed after it was applied",
                m.version
            ));
        }
    }

    let latest = applied_versions.last().copied().unwrap_or(0);
    Ok(format!("latest v{latest}"))
}

async fn lifecycle_check(pool: &PgPool, grace_seconds: u64) -> Result<i64, String> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM repositories \
         WHERE status IN ('creating','deleting') \
         AND updated_at < now() - make_interval(secs => $1)",
    )
    .bind(grace_seconds as f64)
    .fetch_one(pool)
    .await
    .map_err(|_| "cannot query repositories".to_string())
}

pub async fn run(config: &Config) -> i32 {
    let checks = report(config).await;
    let mut failed = 0usize;
    let mut warnings = 0usize;
    let mut skipped = 0usize;
    for check in &checks {
        println!("{}", check.detail);
        match check.status {
            Status::Fail => failed += 1,
            Status::Warn => warnings += 1,
            Status::Skip => skipped += 1,
            _ => {}
        }
    }
    let summary = format!(
        "{} checks, {} failed, {} warnings, {} skipped",
        checks.len(),
        failed,
        warnings,
        skipped
    );
    println!("{summary}");
    if failed > 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use sqlx::ConnectOptions;
    use std::str::FromStr;
    use uuid::Uuid;

    fn test_config(storage_root: &std::path::Path) -> Config {
        Config {
            database_url: "postgres://localhost/test".to_string(),
            storage_root: storage_root.to_path_buf(),
            ..Config::default()
        }
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()))
    }

    // ---- disk_status pure-fn tests ----

    #[test]
    fn disk_status_normal() {
        let (warn, free) = disk_status(4096, 0, 1_000_000, 1_000_000, 1_073_741_824);
        assert!(!warn);
        assert_eq!(free, 1_000_000 * 4096);
    }

    #[test]
    fn disk_status_frsize_zero_falls_back_to_bsize() {
        let (warn, free) = disk_status(0, 4096, 1_000_000, 1_000_000, 0);
        assert!(!warn);
        assert_eq!(free, 1_000_000 * 4096);
    }

    #[test]
    fn disk_status_frsize_differs_from_bsize() {
        let frsize = 1_048_576; // 1 MiB
        let bsize = 4096;
        let (warn, free) = disk_status(frsize, bsize, 10_000, 100_000, 1_073_741_824);
        assert!(!warn);
        assert_eq!(free, 10_000 * frsize);
    }

    #[test]
    fn disk_status_below_10_percent_warns() {
        // total=100*block, free=9*block → ~9.0% → warn
        let (warn, _free) = disk_status(4096, 0, 9, 100, 0);
        assert!(warn);
    }

    #[test]
    fn disk_status_below_2x_push_limit_warns() {
        // plenty of free percent, but free < 2 * push_limit
        let push_limit = 100_000;
        let (warn, _free) = disk_status(4096, 0, 20, 1, push_limit); // 20*4096 = 81920 < 200_000
        assert!(warn);
    }

    // ---- storage tests ----

    #[test]
    fn storage_missing_warns() {
        let dir = temp_dir("doctor-storage-missing");
        let config = test_config(&dir);
        let mut checks = Vec::new();
        check_storage(&config, &mut checks);
        assert!(
            matches!(checks[0].status, Status::Warn),
            "{:?}",
            checks[0].detail
        );
        assert!(
            checks[0]
                .detail
                .contains("does not exist; will be created by serve")
        );
    }

    #[test]
    fn storage_file_not_dir() {
        let dir = temp_dir("doctor-storage-file");
        std::fs::write(&dir, "not a dir").unwrap();
        let config = test_config(&dir);
        let mut checks = Vec::new();
        check_storage(&config, &mut checks);
        assert!(
            matches!(checks[0].status, Status::Fail),
            "{:?}",
            checks[0].detail
        );
        assert!(checks[0].detail.contains("is not a directory"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn storage_symlink_fails() {
        let dir = temp_dir("doctor-storage-symlink");
        let target = temp_dir("doctor-storage-symlink-target");
        std::fs::create_dir_all(&target).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &dir).unwrap();
            let config = test_config(&dir);
            let mut checks = Vec::new();
            check_storage(&config, &mut checks);
            assert!(matches!(checks[0].status, Status::Fail));
            assert!(checks[0].detail.contains("is a symlink"));
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn storage_world_writable_fails() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("doctor-storage-ww");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            let config = test_config(&dir);
            let mut checks = Vec::new();
            check_storage(&config, &mut checks);
            assert!(
                matches!(checks[0].status, Status::Fail),
                "{:?}",
                checks[0].detail
            );
            assert!(checks[0].detail.contains("world-writable"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn storage_good_ok() {
        let dir = temp_dir("doctor-storage-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config(&dir);
        let mut checks = Vec::new();
        check_storage(&config, &mut checks);
        assert!(
            matches!(checks[0].status, Status::Ok),
            "{:?}",
            checks[0].detail
        );
        assert!(checks[0].detail.contains("writable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_not_writable_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("doctor-storage-ro");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut checks = Vec::new();
        check_storage(&test_config(&dir), &mut checks);
        if !rustix::process::geteuid().is_root() {
            assert!(
                checks[0].detail.contains("not writable by this process"),
                "{}",
                checks[0].detail
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_subdir_symlink_fails() {
        let dir = temp_dir("doctor-subdir-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let sub = dir.join("repositories");
        let target = temp_dir("doctor-subdir-symlink-target");
        std::fs::create_dir_all(&target).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &sub).unwrap();
            let mut checks = Vec::new();
            check_storage(&test_config(&dir), &mut checks);
            let fail = checks
                .iter()
                .find(|c| c.detail.contains("repositories") && c.detail.contains("symlink"));
            assert!(
                fail.is_some(),
                "{:?}",
                checks.iter().map(|c| &c.detail).collect::<Vec<_>>()
            );
            // A dangling symlink is still reported, not skipped as missing.
            std::os::unix::fs::symlink(dir.join("gone"), dir.join("trash")).unwrap();
            let mut checks = Vec::new();
            check_storage(&test_config(&dir), &mut checks);
            assert!(
                checks
                    .iter()
                    .any(|c| c.detail.contains("trash is a symlink"))
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn storage_subdir_missing_is_ok() {
        let dir = temp_dir("doctor-subdir-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config(&dir);
        let mut checks = Vec::new();
        check_storage(&config, &mut checks);
        for c in &checks {
            let d = &c.detail;
            assert!(!d.contains("repositories") || d.contains("writable"), "{d}");
            assert!(!d.contains("/tmp ") || d.contains("writable"), "{d}");
            assert!(!d.contains("/trash ") || d.contains("writable"), "{d}");
            assert!(!d.contains("/orphans ") || d.contains("writable"), "{d}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_subdir_good() {
        let dir = temp_dir("doctor-subdir-ok");
        std::fs::create_dir_all(&dir).unwrap();
        for sub in ["repositories", "tmp", "trash", "orphans"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let mut checks = Vec::new();
        check_storage(&test_config(&dir), &mut checks);
        // root ok, no subdir fails
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, Status::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- git ----

    #[test]
    fn git_version_check() {
        let mut checks = Vec::new();
        check_git(&mut checks);
        assert!(matches!(checks[0].status, Status::Ok));
        assert!(checks[0].detail.contains("ok   git: "));
    }

    // ---- disk integration test ----

    #[test]
    fn disk_check_ok() {
        let dir = temp_dir("doctor-disk-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config(&dir);
        let mut checks = Vec::new();
        check_disk(&config, &mut checks);
        assert!(matches!(checks[0].status, Status::Ok) || matches!(checks[0].status, Status::Warn));
        assert!(checks[0].detail.contains("GiB free"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_of_missing_root_uses_nearest_existing_ancestor() {
        for root in [
            temp_dir("doctor-disk-missing").join("a/b"),
            "relative-missing/x".into(),
        ] {
            let mut checks = Vec::new();
            check_disk(&test_config(&root), &mut checks);
            assert!(
                checks[0].detail.contains("GiB free"),
                "{}",
                checks[0].detail
            );
        }
    }

    #[test]
    fn disk_warns_when_push_limit_exceeds_free_space() {
        let config = Config {
            git_push_limit_bytes: usize::MAX / 4,
            ..test_config(&std::env::temp_dir())
        };
        let mut checks = Vec::new();
        check_disk(&config, &mut checks);
        assert!(
            matches!(checks[0].status, Status::Warn),
            "{}",
            checks[0].detail
        );
    }

    // ---- DB tests ----

    async fn db_pool() -> (sqlx::PgPool, String) {
        let base =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let schema = format!("doctor_test_{}", Uuid::new_v4().simple());
        let options = sqlx::postgres::PgConnectOptions::from_str(&base)
            .unwrap()
            .disable_statement_logging();
        let schema_clone = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let s = schema_clone.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {s}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();
        (pool, schema)
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_fresh_migrated_ok() {
        let (pool, _schema) = db_pool().await;
        let result = schema_check(&pool).await;
        assert!(result.is_ok(), "{:?}", result);
        assert!(result.unwrap().starts_with("latest v"));
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_not_initialized() {
        let base =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let schema = format!("doctor_empty_{}", Uuid::new_v4().simple());
        let options = sqlx::postgres::PgConnectOptions::from_str(&base)
            .unwrap()
            .disable_statement_logging();
        let schema_clone = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let s = schema_clone.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {s}"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        let result = schema_check(&pool).await;
        assert!(result.is_err(), "{:?}", result);
        assert_eq!(result.unwrap_err(), "database not initialized; run migrate");
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_newer_than_binary() {
        let (pool, _schema) = db_pool().await;
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (99999, 'future', now(), true, $1, 0)")
            .bind(vec![0u8; 32])
            .execute(&pool)
            .await
            .unwrap();
        let result = schema_check(&pool).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("newer than this binary"));
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_checksum_mismatch() {
        let (pool, _schema) = db_pool().await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
            .bind(vec![0u8; 32])
            .execute(&pool)
            .await
            .unwrap();
        let result = schema_check(&pool).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("was changed after it was applied")
        );
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_migration_failed_row() {
        let (pool, _schema) = db_pool().await;
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (99998, 'failed', now(), false, $1, 0)")
            .bind(vec![0u8; 32])
            .execute(&pool)
            .await
            .unwrap();
        let result = schema_check(&pool).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed"));
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn schema_pending_migrations() {
        let (pool, _schema) = db_pool().await;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
            .execute(&pool)
            .await
            .unwrap();
        let result = schema_check(&pool).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("pending migrations"));
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn lifecycle_no_stuck() {
        let (pool, _schema) = db_pool().await;
        let result = lifecycle_check(&pool, 300).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
        pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn lifecycle_stuck_deleting() {
        let (pool, _schema) = db_pool().await;
        let account_id = Uuid::new_v4();
        let org_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO accounts (id, username, password_hash) VALUES ($1, 'test', 'hash')",
        )
        .bind(account_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, 'org', 'Org')")
            .bind(org_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, description, updated_at) \
             VALUES ($1, $2, 'repo', $3, $4, 'deleting', 'private', '', now() - interval '10 minutes')",
        )
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(account_id)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        let result = lifecycle_check(&pool, 300).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1);
        pool.close().await;
    }

    #[tokio::test]
    async fn database_unreachable_fails_and_skips_schema_lifecycle() {
        let dir = temp_dir("doctor-db-down");
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            database_url: "postgres://user:supersecret@127.0.0.1:1/db".to_string(),
            database_timeout_seconds: 1,
            storage_root: dir.clone(),
            ..Config::default()
        };
        let lines: Vec<String> = report(&config)
            .await
            .into_iter()
            .map(|c| c.detail)
            .collect();
        assert!(lines.contains(&"fail database: cannot connect to PostgreSQL; check database availability and credentials".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"skip schema: database unavailable".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"skip lifecycle: database unavailable".to_string()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("supersecret") && !l.contains("127.0.0.1:1"))
        );
        let code = run(&config).await;
        assert_eq!(
            code, 1,
            "exit code should be 1 for failed database; but database fails, schema/lifecycle skip, failed count = 1"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
