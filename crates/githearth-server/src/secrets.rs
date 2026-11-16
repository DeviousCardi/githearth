//! Envelope encryption for stored secrets (ADR 0014 §3, M4.2 increment 7C).
//!
//! Each stored secret has its own random 32-byte data key. The data key is
//! wrapped by the operator's installation key (a *key encryption key*, KEK) and
//! recorded in `secret_data_keys`; the secret itself is encrypted under the
//! data key and recorded in `stored_secrets`. Only the wrapped data key is ever
//! stored, so a database dump contains no usable key and no plaintext secret.
//!
//! The installation key is read from an operator-owned file at start-up and is
//! never written to the database or to a backup. A backup manifest records its
//! key id, algorithm and wrap method, never the key.
//!
//! # Rotation
//!
//! Rotation wraps each data key again under a new installation key. The secret
//! ciphertext is never decrypted or rewritten: only the 32-byte data key moves
//! between wrappers, and `kek_id` and `wrapped_key` change in one row update.
//! While both the new key and the retired key are configured, every row is
//! readable; a request that already opened a data key finishes with it, and a
//! request that reads a row afterwards uses whichever wrapper the row names.

use crate::config::Config;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, bail};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    fmt, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const ALGORITHM: &str = "AES-256-GCM";
pub const KEY_FILE_VERSION: u32 = 1;
pub const RAW_WRAP_METHOD: &str = "file-aes256gcm";
pub const KDF_MEMORY_KIB: u32 = 65_536;
pub const KDF_ITERATIONS: u32 = 3;
pub const KDF_PARALLELISM: u32 = 1;
/// Parse-time bound for a key file. It is a key, not a document.
const KEY_FILE_LIMIT: u64 = 4096;
const DATA_KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
/// One backfill transaction encrypts at most this many plaintext rows.
pub const BACKFILL_BATCH: usize = 100;
/// One rotation transaction re-wraps at most this many data keys.
pub const ROTATE_BATCH: usize = 256;

const WRAP_DOMAIN: &[u8] = b"githearth-dek-wrap-v1\0";
const SECRET_DOMAIN: &[u8] = b"githearth-secret-v1\0";
const KEY_ID_DOMAIN: &[u8] = b"githearth-installation-key-id-v1\0";

tokio::task_local! {
    /// The installation key set for the task that is serving a request or
    /// running a background job. `event_with` reads it when it records a client
    /// IP digest; the set is scoped, not process-global, so tests that run
    /// several installations in one process cannot use each other's key.
    static TASK_KEYS: std::sync::Arc<InstallationKeySet>;
}

/// Runs `future` with the installation key set visible to `event_with`.
pub async fn scope<F>(keys: Option<std::sync::Arc<InstallationKeySet>>, future: F) -> F::Output
where
    F: std::future::Future,
{
    match keys {
        Some(keys) => TASK_KEYS.scope(keys, future).await,
        None => future.await,
    }
}

/// The key set visible on this task, if one was scoped.
pub fn current_keys() -> Option<std::sync::Arc<InstallationKeySet>> {
    TASK_KEYS.try_with(std::sync::Arc::clone).ok()
}

// ---------------------------------------------------------------------------
// Key files
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize, Serialize)]
struct KeyFile {
    version: u32,
    method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    salt: Option<String>,
    /// Recorded so a later parameter change cannot silently derive a different
    /// key from an old descriptor. Absent means the 7C defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_kib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parallelism: Option<u32>,
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyFile")
            .field("version", &self.version)
            .field("method", &self.method)
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("salt", &self.salt.as_ref().map(|_| "[redacted]"))
            .field("memory_kib", &self.memory_kib)
            .field("iterations", &self.iterations)
            .field("parallelism", &self.parallelism)
            .finish()
    }
}

/// One installation key: a KEK, its stable id, and the method that produced it.
pub struct InstallationKey {
    id: String,
    key: [u8; DATA_KEY_BYTES],
    method: String,
    source: PathBuf,
}

impl fmt::Debug for InstallationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallationKey")
            .field("id", &self.id)
            .field("method", &self.method)
            .field("source", &self.source)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl InstallationKey {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    fn wrap(&self, data_key_id: Uuid, data_key: &[u8; DATA_KEY_BYTES]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("invalid installation key length"))?;
        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: data_key,
                    aad: &wrap_aad(&self.id, &data_key_id, &self.method),
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot wrap a data key"))?;
        let mut wrapped = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        wrapped.extend_from_slice(&nonce);
        wrapped.extend_from_slice(&ciphertext);
        Ok(wrapped)
    }

    fn unwrap(
        &self,
        data_key_id: Uuid,
        method: &str,
        wrapped: &[u8],
    ) -> Result<[u8; DATA_KEY_BYTES]> {
        if method != self.method {
            bail!(
                "data key {} was wrapped with method {method}, but installation key {} uses {}",
                data_key_id,
                self.id,
                self.method
            );
        }
        if wrapped.len() != NONCE_BYTES + DATA_KEY_BYTES + 16 {
            bail!("wrapped data key {} has an invalid length", data_key_id);
        }
        let (nonce, ciphertext) = wrapped.split_at(NONCE_BYTES);
        let nonce: [u8; NONCE_BYTES] = nonce
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid wrapped-key nonce"))?;
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|_| anyhow::anyhow!("invalid installation key length"))?;
        let plaintext = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &wrap_aad(&self.id, &data_key_id, &self.method),
                },
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "installation key {} cannot unwrap data key {}: wrong key or damaged data",
                    self.id,
                    data_key_id
                )
            })?;
        plaintext
            .try_into()
            .map_err(|_| anyhow::anyhow!("data key {} is not 32 bytes", data_key_id))
    }
}

/// The current key plus the decrypt-only predecessor used during rotation.
pub struct InstallationKeySet {
    current: InstallationKey,
    previous: Vec<InstallationKey>,
}

impl fmt::Debug for InstallationKeySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallationKeySet")
            .field("current", &self.current)
            .field("previous", &self.previous)
            .field("keys", &"[redacted]")
            .finish()
    }
}

impl InstallationKeySet {
    /// Loads the configured key set. `Ok(None)` means no key is configured;
    /// callers decide whether that is allowed (a fresh installation) or fatal
    /// (an installation that already holds encrypted secrets).
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        if config.installation_key_method == "kms" {
            bail!(
                "installation_key_method = \"kms\" was requested, but no KMS adapter is built \
                 into this binary; use a file or passphrase installation key (ADR 0014 §3)"
            );
        }
        if config.installation_key_file.as_os_str().is_empty() {
            return Ok(None);
        }
        let current = load_key(
            &config.installation_key_file,
            config.installation_key_passphrase_file.as_deref(),
        )?;
        let previous = if config.installation_key_previous_file.as_os_str().is_empty() {
            Vec::new()
        } else {
            let key = load_key(
                &config.installation_key_previous_file,
                config.installation_key_passphrase_file.as_deref(),
            )?;
            if key.id == current.id {
                bail!(
                    "installation_key_file and installation_key_previous_file both contain key id \
                     {}; the predecessor must be a different key",
                    key.id
                );
            }
            vec![key]
        };
        Ok(Some(Self { current, previous }))
    }

    /// Loads a specific set, for `restore` and the tests. `None` means the
    /// current path was empty.
    pub fn from_paths(
        current: &Path,
        previous: Option<&Path>,
        passphrase_file: Option<&Path>,
    ) -> Result<Option<Self>> {
        if current.as_os_str().is_empty() {
            return Ok(None);
        }
        let current = load_key(current, passphrase_file)?;
        let previous = match previous.filter(|path| !path.as_os_str().is_empty()) {
            Some(path) => {
                let key = load_key(path, passphrase_file)?;
                if key.id == current.id {
                    bail!("the current and previous installation key files are the same key");
                }
                vec![key]
            }
            None => Vec::new(),
        };
        Ok(Some(Self { current, previous }))
    }

    pub fn current(&self) -> &InstallationKey {
        &self.current
    }

    pub fn get(&self, id: &str) -> Option<&InstallationKey> {
        if self.current.id == id {
            return Some(&self.current);
        }
        self.previous.iter().find(|key| key.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        std::iter::once(self.current.id.as_str())
            .chain(self.previous.iter().map(|key| key.id.as_str()))
            .collect()
    }

    pub fn record(&self) -> InstallationRecord {
        InstallationRecord {
            key_id: self.current.id.clone(),
            algorithm: ALGORITHM.into(),
            wrap_method: self.current.method.clone(),
        }
    }
}

/// What a backup manifest records about the installation key: an id, an
/// algorithm and a wrap method. Never key material.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallationRecord {
    pub key_id: String,
    pub algorithm: String,
    pub wrap_method: String,
}

fn wrap_aad(key_id: &str, data_key_id: &Uuid, method: &str) -> Vec<u8> {
    let mut aad = WRAP_DOMAIN.to_vec();
    aad.extend_from_slice(key_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(data_key_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(method.as_bytes());
    aad
}

fn secret_aad(secret_id: &Uuid, data_key_id: &Uuid) -> Vec<u8> {
    let mut aad = SECRET_DOMAIN.to_vec();
    aad.extend_from_slice(secret_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(data_key_id.as_bytes());
    aad
}

fn key_id(key: &[u8; DATA_KEY_BYTES]) -> String {
    let mut digest = Sha256::new();
    digest.update(KEY_ID_DOMAIN);
    digest.update(key);
    format!("sha256:{}", hex::encode(&digest.finalize()[..16]))
}

fn passphrase_method(memory_kib: u32, iterations: u32, parallelism: u32) -> String {
    format!("argon2id-m{memory_kib}-t{iterations}-p{parallelism}-aes256gcm")
}

/// Refuses a key file that group or others can read, that is a symlink, or that
/// another user owns. The check is made on both the key file and a passphrase
/// file; the error names the path and the reason, never the contents.
pub fn check_private_file(path: &Path, label: &str) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("{label} {} is missing", path.display())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect {label} {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "{label} {} is a symbolic link; use a regular file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("{label} {} is not a regular file", path.display());
    }
    let uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != uid {
        bail!(
            "{label} {} is owned by user id {}, not by user id {}",
            path.display(),
            metadata.uid(),
            uid
        );
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "{label} {} has mode {mode:04o}; group or others could read it. \
             Fix it with `chmod 0600 {}`",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

fn load_key(path: &Path, passphrase_file: Option<&Path>) -> Result<InstallationKey> {
    check_private_file(path, "installation key file")?;
    let bytes = read_limited(path)?;
    let text = std::str::from_utf8(&bytes).ok();
    if let Some(text) = text
        && let Ok(file) = toml::from_str::<KeyFile>(text)
        && file.version == KEY_FILE_VERSION
    {
        return match file.method.as_str() {
            "raw" => {
                let encoded = file
                    .key
                    .as_deref()
                    .context("installation key file is missing `key`")?;
                let key = decode_key_material(encoded)?;
                Ok(InstallationKey {
                    id: key_id(&key),
                    key,
                    method: RAW_WRAP_METHOD.into(),
                    source: path.to_path_buf(),
                })
            }
            "passphrase" => {
                let encoded_salt = file
                    .salt
                    .as_deref()
                    .context("passphrase installation key file is missing `salt`")?;
                let salt = BASE64
                    .decode(encoded_salt)
                    .context("installation key file has an invalid salt")?;
                if salt.len() != 16 {
                    bail!("installation key file salt must be 16 bytes");
                }
                let passphrase_path = passphrase_file.context(
                    "installation key file uses method \"passphrase\", but \
                     installation_key_passphrase_file is not set",
                )?;
                check_private_file(passphrase_path, "installation key passphrase file")?;
                let passphrase = read_limited(passphrase_path)?;
                let passphrase = trim_line_endings(passphrase);
                if passphrase.is_empty() {
                    bail!(
                        "installation key passphrase file {} is empty",
                        passphrase_path.display()
                    );
                }
                let memory_kib = file.memory_kib.unwrap_or(KDF_MEMORY_KIB);
                let iterations = file.iterations.unwrap_or(KDF_ITERATIONS);
                let parallelism = file.parallelism.unwrap_or(KDF_PARALLELISM);
                let key =
                    derive_passphrase(&passphrase, &salt, memory_kib, iterations, parallelism)?;
                Ok(InstallationKey {
                    id: key_id(&key),
                    key,
                    method: passphrase_method(memory_kib, iterations, parallelism),
                    source: path.to_path_buf(),
                })
            }
            other => bail!(
                "installation key file {} uses unsupported method {other:?}",
                path.display()
            ),
        };
    }
    // A bare key file: 32 raw bytes, 64 hex characters, or base64. This is the
    // form `secrets keygen --raw` writes and the form an operator gets from
    // `openssl rand -out key 32`.
    let key = decode_key_material(
        std::str::from_utf8(&bytes)
            .map(str::trim)
            .unwrap_or_default(),
    )
    .or_else(|_| {
        if bytes.len() == DATA_KEY_BYTES {
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("installation key must be 32 bytes"))
        } else {
            Err(anyhow::anyhow!(
                "installation key file {} is neither a versioned key file nor 32 raw bytes",
                path.display()
            ))
        }
    })?;
    Ok(InstallationKey {
        id: key_id(&key),
        key,
        method: RAW_WRAP_METHOD.into(),
        source: path.to_path_buf(),
    })
}

fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.len() > KEY_FILE_LIMIT {
        bail!("{} is larger than a key file", path.display());
    }
    fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

fn trim_line_endings(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        bytes.pop();
    }
    bytes
}

fn decode_key_material(value: &str) -> Result<[u8; DATA_KEY_BYTES]> {
    let value = value.trim();
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let bytes = hex::decode(value).context("installation key is not valid hex")?;
        return bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("installation key must be 32 bytes"));
    }
    let bytes = BASE64
        .decode(value)
        .context("installation key is not base64 or hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("installation key must be 32 bytes"))
}

fn derive_passphrase(
    passphrase: &[u8],
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<[u8; DATA_KEY_BYTES]> {
    let params = argon2::Params::new(memory_kib, iterations, parallelism, Some(DATA_KEY_BYTES))
        .map_err(|error| anyhow::anyhow!("invalid Argon2id parameters: {error}"))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; DATA_KEY_BYTES];
    argon
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|error| anyhow::anyhow!("cannot derive the installation key: {error}"))?;
    Ok(key)
}

/// Generates a raw installation key file (a versioned TOML file, mode 0600).
/// The parent directory must already exist. Never overwrites.
pub fn generate_raw_key_file(path: &Path) -> Result<()> {
    let mut key = [0u8; DATA_KEY_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
    let file = KeyFile {
        version: KEY_FILE_VERSION,
        method: "raw".into(),
        key: Some(BASE64.encode(key)),
        salt: None,
        memory_kib: None,
        iterations: None,
        parallelism: None,
    };
    write_new_private(path, toml::to_string(&file)?.as_bytes())
}

/// Generates a passphrase-derived installation key descriptor. The passphrase
/// itself stays in the operator's separately supplied passphrase file; only the
/// per-installation salt is written here.
pub fn generate_passphrase_key_file(path: &Path) -> Result<()> {
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    let file = KeyFile {
        version: KEY_FILE_VERSION,
        method: "passphrase".into(),
        key: None,
        salt: Some(BASE64.encode(salt)),
        memory_kib: Some(KDF_MEMORY_KIB),
        iterations: Some(KDF_ITERATIONS),
        parallelism: Some(KDF_PARALLELISM),
    };
    write_new_private(path, toml::to_string(&file)?.as_bytes())
}

fn write_new_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// A data key as it is stored: wrapped, with the id of the key that wrapped it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct WrappedDataKey {
    pub id: Uuid,
    pub kek_id: String,
    pub wrap_method: String,
    pub wrapped_key: Vec<u8>,
}

/// A sealed secret: the wrapped data key plus the secret ciphertext.
#[derive(Clone, Debug)]
pub struct SealedSecret {
    pub secret_id: Uuid,
    pub data_key_id: Uuid,
    pub kek_id: String,
    pub wrap_method: String,
    pub wrapped_key: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Encrypts `plaintext` under a fresh data key wrapped by the current
/// installation key. `secret_id` binds the ciphertext to its logical row; a
/// ciphertext moved to another row fails authentication instead of decrypting.
pub fn seal(keys: &InstallationKeySet, secret_id: Uuid, plaintext: &[u8]) -> Result<SealedSecret> {
    let mut data_key = [0u8; DATA_KEY_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut data_key);
    let data_key_id = Uuid::new_v4();
    let wrapped_key = keys.current.wrap(data_key_id, &data_key)?;
    let mut nonce = [0u8; NONCE_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let cipher = Aes256Gcm::new_from_slice(&data_key)
        .map_err(|_| anyhow::anyhow!("invalid data key length"))?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &secret_aad(&secret_id, &data_key_id),
            },
        )
        .map_err(|_| anyhow::anyhow!("cannot encrypt a secret"))?;
    Ok(SealedSecret {
        secret_id,
        data_key_id,
        kek_id: keys.current.id.clone(),
        wrap_method: keys.current.method.clone(),
        wrapped_key,
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

/// Opens a secret with a data-key row. The caller reads the row and the
/// ciphertext in one query, so a concurrent rotation cannot pair a new wrapper
/// with an old key id: the row update changes `kek_id` and `wrapped_key`
/// together.
pub fn open(
    keys: &InstallationKeySet,
    secret_id: Uuid,
    data_key: &WrappedDataKey,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let key = keys.get(&data_key.kek_id).context(format!(
        "no installation key with id {} is configured; the row was wrapped by a retired key",
        data_key.kek_id
    ))?;
    let data_key_bytes = key.unwrap(data_key.id, &data_key.wrap_method, &data_key.wrapped_key)?;
    if nonce.len() != NONCE_BYTES {
        bail!("secret {} has an invalid nonce", secret_id);
    }
    let nonce: [u8; NONCE_BYTES] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid secret nonce"))?;
    let cipher = Aes256Gcm::new_from_slice(&data_key_bytes)
        .map_err(|_| anyhow::anyhow!("invalid data key length"))?;
    cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: &secret_aad(&secret_id, &data_key.id),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "secret {} cannot be decrypted: wrong data key or damaged ciphertext",
                secret_id
            )
        })
}

// ---------------------------------------------------------------------------
// Database operations
// ---------------------------------------------------------------------------

/// A secret row as read from `stored_secrets` joined to its data key.
#[derive(Debug, sqlx::FromRow)]
pub struct StoredSecretRow {
    pub secret_id: Uuid,
    pub data_key_id: Uuid,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub kek_id: String,
    pub wrap_method: String,
    pub wrapped_key: Vec<u8>,
}

/// Inserts a sealed secret and its wrapped data key. The caller owns the
/// transaction, so the two rows and the caller's audit-key row commit together.
pub async fn insert_sealed(
    conn: &mut sqlx::PgConnection,
    name: &str,
    sealed: &SealedSecret,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO secret_data_keys (id, kek_id, algorithm, wrap_method, wrapped_key) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(sealed.data_key_id)
    .bind(&sealed.kek_id)
    .bind(ALGORITHM)
    .bind(&sealed.wrap_method)
    .bind(&sealed.wrapped_key)
    .execute(&mut *conn)
    .await
    .context("insert wrapped data key")?;
    sqlx::query(
        "INSERT INTO stored_secrets (id, name, data_key_id, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(sealed.secret_id)
    .bind(name)
    .bind(sealed.data_key_id)
    .bind(&sealed.nonce)
    .bind(&sealed.ciphertext)
    .execute(&mut *conn)
    .await
    .context("insert stored secret")?;
    Ok(())
}

/// Reads and decrypts a stored secret by id. This is a library read path used
/// by the migration check and the tests; no HTTP handler calls it.
pub async fn load_secret(
    pool: &PgPool,
    secret_id: Uuid,
    keys: &InstallationKeySet,
) -> Result<Vec<u8>> {
    let row: StoredSecretRow = sqlx::query_as(
        "SELECT s.id AS secret_id, s.data_key_id, s.nonce, s.ciphertext, \
                d.kek_id, d.wrap_method, d.wrapped_key \
         FROM stored_secrets s JOIN secret_data_keys d ON d.id = s.data_key_id \
         WHERE s.id = $1",
    )
    .bind(secret_id)
    .fetch_one(pool)
    .await
    .with_context(|| format!("cannot read stored secret {secret_id}"))?;
    let data_key = WrappedDataKey {
        id: row.data_key_id,
        kek_id: row.kek_id,
        wrap_method: row.wrap_method,
        wrapped_key: row.wrapped_key,
    };
    open(keys, row.secret_id, &data_key, &row.nonce, &row.ciphertext)
}

/// Decrypts an audit IP key by its `audit_ip_keys.id`. The migration and the
/// dump/archive tests use it; no API exposes it.
pub async fn load_audit_ip_key(
    pool: &PgPool,
    audit_key_id: Uuid,
    keys: &InstallationKeySet,
) -> Result<Vec<u8>> {
    let secret_id: Option<Uuid> =
        sqlx::query_scalar("SELECT secret_id FROM audit_ip_keys WHERE id = $1")
            .bind(audit_key_id)
            .fetch_one(pool)
            .await
            .with_context(|| format!("cannot read audit IP key {audit_key_id}"))?;
    let secret_id = secret_id.with_context(|| {
        format!("audit IP key {audit_key_id} is still plaintext; run `migrate` with the installation key")
    })?;
    load_secret(pool, secret_id, keys).await
}

#[derive(Debug, Default)]
pub struct StartupReport {
    pub data_keys: i64,
    pub secrets: i64,
    pub backfilled: u64,
    pub retired_key_ids: Vec<String>,
}

/// The start-up/migration check: encrypts any plaintext `audit_ip_keys.key`
/// rows, then proves the configured key set can unwrap **every** wrapped data
/// key. A missing or wrong key is an error naming the key id, never a warning
/// that lets the server start with unreadable secrets; one damaged wrapper is
/// one secret that would fail at use, so a sample per key id is not enough.
pub async fn startup(
    pool: &PgPool,
    keys: Option<&InstallationKeySet>,
    batch: usize,
) -> Result<StartupReport> {
    let schema: Option<String> = sqlx::query_scalar("SELECT to_regclass('secret_data_keys')::text")
        .fetch_one(pool)
        .await
        .context("cannot inspect the secrets schema")?;
    if schema.is_none() {
        bail!("the secrets schema is not applied; run `githearth-server migrate` first");
    }

    let legacy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("cannot count plaintext audit IP keys")?;
    let corrupt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NULL",
    )
    .fetch_one(pool)
    .await
    .context("cannot count audit IP keys")?;
    if corrupt > 0 {
        bail!(
            "{} audit IP key row(s) have neither a secret nor migration input; refusing to start",
            corrupt
        );
    }
    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NOT NULL AND key IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("cannot count audit IP key envelopes")?;
    if leaked > 0 {
        bail!(
            "{} audit IP key row(s) carry both an encrypted secret and a plaintext column; \
             refusing to start until the plaintext is cleared",
            leaked
        );
    }

    let mut backfilled = 0u64;
    if legacy > 0 {
        let keys = keys.context(
            "an installation key is required to encrypt existing plaintext audit IP keys; set \
             installation_key_file (ADR 0014 §3) and run `githearth-server migrate`",
        )?;
        backfilled = backfill_plaintext_audit_keys(pool, keys, batch.max(1)).await?;
    }

    // `backfill_plaintext_audit_keys` treats an empty `SKIP LOCKED` batch as
    // completion, which is only true when no other session holds a lock on a
    // remaining plaintext row. Re-count after the backfill so this is the one
    // source of truth: a dumped database must never still contain a plaintext
    // audit key just because another writer kept a row locked.
    let still_plaintext: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("cannot count plaintext audit IP keys after the backfill")?;
    if still_plaintext > 0 {
        bail!(
            "{still_plaintext} plaintext audit IP key(s) were locked by another session and could \
             not be encrypted; stop every other writer and re-run before migrating, serving or \
             backing up"
        );
    }

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kek_id, COUNT(*) FROM secret_data_keys GROUP BY kek_id ORDER BY kek_id",
    )
    .fetch_all(pool)
    .await
    .context("cannot count wrapped data keys")?;
    let secrets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stored_secrets")
        .fetch_one(pool)
        .await
        .context("cannot count stored secrets")?;

    let mut retired = Vec::new();
    if !rows.is_empty() {
        let keys = keys.context(
            "stored secrets need the installation key, but installation_key_file is not set; \
             refusing to start with unreadable secrets",
        )?;
        for (kek_id, _count) in &rows {
            if keys.get(kek_id).is_none() {
                bail!(
                    "stored secrets were wrapped by installation key {kek_id}, which is not \
                     configured; add it as installation_key_previous_file (rotation) or restore the \
                     correct installation_key_file"
                );
            }
            if kek_id != keys.current.id() {
                retired.push(kek_id.clone());
            }
        }
        // Every wrapper, not one sample per key id: a damaged second wrapper
        // would otherwise start the server and fail only when that secret is
        // used, which is exactly what "never starts with unreadable secrets"
        // forbids. Unwrapping is ~48 bytes of AES-GCM per row.
        let wrapped: Vec<WrappedDataKey> = sqlx::query_as(
            "SELECT id, kek_id, wrap_method, wrapped_key FROM secret_data_keys ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .context("cannot read the wrapped data keys")?;
        let mut failures = Vec::new();
        for row in &wrapped {
            let key = keys
                .get(&row.kek_id)
                .expect("every key id was checked above");
            if let Err(error) = key.unwrap(row.id, &row.wrap_method, &row.wrapped_key) {
                failures.push((row.id, row.kek_id.clone(), format!("{error:#}")));
            }
        }
        if let Some((id, kek_id, error)) = failures.first() {
            bail!(
                "{} of {} wrapped data key(s) cannot be opened with the configured installation \
                 key(s); first: data key {id} under {kek_id}: {error}",
                failures.len(),
                wrapped.len()
            );
        }
    }

    Ok(StartupReport {
        data_keys: rows.iter().map(|(_, count)| count).sum(),
        secrets,
        backfilled,
        retired_key_ids: retired,
    })
}

async fn backfill_plaintext_audit_keys(
    pool: &PgPool,
    keys: &InstallationKeySet,
    batch: usize,
) -> Result<u64> {
    let mut total = 0u64;
    loop {
        let mut tx = pool.begin().await?;
        let rows: Vec<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT id, key FROM audit_ip_keys \
             WHERE secret_id IS NULL AND key IS NOT NULL \
             ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(batch as i64)
        .fetch_all(&mut *tx)
        .await
        .context("cannot read plaintext audit IP keys")?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        for (id, plaintext) in &rows {
            if plaintext.len() != DATA_KEY_BYTES {
                bail!(
                    "plaintext audit IP key {id} is {} bytes, not 32; refusing to guess",
                    plaintext.len()
                );
            }
            let secret_id = Uuid::new_v4();
            let sealed = seal(keys, secret_id, plaintext)?;
            insert_sealed(&mut tx, &format!("audit_ip_key/{id}"), &sealed).await?;
            sqlx::query("UPDATE audit_ip_keys SET secret_id = $1, key = NULL WHERE id = $2")
                .bind(secret_id)
                .bind(id)
                .execute(&mut *tx)
                .await
                .context("update audit IP key envelope")?;
        }
        tx.commit().await?;
        total += rows.len() as u64;
        tracing::info!(
            rows = rows.len(),
            total,
            "encrypted plaintext audit IP keys"
        );
    }
    Ok(total)
}

#[derive(Debug, Default)]
pub struct RotationReport {
    pub rewrapped: u64,
    pub remaining: i64,
}

/// Re-wraps every data key that is not under the current installation key.
/// Secrets are not decrypted or rewritten; only the wrapped data key moves.
pub async fn rotate(
    pool: &PgPool,
    keys: &InstallationKeySet,
    batch: usize,
) -> Result<RotationReport> {
    let batch = batch.max(1);
    let mut rewrapped = 0u64;
    loop {
        let mut tx = pool.begin().await?;
        let rows: Vec<WrappedDataKey> = sqlx::query_as(
            "SELECT id, kek_id, wrap_method, wrapped_key FROM secret_data_keys \
             WHERE kek_id <> $1 ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED",
        )
        .bind(keys.current.id())
        .bind(batch as i64)
        .fetch_all(&mut *tx)
        .await
        .context("cannot read data keys to re-wrap")?;
        if rows.is_empty() {
            tx.commit().await?;
            break;
        }
        for row in &rows {
            let source = keys.get(&row.kek_id).with_context(|| {
                format!(
                    "data key {} is under installation key {}, which is not configured; add it as \
                     installation_key_previous_file before rotating",
                    row.id, row.kek_id
                )
            })?;
            let data_key = source.unwrap(row.id, &row.wrap_method, &row.wrapped_key)?;
            let wrapped = keys.current.wrap(row.id, &data_key)?;
            sqlx::query(
                "UPDATE secret_data_keys SET kek_id = $1, wrap_method = $2, wrapped_key = $3 \
                 WHERE id = $4",
            )
            .bind(keys.current.id())
            .bind(keys.current.method())
            .bind(&wrapped)
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .context("re-wrap data key")?;
        }
        tx.commit().await?;
        rewrapped += rows.len() as u64;
        tracing::info!(rows = rows.len(), rewrapped, "re-wrapped data keys");
    }
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM secret_data_keys WHERE kek_id <> $1")
            .bind(keys.current.id())
            .fetch_one(pool)
            .await
            .context("cannot count data keys still under a retired key")?;
    Ok(RotationReport {
        rewrapped,
        remaining,
    })
}

#[derive(Debug)]
pub struct KeyStatus {
    pub key_id: String,
    pub data_keys: i64,
    pub secrets: i64,
}

#[derive(Debug)]
pub struct StatusReport {
    pub current_key_id: Option<String>,
    pub keys: Vec<KeyStatus>,
    pub legacy_plaintext: i64,
    pub unknown_key_ids: Vec<String>,
    /// Key ids whose wrapped data keys cannot all be opened, usually a damaged
    /// wrapper or a wrong key with the same id (impossible in practice, so this
    /// is damage). `status`/`doctor` must not call these "complete".
    pub unreadable_key_ids: Vec<String>,
    pub rotation_complete: bool,
}

/// The operator's rotation signal: every data key is under the current key,
/// every key id is configured, and no plaintext audit key remains. It prints no
/// key material, only ids and counts.
pub async fn status(pool: &PgPool, keys: Option<&InstallationKeySet>) -> Result<StatusReport> {
    let legacy_plaintext: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_ip_keys WHERE secret_id IS NULL AND key IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("cannot count plaintext audit IP keys")?;
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT d.kek_id, COUNT(DISTINCT d.id), COUNT(s.id) \
         FROM secret_data_keys d LEFT JOIN stored_secrets s ON s.data_key_id = d.id \
         GROUP BY d.kek_id ORDER BY d.kek_id",
    )
    .fetch_all(pool)
    .await
    .context("cannot count wrapped data keys")?;
    let current = keys.map(|keys| keys.current.id().to_owned());
    let wrapped: Vec<WrappedDataKey> = sqlx::query_as(
        "SELECT id, kek_id, wrap_method, wrapped_key FROM secret_data_keys ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("cannot read the wrapped data keys for status")?;
    let mut unknown = Vec::new();
    let mut unreadable = Vec::new();
    let mut complete = legacy_plaintext == 0;
    let mut out = Vec::new();
    for (key_id, data_keys, secrets) in rows {
        let configured = keys.and_then(|keys| keys.get(&key_id));
        if configured.is_none() {
            unknown.push(key_id.clone());
            complete = false;
        }
        if let Some(key) = configured {
            // Every wrapper is checked, not a sample: a damaged second wrapper
            // is an unreadable secret and must not be reported as "rotation
            // complete". `startup` unwraps the same set and refuses to start.
            let broken = wrapped
                .iter()
                .filter(|row| row.kek_id == key_id)
                .filter(|row| {
                    key.unwrap(row.id, &row.wrap_method, &row.wrapped_key)
                        .is_err()
                })
                .count();
            if broken > 0 {
                unreadable.push(key_id.clone());
                complete = false;
            }
        }
        if current.as_deref() != Some(key_id.as_str()) {
            complete = false;
        }
        out.push(KeyStatus {
            key_id,
            data_keys,
            secrets,
        });
    }
    if out.is_empty() && current.is_none() {
        complete = true;
    }
    Ok(StatusReport {
        current_key_id: current,
        keys: out,
        legacy_plaintext,
        unknown_key_ids: unknown,
        unreadable_key_ids: unreadable,
        rotation_complete: complete,
    })
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Writes a temporary raw key file for a test. The caller owns the path and
/// removes it. It is deliberately a thin wrapper so tests do not have to know
/// the key-file format.
#[doc(hidden)]
pub fn test_key_file(directory: &Path, name: &str) -> Result<PathBuf> {
    let path = directory.join(format!("{name}-{}.key", Uuid::new_v4().simple()));
    generate_raw_key_file(&path)?;
    Ok(path)
}

/// Reads a `stored_secrets` row directly, for tests that need the ciphertext.
#[doc(hidden)]
pub async fn read_stored_secret(
    pool: &PgPool,
    secret_id: Uuid,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let row: (Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT d.wrapped_key, s.nonce, s.ciphertext FROM stored_secrets s \
         JOIN secret_data_keys d ON d.id = s.data_key_id WHERE s.id = $1",
    )
    .bind(secret_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// The 32-byte key behind a raw key file, for the dump/archive tests. It is a
/// read of the operator's file, not an API and not a production read path.
#[doc(hidden)]
pub fn test_key_bytes(path: &Path) -> Result<Vec<u8>> {
    Ok(load_key(path, None)?.key.to_vec())
}
