//! Secrets at rest (M4.2 7C, U28/U29 rotation half) against a real PostgreSQL.
//!
//! Every case creates its own database and storage root. The suite proves the
//! dump property, the archive property and the key's behaviour; it does not
//! inspect key material through any HTTP endpoint, because no endpoint returns
//! it.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use githearth_server::{
    AppState, MIGRATOR, audit_retention, backup, config::Config, connect, doctor, hash_password,
    router, secrets, ssh,
};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::{
    fs,
    io::Read,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "a sufficiently long password for secrets";
const SMTP_PASSWORD: &str = "smtp-credential-must-not-appear";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn nonce() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn base_url() -> String {
    std::env::var("GITHEARTH_TEST_DATABASE_URL").expect(
        "set GITHEARTH_TEST_DATABASE_URL to a disposable PostgreSQL database; see docs/foundation.md",
    )
}

struct Database {
    admin_url: String,
    name: String,
    url: String,
    created: bool,
}

impl Database {
    async fn new(label: &str) -> Self {
        let admin_url = base_url();
        let name = format!(
            "githearth_secrets_{}_{}_{}",
            label,
            std::process::id(),
            nonce()
        );
        let url = with_database(&admin_url, &name);
        let mut db = Self {
            admin_url,
            name,
            url,
            created: false,
        };
        db.create().await;
        db
    }

    fn reserve(label: &str) -> Self {
        let admin_url = base_url();
        let name = format!(
            "githearth_secrets_{}_{}_{}",
            label,
            std::process::id(),
            nonce()
        );
        let url = with_database(&admin_url, &name);
        Self {
            admin_url,
            name,
            url,
            created: false,
        }
    }

    async fn exists(&self) -> bool {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let pool = PgPool::connect(&admin_url).await.unwrap();
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
                .bind(&name)
                .fetch_one(&pool)
                .await
                .unwrap();
        pool.close().await;
        exists
    }

    async fn create(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let pool = PgPool::connect(&admin_url).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        self.created = true;
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if !self.created {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            runtime().block_on(async move {
                let pool = PgPool::connect(&admin_url).await?;
                let _ = sqlx::query(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = $1 AND pid <> pg_backend_pid()",
                )
                .bind(&name)
                .execute(&pool)
                .await;
                sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                    .execute(&pool)
                    .await?;
                pool.close().await;
                Ok::<(), sqlx::Error>(())
            })
        })
        .join();
    }
}

fn with_database(base: &str, database: &str) -> String {
    let (head, query) = match base.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (base, None),
    };
    let head = head.trim_end_matches('/');
    let slash = head.rfind('/').expect("database URL has a path");
    let mut url = format!("{}/{database}", &head[..slash]);
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    url
}

struct Installation {
    path: PathBuf,
    database: Database,
    config: Config,
    key_path: PathBuf,
    host_key_path: PathBuf,
    backup_key: Vec<u8>,
    smtp_password_file: PathBuf,
}

impl Installation {
    async fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gh-secrets-{}-{}-{label}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let database = Database::new(label).await;
        let key_path = path.join("installation.key");
        secrets::generate_raw_key_file(&key_path).unwrap();
        let host_key_path = path.join("ssh_host_key");
        ssh::generate_host_key(&host_key_path).unwrap();
        let smtp_password_file = path.join("smtp.password");
        fs::write(&smtp_password_file, SMTP_PASSWORD).unwrap();
        fs::set_permissions(&smtp_password_file, fs::Permissions::from_mode(0o600)).unwrap();
        let config = Config {
            database_url: database.url.clone(),
            database_max_connections: 8,
            database_timeout_seconds: 10,
            storage_root: path.join("data/repositories"),
            installation_key_file: key_path.clone(),
            ssh_enabled: true,
            ssh_host_key_path: host_key_path.clone(),
            bind: "127.0.0.1:1".parse().unwrap(),
            ssh_bind: "127.0.0.1:2".parse().unwrap(),
            smtp_host: "smtp.example.com".into(),
            smtp_from: "noreply@example.com".into(),
            smtp_username: Some("mailer".into()),
            smtp_password_file: Some(smtp_password_file.clone()),
            ..Config::default()
        };
        config.validate().unwrap();
        Self {
            path,
            database,
            config,
            key_path,
            host_key_path,
            backup_key: b"backup passphrase for the secrets test".to_vec(),
            smtp_password_file,
        }
    }

    fn keys(&self) -> secrets::InstallationKeySet {
        secrets::InstallationKeySet::from_paths(&self.key_path, None, None)
            .unwrap()
            .unwrap()
    }

    async fn pool(&self) -> PgPool {
        let pool = connect(&self.config).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    async fn state(&self) -> AppState {
        let pool = connect(&self.config).await.unwrap();
        AppState::new(pool, self.config.clone()).await.unwrap()
    }

    fn key_base64(&self) -> String {
        let text = fs::read_to_string(&self.key_path).unwrap();
        text.lines()
            .find_map(|line| line.strip_prefix("key = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("key file has a key line")
            .to_owned()
    }

    /// Inserts an account and sends one login from a fixed address, so the
    /// audit row carries an IP digest and the server creates its IP key.
    async fn login_with_ip(&self, app: &axum::Router, pool: &PgPool) -> String {
        let response = self.login_response(app, pool).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// The same login without asserting the outcome, for tests that expect a
    /// refusal. It still creates the account it tries to sign in as.
    async fn login_response(&self, app: &axum::Router, pool: &PgPool) -> axum::response::Response {
        let hash = hash_password(PASSWORD).unwrap();
        let username = format!("user-{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO accounts (id, username, password_hash, is_admin) \
             VALUES ($1, $2, $3, false)",
        )
        .bind(Uuid::new_v4())
        .bind(&username)
        .bind(&hash)
        .execute(pool)
        .await
        .unwrap();
        let body = serde_json::json!({"username": username, "password": PASSWORD}).to_string();
        let mut request = Request::post("/api/v1/login")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 19], 4242))));
        app.clone().oneshot(request).await.unwrap()
    }

    async fn newest_audit_key(&self, pool: &PgPool) -> Uuid {
        sqlx::query_scalar("SELECT id FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn dump(&self) -> Vec<u8> {
        let out = self.path.join("dump.sql");
        let status = Command::new("pg_dump")
            .args(["--no-owner", "--no-privileges", "--format=plain", "--file"])
            .arg(&out)
            .arg(&self.database.url)
            .status()
            .unwrap();
        assert!(status.success(), "pg_dump failed");
        fs::read(out).unwrap()
    }

    async fn backup(&self) -> PathBuf {
        let output = self.path.join("archive.ghbackup");
        backup::run_backup(&self.config, None, &self.backup_key, &output)
            .await
            .unwrap();
        output
    }

    fn decrypt_archive(&self, archive: &Path) -> PathBuf {
        let out = self.path.join("decrypted.tar");
        let header =
            backup::decrypt_file(archive, &out, &self.backup_key).expect("archive decrypts");
        assert!(!header.key_id.is_empty());
        let root = self.path.join("archive");
        fs::create_dir_all(&root).unwrap();
        let status = Command::new("tar")
            .args(["-xf", out.to_str().unwrap(), "-C", root.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "tar extract failed");
        root
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// `bytea` in a plain SQL dump is written as `\\x` followed by lowercase hex,
/// so the raw bytes alone are not enough to prove the property.
fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn dump_contains_secret(dump: &[u8], secret: &[u8]) -> bool {
    let text = String::from_utf8_lossy(dump);
    contains(dump, secret) || text.contains(&format!("\\x{}", hex_of(secret)))
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

fn read(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn dump_and_decrypted_archive_contain_no_plaintext_secret() {
    let installation = Installation::new("dump").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;

    let audit_key_id = installation.newest_audit_key(&pool).await;
    let plaintext = secrets::load_audit_ip_key(&pool, audit_key_id, &installation.keys())
        .await
        .expect("the audit IP key decrypts with the installation key");
    assert_eq!(plaintext.len(), 32);

    // U28: the dump contains no plaintext secret and no key material.
    let dump = installation.dump();
    assert!(
        !dump_contains_secret(&dump, &plaintext),
        "the database dump contains the plaintext audit IP key"
    );
    assert!(
        !contains(&dump, installation.key_base64().as_bytes()),
        "the database dump contains the installation key"
    );
    assert!(
        !contains(
            &dump,
            &secrets::test_key_bytes(&installation.key_path).unwrap()
        ),
        "the database dump contains the raw installation key"
    );
    assert!(
        !contains(&dump, &installation.backup_key),
        "the database dump contains the backup key"
    );
    assert!(
        !contains(&dump, SMTP_PASSWORD.as_bytes()),
        "the database dump contains the SMTP credential"
    );

    // U28: the encrypted archive contains no plaintext secret; after
    // decryption only operator-owned material (the database URL in the config
    // and the SSH host key) is present as private material.
    let archive = installation.backup().await;
    let root = installation.decrypt_archive(&archive);
    let manifest: serde_json::Value =
        serde_json::from_slice(&read(&root.join("manifest.json"))).unwrap();
    assert_eq!(
        manifest["installation"]["key_id"].as_str(),
        Some(installation.keys().current().id()),
        "the manifest records the installation key id"
    );
    assert!(manifest["encryption"]["key_id"].as_str().is_some());
    let manifest_text = serde_json::to_string(&manifest).unwrap();
    assert!(
        !manifest_text.contains(&installation.key_base64()),
        "the manifest contains installation key material"
    );
    assert!(
        !contains(manifest_text.as_bytes(), &installation.backup_key),
        "the manifest contains the archive key"
    );
    assert_eq!(
        read(&root.join("ssh_host_key")),
        read(&installation.host_key_path),
        "the archive carries the operator's SSH host key"
    );
    assert!(root.join("database.dump").is_file());
    assert!(root.join("storage.tar").is_file());
    assert!(root.join("config.toml").is_file());

    // `database.dump` is a compressed custom-format dump; expand it so the
    // search sees the same text a plain dump would contain.
    let expanded = installation.path.join("archive-database.sql");
    let status = Command::new("pg_restore")
        .args(["--no-owner", "--no-acl", "--file"])
        .arg(&expanded)
        .arg(root.join("database.dump"))
        .status()
        .unwrap();
    assert!(status.success(), "pg_restore of the archived dump failed");
    let expanded_bytes = read(&expanded);
    assert!(
        !dump_contains_secret(&expanded_bytes, &plaintext),
        "the archived database contains the plaintext audit IP key"
    );
    assert!(
        !dump_contains_secret(
            &expanded_bytes,
            &secrets::test_key_bytes(&installation.key_path).unwrap()
        ),
        "the archived database contains the installation key"
    );

    let mut host_key_seen = false;
    for file in walk_files(&root) {
        let bytes = read(&file);
        assert!(
            !contains(&bytes, &plaintext),
            "{} contains the plaintext audit IP key",
            file.display()
        );
        assert!(
            !contains(&bytes, installation.key_base64().as_bytes()),
            "{} contains the installation key",
            file.display()
        );
        assert!(
            !contains(
                &bytes,
                &secrets::test_key_bytes(&installation.key_path).unwrap()
            ),
            "{} contains the raw installation key",
            file.display()
        );
        assert!(
            !contains(&bytes, &installation.backup_key),
            "{} contains the backup key",
            file.display()
        );
        assert!(
            !contains(&bytes, SMTP_PASSWORD.as_bytes()),
            "{} contains the SMTP credential",
            file.display()
        );
        if file.file_name().is_some_and(|name| name == "ssh_host_key") {
            host_key_seen = true;
        }
    }
    assert!(host_key_seen, "the host key is present after decryption");
    let config_text = fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        config_text.contains(&installation.database.url),
        "the configuration is the operator-owned database value"
    );
    assert!(config_text.contains("smtp_username = \"mailer\""));
    assert!(
        !config_text.contains(SMTP_PASSWORD),
        "the SMTP credential is a separate operator file, not a config value"
    );
    let _ = installation.smtp_password_file;

    // Control: the search is a real byte search, not a flag. Plant one
    // plaintext row and the same check must find it in the `bytea` hex form
    // pg_dump writes. It is removed again so the fixture is left clean.
    let planted = [0xc7u8; 32];
    let planted_id = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
        .bind(planted_id)
        .bind(planted.to_vec())
        .execute(&pool)
        .await
        .unwrap();
    let control = installation.dump();
    assert!(
        dump_contains_secret(&control, &planted),
        "the dump search is insensitive: a planted plaintext key was not found"
    );
    sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
        .bind(planted_id)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rotation_rewraps_data_keys_and_keeps_every_secret_readable() {
    let installation = Installation::new("rotate").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;
    let audit_key_id = installation.newest_audit_key(&pool).await;
    let old_keys = installation.keys();
    let before = secrets::load_audit_ip_key(&pool, audit_key_id, &old_keys)
        .await
        .unwrap();
    let (_, _, cipher_before) = secrets::read_stored_secret(
        &pool,
        sqlx::query_scalar("SELECT secret_id FROM audit_ip_keys WHERE id = $1")
            .bind(audit_key_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .await
    .unwrap();

    let new_key_path = installation.path.join("installation-new.key");
    secrets::generate_raw_key_file(&new_key_path).unwrap();
    let new_keys =
        secrets::InstallationKeySet::from_paths(&new_key_path, Some(&installation.key_path), None)
            .unwrap()
            .unwrap();
    let report = secrets::rotate(&pool, &new_keys, secrets::ROTATE_BATCH)
        .await
        .unwrap();
    assert!(report.rewrapped >= 1, "at least one data key is re-wrapped");
    assert_eq!(report.remaining, 0, "every data key is under the new key");

    // The secret ciphertext is untouched: rotation never decrypts a secret.
    let secret_id: Uuid = sqlx::query_scalar("SELECT secret_id FROM audit_ip_keys WHERE id = $1")
        .bind(audit_key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_, _, cipher_after) = secrets::read_stored_secret(&pool, secret_id).await.unwrap();
    assert_eq!(cipher_before, cipher_after);

    assert_eq!(
        secrets::load_audit_ip_key(&pool, audit_key_id, &new_keys)
            .await
            .unwrap(),
        before
    );
    // A row that has been re-wrapped is not decryptable with the old key alone;
    // the retired key is only needed while rows still name it.
    assert!(
        secrets::load_audit_ip_key(&pool, audit_key_id, &old_keys)
            .await
            .is_err(),
        "the old key alone must not decrypt a re-wrapped row"
    );
    let status = secrets::status(&pool, Some(&new_keys)).await.unwrap();
    assert!(status.rotation_complete);
    assert_eq!(status.legacy_plaintext, 0);
    assert!(status.unknown_key_ids.is_empty());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn missing_or_wrong_key_fails_at_startup_and_migration() {
    let installation = Installation::new("wrong").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;

    // Missing key: stored secrets exist, so start-up refuses with a named error.
    let error = secrets::startup(&pool, None, secrets::BACKFILL_BATCH)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("installation key") || error.contains("stored secrets"),
        "{error}"
    );

    // Wrong key: the key id is named, and the error says the key does not match.
    let wrong_path = installation.path.join("wrong.key");
    secrets::generate_raw_key_file(&wrong_path).unwrap();
    let wrong = secrets::InstallationKeySet::from_paths(&wrong_path, None, None)
        .unwrap()
        .unwrap();
    let error = secrets::startup(&pool, Some(&wrong), secrets::BACKFILL_BATCH)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not match")
            || error.contains("wrong key")
            || error.contains("is not configured"),
        "{error}"
    );
    assert!(
        !error.contains(&installation.key_base64()),
        "the error names no key material: {error}"
    );

    // Migration with a plaintext row and no key refuses rather than leaving it.
    let legacy_id = Uuid::new_v4();
    sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
        .bind(legacy_id)
        .bind(vec![0x42u8; 32])
        .execute(&pool)
        .await
        .unwrap();
    let error = secrets::startup(&pool, None, secrets::BACKFILL_BATCH)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("installation key is required to encrypt existing plaintext"),
        "{error}"
    );

    // With the correct key, the same call backfills and clears the plaintext.
    let report = secrets::startup(&pool, Some(&installation.keys()), secrets::BACKFILL_BATCH)
        .await
        .unwrap();
    assert!(report.backfilled >= 1);
    let plaintext: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT key FROM audit_ip_keys WHERE id = $1")
            .bind(legacy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        plaintext.is_none(),
        "the migration clears the plaintext column"
    );

    // A KMS method is refused clearly rather than silently falling back.
    let mut kms = installation.config.clone();
    kms.installation_key_method = "kms".into();
    let error = secrets::InstallationKeySet::from_config(&kms)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no KMS adapter"), "{error}");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn no_api_doctor_or_error_returns_a_secret() {
    let installation = Installation::new("no-leak").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    let token = installation.login_with_ip(&app, &pool).await;
    let audit_key_id = installation.newest_audit_key(&pool).await;
    let plaintext = secrets::load_audit_ip_key(&pool, audit_key_id, &installation.keys())
        .await
        .unwrap();
    let key_raw = secrets::test_key_bytes(&installation.key_path).unwrap();
    let key_hex = hex_of(&key_raw);

    let checks = doctor::report(&installation.config).await;
    let doctor_text = checks
        .iter()
        .map(|check| check.detail.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!contains(doctor_text.as_bytes(), &plaintext));
    assert!(!doctor_text.contains(&installation.key_base64()));
    assert!(!doctor_text.contains(&key_hex));

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/me")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!contains(&body, &plaintext));
    assert!(!contains(&body, installation.key_base64().as_bytes()));

    let debug = format!("{:?}", state.installation_keys);
    assert!(
        debug.contains("[redacted]"),
        "key material is redacted in Debug"
    );
    assert!(!debug.contains(&installation.key_base64()));
    assert!(!debug.contains(&key_hex));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn backup_refuses_a_private_file_inside_storage_root() {
    let installation = Installation::new("inside").await;
    let _pool = installation.pool().await;
    let _state = installation.state().await;
    let inside = installation.config.storage_root.join("installation.key");
    fs::create_dir_all(&installation.config.storage_root).unwrap();
    fs::copy(&installation.key_path, &inside).unwrap();
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o600)).unwrap();
    let mut config = installation.config.clone();
    config.installation_key_file = inside;
    let output = installation.path.join("should-not-exist.ghbackup");
    let error = match backup::run_backup(&config, None, &installation.backup_key, &output).await {
        Ok(_) => panic!("a private file inside storage_root must refuse the backup"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("inside storage_root"), "{error}");
    assert!(!output.exists(), "no archive is published");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_without_the_installation_key_fails_before_publishing() {
    let installation = Installation::new("restore-nokey").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;
    pool.close().await;
    let archive = installation.backup().await;

    // Move the key away: the archived configuration names this path, and the
    // environment does not override it.
    let hidden = installation.path.join("hidden-installation.key");
    fs::rename(&installation.key_path, &hidden).unwrap();
    let destination = Database::reserve("restore-nokey-dest");
    let destination_root = installation.path.join("restore-root");
    let config_path = installation.path.join("restore-config.toml");
    let host_key_path = installation.path.join("restore-host-key");
    let options = backup::RestoreOptions {
        archive,
        passphrase: installation.backup_key.clone(),
        database_url: destination.url.clone(),
        config_path: config_path.clone(),
        storage_root: Some(destination_root.clone()),
        ssh_host_key_path: Some(host_key_path.clone()),
        accept_shredded: false,
    };
    let error = match backup::run_restore(&options).await {
        Ok(_) => panic!("restoring without the installation key must fail"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("installation key")
            || error.contains("cannot inspect")
            || error.contains("cannot read"),
        "{error}"
    );
    assert!(
        !destination.exists().await,
        "a failed restore leaves no destination database"
    );
    assert!(
        !config_path.exists(),
        "a failed restore writes no configuration"
    );
    assert!(
        !destination_root.exists(),
        "a failed restore publishes no storage"
    );
    assert!(
        !host_key_path.exists(),
        "a failed restore writes no host key"
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn startup_refuses_when_another_session_locks_a_plaintext_key() {
    let installation = Installation::new("locked").await;
    let pool = installation.pool().await;

    // Three pre-envelope rows: one stays locked by a second session, so the
    // backfill's `SKIP LOCKED` batches cannot see it.
    let mut ids = Vec::new();
    for byte in [0x11u8, 0x22, 0x33] {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
            .bind(id)
            .bind(vec![byte; 32])
            .execute(&pool)
            .await
            .unwrap();
        ids.push(id);
    }
    let mut lock = pool.begin().await.unwrap();
    let locked: Uuid = sqlx::query_scalar("SELECT id FROM audit_ip_keys WHERE id = $1 FOR UPDATE")
        .bind(ids[0])
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    assert_eq!(locked, ids[0]);

    let error = secrets::startup(&pool, Some(&installation.keys()), secrets::BACKFILL_BATCH)
        .await
        .expect_err("a locked plaintext row must fail start-up, not report success")
        .to_string();
    assert!(error.contains("locked by another session"), "{error}");
    let still_plaintext: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        still_plaintext >= 1,
        "the locked row is still plaintext after the refused start-up"
    );

    // Release the lock: the same call now succeeds and clears every row.
    lock.rollback().await.unwrap();
    let report = secrets::startup(&pool, Some(&installation.keys()), secrets::BACKFILL_BATCH)
        .await
        .unwrap();
    assert!(report.backfilled >= 1);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_stale_installation_key_is_reported_as_an_installation_key_error() {
    let installation = Installation::new("stale").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;

    // Rotate under a new key but leave `serve` holding the old set, the
    // documented "rotate before restarting" trap.
    let new_key_path = installation.path.join("installation-new.key");
    secrets::generate_raw_key_file(&new_key_path).unwrap();
    let new_keys =
        secrets::InstallationKeySet::from_paths(&new_key_path, Some(&installation.key_path), None)
            .unwrap()
            .unwrap();
    secrets::rotate(&pool, &new_keys, secrets::ROTATE_BATCH)
        .await
        .unwrap();

    let stale_state = installation.state().await; // config still names the old key
    let stale_app = router(stale_state);
    let response = installation.login_response(&stale_app, &pool).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["error"].as_str(),
        Some("installation key unavailable"),
        "a key failure must not masquerade as a database outage: {body}"
    );
    assert!(
        !String::from_utf8_lossy(&bytes).contains(&installation.key_base64()),
        "the error body names no key material"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_plaintext_audit_key_is_refused_at_use_and_never_returned() {
    let installation = Installation::new("legacy-read").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;

    // A mixed-version writer (or a fixture) inserts a newer plaintext row
    // after start-up. The running server must fail closed rather than use it.
    let legacy_id = Uuid::new_v4();
    let legacy_key = [0x5au8; 32];
    sqlx::query(
        "INSERT INTO audit_ip_keys (id, key, created_at) \
         VALUES ($1, $2, clock_timestamp() + interval '1 second')",
    )
    .bind(legacy_id)
    .bind(legacy_key.to_vec())
    .execute(&pool)
    .await
    .unwrap();

    let response = installation.login_response(&app, &pool).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["error"].as_str(),
        Some("installation key unavailable"),
        "{body}"
    );
    let used: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE client_ip_key_id = $1")
            .bind(legacy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        used, 0,
        "the plaintext key was never used to stamp an event"
    );

    // The operator path conforms: `migrate` encrypts the row and clears it.
    let report = secrets::startup(&pool, Some(&installation.keys()), secrets::BACKFILL_BATCH)
        .await
        .unwrap();
    assert!(report.backfilled >= 1);
    let plaintext: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT key FROM audit_ip_keys WHERE id = $1")
            .bind(legacy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(plaintext.is_none());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn backup_refuses_while_a_rotation_is_incomplete() {
    let installation = Installation::new("backup-rotation").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;
    let old_keys = installation.keys();
    audit_retention::rotate_ip_key(&pool, &old_keys, false)
        .await
        .unwrap();

    let new_key_path = installation.path.join("installation-new.key");
    secrets::generate_raw_key_file(&new_key_path).unwrap();
    let new_keys =
        secrets::InstallationKeySet::from_paths(&new_key_path, Some(&installation.key_path), None)
            .unwrap()
            .unwrap();
    // Hold one data-key row locked so `rotate` leaves it under the retired
    // key; this is the deterministic partial-rotation state.
    let mut lock = pool.begin().await.unwrap();
    let locked: Uuid = sqlx::query_scalar(
        "SELECT id FROM secret_data_keys WHERE kek_id = $1 ORDER BY id LIMIT 1 FOR UPDATE",
    )
    .bind(old_keys.current().id())
    .fetch_one(&mut *lock)
    .await
    .unwrap();
    let report = secrets::rotate(&pool, &new_keys, 1).await.unwrap();
    assert!(report.rewrapped >= 1);
    assert_eq!(
        report.remaining, 1,
        "one data key is deliberately left behind"
    );
    let remaining_id: Uuid =
        sqlx::query_scalar("SELECT id FROM secret_data_keys WHERE kek_id = $1")
            .bind(old_keys.current().id())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining_id, locked);

    // The interrupted batch loses no secret: the row still under the retired
    // key opens with the retired key, and with both keys configured, to the
    // same value it had before the rotation.
    let audit_key_id: Uuid = sqlx::query_scalar(
        "SELECT a.id FROM audit_ip_keys a JOIN stored_secrets s ON s.id = a.secret_id \
         WHERE s.data_key_id = $1",
    )
    .bind(remaining_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let with_old = secrets::load_audit_ip_key(&pool, audit_key_id, &old_keys)
        .await
        .unwrap();
    let with_both = secrets::load_audit_ip_key(&pool, audit_key_id, &new_keys)
        .await
        .unwrap();
    assert_eq!(with_old, with_both);
    assert_eq!(with_old.len(), 32);

    let mut config = installation.config.clone();
    config.installation_key_file = new_key_path;
    config.installation_key_previous_file = installation.key_path.clone();
    let output = installation.path.join("incomplete-rotation.ghbackup");
    let error = match backup::run_backup(&config, None, &installation.backup_key, &output).await {
        Ok(_) => panic!("a backup during an incomplete rotation must refuse"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("rotation is incomplete"), "{error}");
    assert!(!output.exists(), "no archive is published");
    lock.rollback().await.unwrap();
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn status_and_doctor_report_a_corrupt_wrapper() {
    let installation = Installation::new("corrupt-wrapper").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;

    sqlx::query(
        "UPDATE secret_data_keys \
         SET wrapped_key = set_byte(wrapped_key, 0, (get_byte(wrapped_key, 0) + 1) % 256)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let status = secrets::status(&pool, Some(&installation.keys()))
        .await
        .unwrap();
    assert!(
        !status.rotation_complete,
        "a damaged wrapper is not complete"
    );
    assert_eq!(
        status.unreadable_key_ids,
        vec![installation.keys().current().id().to_owned()]
    );

    let checks = doctor::report(&installation.config).await;
    let check = checks
        .iter()
        .find(|check| check.name == "installation key")
        .expect("doctor reports the installation key");
    assert!(
        check.detail.contains("cannot be unwrapped"),
        "{}",
        check.detail
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_damaged_second_wrapper_refuses_startup_and_is_not_rotation_complete() {
    let installation = Installation::new("second-wrapper").await;
    let pool = installation.pool().await;
    let state = installation.state().await;
    let app = router(state.clone());
    installation.login_with_ip(&app, &pool).await;
    let keys = installation.keys();
    // A second data key under the same key id. Each sealed secret gets its own
    // data key, so this is the ordinary shape of an installation with more than
    // one secret.
    audit_retention::rotate_ip_key(&pool, &keys, false)
        .await
        .unwrap();
    let wrapped: Vec<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, wrapped_key FROM secret_data_keys ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(wrapped.len(), 2, "two data keys under one key id");

    // Damage only the second wrapper. Sampling one row per key id still sees a
    // readable wrapper, which is exactly the secret-loss path this closes.
    let (damaged, _) = wrapped[1].clone();
    sqlx::query(
        "UPDATE secret_data_keys \
         SET wrapped_key = set_byte(wrapped_key, 0, (get_byte(wrapped_key, 0) + 1) % 256) \
         WHERE id = $1",
    )
    .bind(damaged)
    .execute(&pool)
    .await
    .unwrap();
    let after: Vec<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, wrapped_key FROM secret_data_keys ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        wrapped[0].1, after[0].1,
        "the first wrapper is untouched, so a one-row sample would pass"
    );
    assert_ne!(wrapped[1].1, after[1].1, "the second wrapper is damaged");

    let status = secrets::status(&pool, Some(&keys)).await.unwrap();
    assert!(
        !status.rotation_complete,
        "a damaged non-sampled wrapper must not be reported as rotation complete"
    );
    assert_eq!(
        status.unreadable_key_ids,
        vec![keys.current().id().to_owned()]
    );

    let error = secrets::startup(&pool, Some(&keys), secrets::BACKFILL_BATCH)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cannot be opened"),
        "startup must refuse the damaged wrapper: {error}"
    );
    assert!(
        error.contains("1 of 2"),
        "the refusal counts every wrapper: {error}"
    );
    assert!(!error.contains(&installation.key_base64()), "{error}");

    let checks = doctor::report(&installation.config).await;
    let check = checks
        .iter()
        .find(|check| check.name == "installation key")
        .expect("doctor reports the installation key");
    assert!(
        matches!(check.status, doctor::Status::Fail),
        "{}",
        check.detail
    );
    pool.close().await;
}

/// Wraps a 32-byte data key the way an older binary did: the same key material
/// and key id, but a different `wrap_method`, which is part of the AAD. This is
/// the row shape a descriptor-format rename leaves behind.
fn wrap_with_recorded_method(
    key: &[u8; 32],
    kek_id: &str,
    data_key_id: Uuid,
    method: &str,
    data_key: &[u8; 32],
) -> Vec<u8> {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit, Payload},
    };
    let cipher = Aes256Gcm::new_from_slice(key).unwrap();
    let nonce = [0x5au8; 12];
    let mut aad = b"githearth-dek-wrap-v1\0".to_vec();
    aad.extend_from_slice(kek_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(data_key_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(method.as_bytes());
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: data_key,
                aad: &aad,
            },
        )
        .unwrap();
    let mut wrapped = nonce.to_vec();
    wrapped.extend_from_slice(&ciphertext);
    wrapped
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rotation_rewraps_a_current_key_row_whose_wrap_method_is_older() {
    let installation = Installation::new("method-migration").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let data_key_id = Uuid::new_v4();
    let data_key = [0x42u8; 32];
    let encoded = installation.key_base64();
    let key: [u8; 32] = BASE64.decode(encoded).unwrap().try_into().unwrap();
    let legacy_method = "file-aes256gcm-v0";
    assert_ne!(legacy_method, keys.current().method());
    let wrapped = wrap_with_recorded_method(
        &key,
        keys.current().id(),
        data_key_id,
        legacy_method,
        &data_key,
    );
    sqlx::query(
        "INSERT INTO secret_data_keys (id, kek_id, algorithm, wrap_method, wrapped_key) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(data_key_id)
    .bind(keys.current().id())
    .bind(secrets::ALGORITHM)
    .bind(legacy_method)
    .bind(&wrapped)
    .execute(&pool)
    .await
    .unwrap();

    // Same key id, so the same key material wraps it, but start-up still
    // refuses: the configured method and the recorded method disagree and only
    // a rotation can settle which one the row will be read with.
    let error = secrets::startup(&pool, Some(&keys), secrets::BACKFILL_BATCH)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("wrapped with method"),
        "startup must refuse a row under an older wrap method: {error}"
    );
    let status = secrets::status(&pool, Some(&keys)).await.unwrap();
    assert!(
        !status.rotation_complete,
        "a row under an older wrap method is not rotation complete"
    );
    assert_eq!(status.outdated_wrap_method, 1);
    assert!(
        status.unreadable_key_ids.is_empty(),
        "the row opens with the method it records, so it is not a damaged wrapper"
    );
    let checks = doctor::report(&installation.config).await;
    let check = checks
        .iter()
        .find(|check| check.name == "installation key")
        .expect("doctor reports the installation key");
    assert!(
        matches!(check.status, doctor::Status::Warn),
        "doctor must warn about the rotation, not call the wrapper damaged: {}",
        check.detail
    );

    // Rotation selects on the key id *or* the method, opens the row with the
    // method it records (the id already names the key material) and re-wraps it
    // under the current method. Without the method term the row is left behind
    // and this installation can never start.
    let report = secrets::rotate(&pool, &keys, secrets::ROTATE_BATCH)
        .await
        .unwrap();
    assert_eq!(report.rewrapped, 1, "the older-method row is re-wrapped");
    assert_eq!(report.remaining, 0, "nothing needs re-wrapping afterwards");

    let (kek_id, method): (String, String) =
        sqlx::query_as("SELECT kek_id, wrap_method FROM secret_data_keys WHERE id = $1")
            .bind(data_key_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kek_id, keys.current().id());
    assert_eq!(method, keys.current().method());

    secrets::startup(&pool, Some(&keys), secrets::BACKFILL_BATCH)
        .await
        .expect("a re-wrapped row opens at the next start-up");
    let status = secrets::status(&pool, Some(&keys)).await.unwrap();
    assert!(status.rotation_complete);
    assert_eq!(status.outdated_wrap_method, 0);
    pool.close().await;
}

#[test]
fn config_validate_refuses_a_private_file_inside_storage_root() {
    let directory = std::env::temp_dir().join(format!(
        "gh-secrets-config-{}-{}",
        std::process::id(),
        nonce()
    ));
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let key_path = directory.join("installation.key");
    secrets::generate_raw_key_file(&key_path).unwrap();
    let storage_root = directory.join("storage");
    fs::create_dir_all(&storage_root).unwrap();
    let mut config = Config {
        database_url: "postgres://localhost/unused".into(),
        storage_root: storage_root.clone(),
        installation_key_file: storage_root.join("installation.key"),
        ..Config::default()
    };
    config.installation_key_method = "file".into();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("inside storage_root"), "{error}");
    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn passphrase_key_round_trips_and_wrong_passphrase_fails() {
    let directory = std::env::temp_dir().join(format!(
        "gh-secrets-passphrase-{}-{}",
        std::process::id(),
        nonce()
    ));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let descriptor = directory.join("installation.key");
    let passphrase = directory.join("passphrase");
    fs::write(&passphrase, b"correct horse battery staple").unwrap();
    fs::set_permissions(&passphrase, fs::Permissions::from_mode(0o600)).unwrap();
    secrets::generate_passphrase_key_file(&descriptor).unwrap();
    let keys =
        secrets::InstallationKeySet::from_paths(&descriptor, None, Some(passphrase.as_path()))
            .unwrap()
            .unwrap();
    let secret_id = Uuid::new_v4();
    let sealed = secrets::seal(&keys, secret_id, b"a secret value").unwrap();
    let data_key = secrets::WrappedDataKey {
        id: sealed.data_key_id,
        kek_id: sealed.kek_id.clone(),
        wrap_method: sealed.wrap_method.clone(),
        wrapped_key: sealed.wrapped_key.clone(),
        owner_key_id: None,
        owner_kind: None,
        owner_id: None,
    };
    assert_eq!(
        secrets::open(
            &keys,
            secret_id,
            &data_key,
            &sealed.nonce,
            &sealed.ciphertext
        )
        .unwrap(),
        b"a secret value"
    );

    fs::write(&passphrase, b"wrong passphrase").unwrap();
    let error =
        secrets::InstallationKeySet::from_paths(&descriptor, None, Some(passphrase.as_path()))
            .err()
            .map(|error| error.to_string());
    // Deriving with the wrong passphrase succeeds at load time; the mismatch is
    // detected when the first wrapped data key is unwrapped.
    if let Some(keys) =
        secrets::InstallationKeySet::from_paths(&descriptor, None, Some(passphrase.as_path()))
            .unwrap()
    {
        assert!(
            secrets::open(
                &keys,
                secret_id,
                &data_key,
                &sealed.nonce,
                &sealed.ciphertext
            )
            .is_err()
        );
    } else {
        assert!(error.is_some());
    }
    let _ = fs::remove_dir_all(directory);
}
