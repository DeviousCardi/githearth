//! Per-account and per-organization data keys, crypto-shredding and the backup
//! interaction (M11a increment 8A, case U36).
//!
//! These cases run against a real PostgreSQL and the real backup/restore
//! library. No HTTP endpoint returns a secret and no acceptance path reads a
//! key through the database; the reads are the same library read path the
//! product uses at the point of use.

use githearth_server::{
    MIGRATOR, audit,
    backup::{self, RestoreOptions},
    config::Config,
    connect, data_keys, hash_password, secrets, ssh,
};
use serde_json::Value;
use sqlx::{Acquire, PgPool};
use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

const PASSWORD: &str = "a sufficiently long password for shredding";

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
            "githearth_shred_{}_{}_{}",
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
            "githearth_shred_{}_{}_{}",
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

struct Installation {
    path: PathBuf,
    database: Database,
    config: Config,
    key_path: PathBuf,
    data_key_directory: PathBuf,
    backup_key: Vec<u8>,
}

impl Installation {
    async fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "gh-shred-{}-{}-{label}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(path.join("data/repositories/repositories")).unwrap();
        let database = Database::new(label).await;
        let key_path = path.join("installation.key");
        secrets::generate_raw_key_file(&key_path).unwrap();
        let host_key_path = path.join("ssh_host_key");
        ssh::generate_host_key(&host_key_path).unwrap();
        let data_key_directory = path.join("data-keys");
        let config = Config {
            database_url: database.url.clone(),
            database_max_connections: 8,
            database_timeout_seconds: 10,
            storage_root: path.join("data/repositories"),
            installation_key_file: key_path.clone(),
            data_key_directory: data_key_directory.clone(),
            ssh_enabled: true,
            ssh_host_key_path: host_key_path.clone(),
            bind: "127.0.0.1:1".parse().unwrap(),
            ssh_bind: "127.0.0.1:2".parse().unwrap(),
            ..Config::default()
        };
        config.validate().unwrap();
        Self {
            path,
            database,
            config,
            key_path,
            data_key_directory,
            backup_key: b"backup passphrase for the crypto-shredding test".to_vec(),
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

    async fn backup(&self) -> PathBuf {
        let output = self.path.join("archive.ghbackup");
        backup::run_backup(&self.config, None, &self.backup_key, &output)
            .await
            .unwrap();
        output
    }

    fn decrypt_archive(&self, archive: &Path) -> PathBuf {
        let out = self.path.join("decrypted.tar");
        backup::decrypt_file(archive, &out, &self.backup_key).expect("archive decrypts");
        let root = self.path.join("archive");
        fs::create_dir_all(&root).unwrap();
        let status = Command::new("tar")
            .args(["-xf", out.to_str().unwrap(), "-C", root.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "tar extract failed");
        root
    }

    fn restore_options(
        &self,
        archive: &Path,
        destination: &Database,
        label: &str,
    ) -> RestoreOptions {
        self.restore_options_with(archive, destination, label, true)
    }

    fn restore_options_with(
        &self,
        archive: &Path,
        destination: &Database,
        label: &str,
        accept_shredded: bool,
    ) -> RestoreOptions {
        RestoreOptions {
            archive: archive.to_path_buf(),
            passphrase: self.backup_key.clone(),
            database_url: destination.url.clone(),
            config_path: self.path.join(format!("restore-{label}.toml")),
            storage_root: Some(self.path.join(format!("restore-root-{label}"))),
            ssh_host_key_path: Some(self.path.join(format!("restore-host-key-{label}"))),
            accept_shredded,
        }
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

async fn insert_account(pool: &PgPool, username: &str, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id, username, password_hash, is_admin, email) \
         VALUES ($1, $2, $3, false, $4)",
    )
    .bind(id)
    .bind(username)
    .bind(hash_password(PASSWORD).unwrap())
    .bind(email)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn insert_organization(pool: &PgPool, slug: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(format!("Organization {slug}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn store_secret(
    pool: &PgPool,
    keys: &secrets::InstallationKeySet,
    directory: &Path,
    kind: data_keys::OwnerKind,
    owner: Uuid,
    name: &str,
    plaintext: &[u8],
) -> Uuid {
    let secret_id = Uuid::new_v4();
    let sealed = secrets::seal_owned(keys, directory, secret_id, plaintext, kind, owner).unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    secrets::insert_sealed(&mut tx, name, &sealed)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    secret_id
}

fn read(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// C1/M1: the operator docs state the custody-copy limit, not its negation.
// A security disclosure is part of the mechanism here: the old sentence
// promised that a shred stays a shred even when an older custody copy is
// restored, which the test below proves false. This guard makes a reverted
// sentence fail the suite rather than silently promising a property the
// design does not have.
// ---------------------------------------------------------------------------

#[test]
fn the_docs_state_the_custody_copy_limit_honestly() {
    const SECRETS: &str = include_str!("../../../docs/operations/secrets-at-rest.md");
    const BACKUP: &str = include_str!("../../../docs/operations/backup-restore.md");
    const ADR: &str = include_str!("../../../docs/decisions/0015-identity-slice.md");

    for doc in [SECRETS, BACKUP] {
        assert!(
            doc.contains("A shred is only as strong as the custody copies you discard"),
            "the operator docs must say what bounds a shred against an older archive"
        );
        assert!(
            doc.contains("never restore a custody copy that predates a deletion"),
            "the operator docs must forbid the copy that un-shreds the archive"
        );
        assert!(
            !doc.contains("with it, a shredded key stays destroyed"),
            "the docs must not claim a retained custody copy cannot un-shred an archive"
        );
    }
    assert!(
        ADR.contains("Retention of `data_key_directory` copies, not the tombstone, bounds a shred"),
        "ADR 0015's Consequences must record the custody-copy bound"
    );
    assert!(
        ADR.contains("a custody copy taken before a deletion still holds"),
        "ADR 0015 must say why a custody copy breaks the shred"
    );
}

// ---------------------------------------------------------------------------
// U36: an account's secret is unreadable in an older backup after the shred,
// while the pseudonymous row and its profile columns remain.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn account_shred_makes_an_older_backup_unreadable_and_keeps_the_row() {
    let installation = Installation::new("account").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "shredded-user", "shredded@example.com").await;

    let secret = b"a TOTP seed that must become unreadable";
    let secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/shredded-user",
        secret,
    )
    .await;
    let key_id = data_keys::account_data_key_id(account);
    assert!(
        installation
            .data_key_directory
            .join(format!("{key_id}.key"))
            .is_file()
    );

    // The secret is readable before the shred, so the later refusal is about
    // the destroyed key and not an unrelated read failure.
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &pool,
            secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        secret
    );

    // Take the backup *before* the deletion. It carries the ciphertext and the
    // account row, never the owner key.
    let archive = installation.backup().await;
    pool.close().await;

    let report = data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();
    assert!(report.had_key);
    assert!(
        !installation
            .data_key_directory
            .join(format!("{key_id}.key"))
            .exists()
    );

    let destination = Database::reserve("account-dest");
    let options = installation.restore_options(&archive, &destination, "account");
    let restored = backup::run_restore(&options).await.unwrap();
    assert_eq!(
        restored.shredded_data_keys, 1,
        "the restore reports the completed shred"
    );

    let restored_pool = PgPool::connect(&destination.url).await.unwrap();
    // The pseudonymous row and its plaintext profile columns remain.
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT username, email FROM accounts WHERE id = $1")
            .bind(account)
            .fetch_one(&restored_pool)
            .await
            .unwrap();
    assert_eq!(row.0, "shredded-user");
    assert_eq!(row.1.as_deref(), Some("shredded@example.com"));
    // The secret row and its ciphertext are still present in the backup.
    let present: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stored_secrets WHERE id = $1")
        .bind(secret_id)
        .fetch_one(&restored_pool)
        .await
        .unwrap();
    assert_eq!(present, 1, "the ciphertext row is still in the backup");

    let error = secrets::load_secret_with_data_keys(
        &restored_pool,
        secret_id,
        &keys,
        Some(&installation.data_key_directory),
    )
    .await
    .expect_err("a shredded key must make the older backup's secret unreadable");
    assert!(
        error.to_string().contains("shredded"),
        "the refusal names the shred, not a generic failure: {error}"
    );
    // The live database also cannot read it, and the account row survived.
    let live = PgPool::connect(&installation.database.url).await.unwrap();
    assert!(
        secrets::load_secret_with_data_keys(
            &live,
            secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .is_err()
    );
    let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE id = $1")
        .bind(account)
        .fetch_one(&live)
        .await
        .unwrap();
    assert_eq!(still_there, 1);
    live.close().await;
    restored_pool.close().await;
}

// ---------------------------------------------------------------------------
// U36: an organization key shreds its own secrets and not an account's.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn organization_shred_is_scoped_to_its_own_secrets() {
    let installation = Installation::new("organization").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "org-owner", "owner@example.com").await;
    let organization = insert_organization(&pool, "shredded-org").await;

    let account_secret = b"an account secret that must survive";
    let organization_secret = b"an organization secret that must not";
    let account_secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "account/org-owner",
        account_secret,
    )
    .await;
    let organization_secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Organization,
        organization,
        "organization/shredded-org",
        organization_secret,
    )
    .await;

    let archive = installation.backup().await;
    pool.close().await;

    let report = data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Organization,
        organization,
    )
    .unwrap();
    assert!(report.had_key);
    assert_eq!(
        report.key_id,
        data_keys::organization_data_key_id(organization)
    );

    let destination = Database::reserve("organization-dest");
    let options = installation.restore_options(&archive, &destination, "organization");
    let restored = backup::run_restore(&options).await.unwrap();
    assert_eq!(restored.shredded_data_keys, 1);

    let restored_pool = PgPool::connect(&destination.url).await.unwrap();
    // The organization row is still present; only its secrets are gone.
    let present: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM organizations WHERE id = $1")
        .bind(organization)
        .fetch_one(&restored_pool)
        .await
        .unwrap();
    assert_eq!(present, 1);
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &restored_pool,
            account_secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        account_secret,
        "another owner's key is unaffected"
    );
    let error = secrets::load_secret_with_data_keys(
        &restored_pool,
        organization_secret_id,
        &keys,
        Some(&installation.data_key_directory),
    )
    .await
    .expect_err("the organization secret must be unreadable after its key is shredded");
    assert!(error.to_string().contains("shredded"), "{error}");
    restored_pool.close().await;
}

// ---------------------------------------------------------------------------
// U36: a backup predating the per-account keys cannot be shredded at all.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_pre_key_backup_cannot_be_shredded() {
    let installation = Installation::new("prekey").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "pre-key-user", "prekey@example.com").await;

    // A pre-key secret: wrapped directly by the installation key, with no
    // owner data key anywhere. This is the shape every M4.2 secret has.
    let secret = b"a pre-key secret that retention must bound";
    let secret_id = Uuid::new_v4();
    let sealed = secrets::seal(&keys, secret_id, secret).unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    secrets::insert_sealed(&mut tx, "legacy/pre-key", &sealed)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let archive = installation.backup().await;
    let manifest = fs::read_to_string({
        let root = installation.decrypt_archive(&archive);
        root.join("manifest.json")
    })
    .unwrap();
    let manifest: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        manifest["data_keys"].as_array().map(Vec::len),
        Some(0),
        "a pre-key archive carries no owner keys"
    );
    pool.close().await;

    let report = data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();
    assert!(!report.had_key, "there was no key to destroy");

    let destination = Database::reserve("prekey-dest");
    let options = installation.restore_options(&archive, &destination, "prekey");
    let restored = backup::run_restore(&options).await.unwrap();
    assert_eq!(
        restored.shredded_data_keys, 0,
        "a pre-key archive has nothing to shred"
    );
    assert!(
        restored.pre_key_archive,
        "restore reports that the archive predates the per-owner keys"
    );
    let restored_pool = PgPool::connect(&destination.url).await.unwrap();
    assert_eq!(
        secrets::load_secret_with_data_keys(&restored_pool, secret_id, &keys, None)
            .await
            .unwrap(),
        secret,
        "the pre-key secret is still readable; retention is the only control"
    );
    restored_pool.close().await;
}

// ---------------------------------------------------------------------------
// U36: restore distinguishes a missing key from a shredded one.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_refuses_a_missing_key_and_accepts_a_shredded_one() {
    let installation = Installation::new("missing").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "missing-key-user", "missing@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/missing-key-user",
        b"a secret behind a key that will go missing",
    )
    .await;
    let archive = installation.backup().await;
    pool.close().await;

    let key_id = data_keys::account_data_key_id(account);
    let key_path = installation
        .data_key_directory
        .join(format!("{key_id}.key"));
    // Remove the key without writing a tombstone: a lost key, not a deletion.
    fs::remove_file(&key_path).unwrap();

    let missing = Database::reserve("missing-dest");
    let options = installation.restore_options(&archive, &missing, "missing");
    let error = match backup::run_restore(&options).await {
        Ok(_) => panic!("restore must refuse a referenced owner key that is merely missing"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("missing") && error.contains(&key_id.to_string()),
        "{error}"
    );
    assert!(
        !missing.exists().await,
        "a failed restore leaves no destination database"
    );

    // Record the deletion. Now the same archive restores: the ciphertext is
    // unreadable by design, which is the completed shred.
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();
    let shredded = Database::reserve("shredded-dest");
    let options = installation.restore_options(&archive, &shredded, "shredded");
    let restored = backup::run_restore(&options).await.unwrap();
    assert_eq!(restored.shredded_data_keys, 1);
}

// ---------------------------------------------------------------------------
// U36: the archive carries key metadata, never key material.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_archive_carries_key_metadata_and_never_a_key() {
    let installation = Installation::new("manifest").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "manifest-user", "manifest@example.com").await;
    let key_id = data_keys::account_data_key_id(account);
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/manifest-user",
        b"a secret whose owner key must not enter the archive",
    )
    .await;
    let archive = installation.backup().await;
    pool.close().await;

    let root = installation.decrypt_archive(&archive);
    let manifest: Value = serde_json::from_slice(&read(&root.join("manifest.json"))).unwrap();
    let records = manifest["data_keys"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["key_id"], key_id.to_string());
    assert_eq!(record["owner_kind"], "account");
    assert_eq!(record["owner_id"], account.to_string());
    assert_eq!(record["algorithm"], secrets::ALGORITHM);
    assert_eq!(record["wrap_method"], keys.current().method());
    assert_eq!(record["shredded"], false);
    assert!(
        record.get("wrapped_key").is_none(),
        "the manifest carries no wrapped key"
    );

    // Nor does any file in the archive contain the owner key bytes, the
    // installation key bytes, or the descriptor's wrapped blob.
    let owner_key =
        data_keys::test_key_bytes(&keys, &installation.data_key_directory, key_id).unwrap();
    let installation_key = secrets::test_key_bytes(&installation.key_path).unwrap();
    let descriptor = fs::read_to_string(
        installation
            .data_key_directory
            .join(format!("{key_id}.key")),
    )
    .unwrap();
    let wrapped_blob = descriptor
        .lines()
        .find_map(|line| line.strip_prefix("wrapped_key = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap()
        .to_owned();
    for file in walk_files(&root) {
        let bytes = read(&file);
        assert!(
            !contains(&bytes, &owner_key),
            "{} contains the owner data key",
            file.display()
        );
        assert!(
            !contains(&bytes, &installation_key),
            "{} contains the installation key",
            file.display()
        );
        assert!(
            !contains(&bytes, wrapped_blob.as_bytes()),
            "{} contains the owner key's wrapped blob",
            file.display()
        );
    }
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

// ---------------------------------------------------------------------------
// U36: the live audit trail still resolves the actor after the shred.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn the_audit_chain_still_resolves_the_actor_after_shredding() {
    let installation = Installation::new("audit").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "audit-user", "audit@example.com").await;
    let event = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO audit_events (id, organization_id, actor_id, kind) \
         VALUES ($1, NULL, $2, 'account.deleted')",
    )
    .bind(event)
    .bind(account)
    .execute(&pool)
    .await
    .unwrap();
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/audit-user",
        b"a secret the deletion must shred",
    )
    .await;

    let mut conn = pool.acquire().await.unwrap();
    let before = audit::verify(&mut conn, None).await.unwrap();
    assert!(before.ok);
    drop(conn);

    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let after = audit::verify(&mut conn, None).await.unwrap();
    assert!(after.ok, "the hash chain verifies after the shred");
    assert_eq!(after.rows, before.rows, "no audit row was touched");
    let actor: Uuid = sqlx::query_scalar("SELECT actor_id FROM audit_events WHERE id = $1")
        .bind(event)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(actor, account, "the audit row still resolves to the row");
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(actor)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(username, "audit-user");
}

// ---------------------------------------------------------------------------
// The operator's status signal names the shred and does not call it damage.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn status_reports_a_shredded_owner_key_as_complete_not_damaged() {
    let installation = Installation::new("status").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "status-user", "status@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/status-user",
        b"a secret status must not call damaged",
    )
    .await;
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();

    let report =
        secrets::status_with_data_keys(&pool, Some(&keys), Some(&installation.data_key_directory))
            .await
            .unwrap();
    assert_eq!(report.shredded_owner_keys, 1);
    assert!(report.missing_owner_key_ids.is_empty());
    assert!(report.unreadable_key_ids.is_empty());
    assert!(
        report.rotation_complete,
        "a completed shred is not an incomplete rotation"
    );
    assert!(report.owner_keys.iter().any(|key| key.shredded));
    pool.close().await;
}

// ---------------------------------------------------------------------------
// C2: restore refuses a completed shred by default and names the keys; only an
// explicit --accept-shredded publishes the archive.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn restore_refuses_a_shredded_key_unless_accept_shredded_is_given() {
    let installation = Installation::new("accept").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "accept-user", "accept@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/accept-user",
        b"a secret behind a completed shred",
    )
    .await;
    let key_id = data_keys::account_data_key_id(account);
    let archive = installation.backup().await;
    pool.close().await;
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();

    let refused = Database::reserve("accept-refused");
    let options = installation.restore_options_with(&archive, &refused, "refused", false);
    let error = match backup::run_restore(&options).await {
        Ok(_) => panic!("restore must refuse a shredded key unless the operator accepts it"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("shredded") && error.contains(&key_id.to_string()),
        "the refusal names the shredded key: {error}"
    );
    assert!(
        !refused.exists().await,
        "a refused restore publishes no database"
    );

    let accepted = Database::reserve("accept-accepted");
    let options = installation.restore_options_with(&archive, &accepted, "accepted", true);
    let report = backup::run_restore(&options).await.unwrap();
    assert_eq!(report.shredded_data_keys, 1);
}

// ---------------------------------------------------------------------------
// The binding shared with the application half: a seed is wrapped by the
// account's owner key through `secret_data_keys.owner_key_id`, not by the row
// id that `accounts.data_key_id` would name in a shared-key design.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_seed_is_bound_to_the_account_owner_key_not_a_shared_row_id() {
    let installation = Installation::new("binding").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "binding-user", "binding@example.com").await;
    let secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/binding-user",
        b"a seed whose binding must survive the shred",
    )
    .await;
    let account_key = data_keys::account_data_key_id(account);
    let (row_id, owner_key_id, owner_kind, owner_id): (
        Uuid,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT s.data_key_id, d.owner_key_id, d.owner_kind, d.owner_id \
         FROM stored_secrets s JOIN secret_data_keys d ON d.id = s.data_key_id \
         WHERE s.id = $1",
    )
    .bind(secret_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        owner_key_id,
        Some(account_key),
        "the seed's per-secret key is wrapped by the account's owner key"
    );
    assert_ne!(
        Some(row_id),
        owner_key_id,
        "the per-secret data-key row is not the account key; the binding is owner_key_id"
    );
    assert_eq!(owner_kind.as_deref(), Some("account"));
    assert_eq!(owner_id, Some(account));

    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();
    assert!(
        secrets::load_secret_with_data_keys(
            &pool,
            secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .is_err(),
        "destroying the owner key makes the seed unreadable"
    );
    let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stored_secrets WHERE id = $1")
        .bind(secret_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        still_there, 1,
        "the ciphertext row and its reference remain"
    );
    pool.close().await;
}

// ---------------------------------------------------------------------------
// C4: backup refuses a lost key before publishing an archive.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn backup_refuses_a_lost_owner_key() {
    let installation = Installation::new("lost").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "lost-user", "lost@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/lost-user",
        b"a secret whose key will go missing",
    )
    .await;
    let key_id = data_keys::account_data_key_id(account);
    fs::remove_file(
        installation
            .data_key_directory
            .join(format!("{key_id}.key")),
    )
    .unwrap();
    pool.close().await;

    let output = installation.path.join("lost.ghbackup");
    let error = match backup::run_backup(
        &installation.config,
        None,
        &installation.backup_key,
        &output,
    )
    .await
    {
        Ok(_) => panic!("backup must refuse a referenced key that is missing without a tombstone"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("missing") && error.contains(&key_id.to_string()),
        "{error}"
    );
    assert!(!output.exists(), "a failed backup publishes no archive");
}

// ---------------------------------------------------------------------------
// C4: one account's shred leaves another account readable (account-vs-account).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn shredding_one_account_leaves_another_account_readable() {
    let installation = Installation::new("two-accounts").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let shredded = insert_account(&pool, "two-shredded", "two-shredded@example.com").await;
    let survivor = insert_account(&pool, "two-survivor", "two-survivor@example.com").await;
    let shredded_secret = b"the shredded account's seed";
    let survivor_secret = b"the surviving account's seed";
    let shredded_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        shredded,
        "totp/two-shredded",
        shredded_secret,
    )
    .await;
    let survivor_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        survivor,
        "totp/two-survivor",
        survivor_secret,
    )
    .await;

    let archive = installation.backup().await;
    pool.close().await;
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        shredded,
    )
    .unwrap();

    let destination = Database::reserve("two-accounts-dest");
    let options = installation.restore_options(&archive, &destination, "two-accounts");
    let restored = backup::run_restore(&options).await.unwrap();
    assert_eq!(restored.shredded_data_keys, 1);

    let restored_pool = PgPool::connect(&destination.url).await.unwrap();
    let error = secrets::load_secret_with_data_keys(
        &restored_pool,
        shredded_id,
        &keys,
        Some(&installation.data_key_directory),
    )
    .await
    .expect_err("the shredded account's key is gone");
    assert!(error.to_string().contains("shredded"), "{error}");
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &restored_pool,
            survivor_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        survivor_secret,
        "the other account's key and ciphertext are untouched"
    );
    restored_pool.close().await;
}

// ---------------------------------------------------------------------------
// C3/F2 and C4: a manifest marks a shred and records a pre-key tombstone
// honestly (`had_key = false`).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_manifest_marks_a_shred_and_never_pretends_a_pre_key_key_existed() {
    let installation = Installation::new("manifest-shred").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let keyed = insert_account(&pool, "manifest-keyed", "keyed@example.com").await;
    let pre_key = insert_account(&pool, "manifest-prekey", "prekey@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        keyed,
        "totp/manifest-keyed",
        b"a seed behind a key that is shredded",
    )
    .await;
    let keyed_id = data_keys::account_data_key_id(keyed);
    let pre_key_id = data_keys::account_data_key_id(pre_key);
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        keyed,
    )
    .unwrap();
    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        pre_key,
    )
    .unwrap();
    let archive = installation.backup().await;
    pool.close().await;

    let root = installation.decrypt_archive(&archive);
    let manifest: Value = serde_json::from_slice(&read(&root.join("manifest.json"))).unwrap();
    let records = manifest["data_keys"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    let keyed_record = records
        .iter()
        .find(|record| record["key_id"] == keyed_id.to_string())
        .unwrap();
    assert_eq!(keyed_record["shredded"], true);
    assert_eq!(keyed_record["had_key"], true);
    assert!(keyed_record.get("wrapped_key").is_none());
    let pre_key_record = records
        .iter()
        .find(|record| record["key_id"] == pre_key_id.to_string())
        .unwrap();
    assert_eq!(pre_key_record["shredded"], true);
    assert_eq!(
        pre_key_record["had_key"], false,
        "a pre-key tombstone must not pretend a key was destroyed"
    );
}

// ---------------------------------------------------------------------------
// C4: doctor fails on a referenced owner key that is missing without a
// tombstone, and names it.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn doctor_fails_on_a_missing_owner_key() {
    let installation = Installation::new("doctor-missing").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "doctor-missing-user", "doctor@example.com").await;
    store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/doctor-missing-user",
        b"a secret doctor must notice is unreadable",
    )
    .await;
    pool.close().await;
    let key_id = data_keys::account_data_key_id(account);
    fs::remove_file(
        installation
            .data_key_directory
            .join(format!("{key_id}.key")),
    )
    .unwrap();

    let config_path = installation.path.join("doctor.toml");
    fs::write(
        &config_path,
        format!(
            "database_url = \"{}\"\nstorage_root = \"{}\"\ninstallation_key_file = \"{}\"\n\
             data_key_directory = \"{}\"\nssh_enabled = false\ndatabase_timeout_seconds = 5\n",
            installation.database.url,
            installation.path.join("data/repositories").display(),
            installation.key_path.display(),
            installation.data_key_directory.display()
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_githearth-server"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GITHEARTH_") {
            command.env_remove(name);
        }
    }
    let output = command
        .args(["--config", config_path.to_str().unwrap(), "doctor"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains(&key_id.to_string()),
        "doctor names the missing owner key: {text}"
    );
    assert!(text.contains("missing"), "{text}");
}

// ---------------------------------------------------------------------------
// C1/M1: the honest limit. A retained custody copy that predates a deletion
// un-shreds the older archive; the tombstone is forward-only.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn an_older_custody_copy_unshreds_the_archive_and_the_current_directory_does_not() {
    let installation = Installation::new("custody-copy").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "custody-user", "custody@example.com").await;
    let secret = b"a seed an old custody copy can still open";
    let secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/custody-user",
        secret,
    )
    .await;
    let archive = installation.backup().await;
    pool.close().await;

    // The pre-deletion custody copy an operator might have kept.
    let custody_copy = installation.path.join("custody-copy");
    fs::create_dir(&custody_copy).unwrap();
    for entry in fs::read_dir(&installation.data_key_directory).unwrap() {
        let entry = entry.unwrap();
        if entry.path().is_file() {
            fs::copy(entry.path(), custody_copy.join(entry.file_name())).unwrap();
        }
    }

    data_keys::shred(
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
    )
    .unwrap();

    // The current directory: the shred holds.
    let current = Database::reserve("custody-current");
    let options = installation.restore_options(&archive, &current, "custody-current");
    assert_eq!(
        backup::run_restore(&options)
            .await
            .unwrap()
            .shredded_data_keys,
        1
    );
    let current_pool = PgPool::connect(&current.url).await.unwrap();
    assert!(
        secrets::load_secret_with_data_keys(
            &current_pool,
            secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .is_err(),
        "the current directory holds the tombstone and no key"
    );
    current_pool.close().await;

    // Restoring the older custody copy (key file back, tombstone absent)
    // un-shreds the same archive: that is the documented limit, not a bug in
    // the shred.
    fs::remove_dir_all(&installation.data_key_directory).unwrap();
    fs::create_dir(&installation.data_key_directory).unwrap();
    fs::set_permissions(
        &installation.data_key_directory,
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    for entry in fs::read_dir(&custody_copy).unwrap() {
        let entry = entry.unwrap();
        fs::copy(
            entry.path(),
            installation.data_key_directory.join(entry.file_name()),
        )
        .unwrap();
    }
    let older = Database::reserve("custody-older");
    let options = installation.restore_options(&archive, &older, "custody-older");
    assert_eq!(
        backup::run_restore(&options)
            .await
            .unwrap()
            .shredded_data_keys,
        0,
        "an older custody copy has neither the tombstone nor the removal"
    );
    let older_pool = PgPool::connect(&older.url).await.unwrap();
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &older_pool,
            secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        secret,
        "the older copy makes the older archive readable again"
    );
    older_pool.close().await;
}

// ---------------------------------------------------------------------------
// C4: a secret row that names another owner's key is refused, and both keys
// survive.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn a_secret_row_cannot_name_another_owners_key() {
    let installation = Installation::new("row-mismatch").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let first = insert_account(&pool, "row-first", "row-first@example.com").await;
    let second = insert_account(&pool, "row-second", "row-second@example.com").await;
    let first_secret = b"the first account's seed";
    let second_secret = b"the second account's seed";
    let first_secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        first,
        "totp/row-first",
        first_secret,
    )
    .await;
    let second_secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        second,
        "totp/row-second",
        second_secret,
    )
    .await;
    let first_key = data_keys::account_data_key_id(first);
    let second_key = data_keys::account_data_key_id(second);
    let first_data_key: Uuid =
        sqlx::query_scalar("SELECT data_key_id FROM stored_secrets WHERE id = $1")
            .bind(first_secret_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // An application bug (or a row written by hand) points the first secret's
    // data-key row at the second account's owner key.
    sqlx::query(
        "UPDATE secret_data_keys SET owner_key_id = $2, kek_id = $4, owner_kind = 'account', \
         owner_id = $3 WHERE id = $1",
    )
    .bind(first_data_key)
    .bind(second_key)
    .bind(first)
    .bind(second_key.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let error = secrets::load_secret_with_data_keys(
        &pool,
        first_secret_id,
        &keys,
        Some(&installation.data_key_directory),
    )
    .await
    .expect_err("a row naming another owner's key must be refused");
    assert!(error.to_string().contains("belongs to"), "{error}");
    assert!(
        installation
            .data_key_directory
            .join(format!("{first_key}.key"))
            .is_file()
    );
    assert!(
        installation
            .data_key_directory
            .join(format!("{second_key}.key"))
            .is_file()
    );
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &pool,
            second_secret_id,
            &keys,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        second_secret
    );
    pool.close().await;
}

// ---------------------------------------------------------------------------
// C4: installation-key rotation through rotate_with_data_keys re-wraps the
// owner key and leaves the secret readable under the new key alone.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rotation_rewraps_the_owner_key_through_the_shipped_path() {
    let installation = Installation::new("rotate-owner").await;
    let pool = installation.pool().await;
    let keys = installation.keys();
    let account = insert_account(&pool, "rotate-user", "rotate@example.com").await;
    let secret = b"a seed that survives installation-key rotation";
    let secret_id = store_secret(
        &pool,
        &keys,
        &installation.data_key_directory,
        data_keys::OwnerKind::Account,
        account,
        "totp/rotate-user",
        secret,
    )
    .await;

    let new_path = installation.path.join("installation.new.key");
    secrets::generate_raw_key_file(&new_path).unwrap();
    let new =
        secrets::InstallationKeySet::from_paths(&new_path, Some(&installation.key_path), None)
            .unwrap()
            .unwrap();
    let report =
        secrets::rotate_with_data_keys(&pool, &new, Some(&installation.data_key_directory), 256)
            .await
            .unwrap();
    assert_eq!(report.owner_keys_rewrapped, 1);
    assert_eq!(report.remaining, 0);

    let only_new = secrets::InstallationKeySet::from_paths(&new_path, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        secrets::load_secret_with_data_keys(
            &pool,
            secret_id,
            &only_new,
            Some(&installation.data_key_directory)
        )
        .await
        .unwrap(),
        secret,
        "the owner key moved to the new installation key; the secret did not change"
    );
    pool.close().await;
}
