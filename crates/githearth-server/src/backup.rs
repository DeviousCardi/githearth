//! `backup` and `restore` with a proved write fence (M4.1 U13).
//!
//! A backup is a PostgreSQL custom-format dump, a tar of the storage root, the
//! effective configuration and the SSH host key. All four are listed in a
//! checksummed manifest, and the whole payload is encrypted with a
//! key/passphrase supplied by the operator on stdin or through a file. The
//! manifest names the key id, algorithm and wrap method; it never contains the
//! key.
//!
//! The fence is proved in [`crate::maintenance::prove`] and recorded both in
//! the command output and in `manifest.fence`. `restore` stages every part
//! outside the destination, verifies the manifest and database-to-storage
//! consistency, and only then publishes the storage directory, the database,
//! the configuration and the host key. A refusal or failure removes whatever
//! staging it created and leaves the destination untouched.

use crate::config::Config;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::{Context, Result, bail};
use argon2::Argon2;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use githearth_git::object_store::{FsObjectStore, ObjectProblemKind, ObjectRecord, ObjectStore};
use githearth_git::{journal, layout::Storage};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, PgPool, postgres::PgPoolOptions};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAGIC: &str = "GITHEARTH-BACKUP1";
/// Version 2 added the required object manifest; version 3 added the
/// installation-key record (`key_id`, algorithm and wrap method, never the key).
/// Older archives are still read; this binary only writes 3.
const FORMAT_VERSION: u32 = 3;
const MIN_FORMAT_VERSION: u32 = 1;
const CHUNK_BYTES: usize = 1 << 20;
const CIPHER: &str = "AES-256-GCM";
const KDF: &str = "Argon2id-v19";
const WRAP: &str = "argon2id-symmetric";
const KDF_MEMORY_KIB: u32 = 65_536;
const KDF_ITERATIONS: u32 = 3;

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryPosition {
    pub id: Uuid,
    pub status: String,
    pub object_format: String,
    pub journal_position: u64,
    pub journal_applied: u64,
    /// SHA-256 of the whole client-visible ref set (`ref\0oid`, sorted) read
    /// from the live repository at the recorded position, before the storage
    /// snapshot was taken. It is the expected ref set of the snapshot, the
    /// published archive and the restore, including refs that predate the live
    /// journal or were never journaled (an import or a fork). `None` in an
    /// archive written before this field existed; those are verified against
    /// the journal only.
    #[serde(default)]
    pub refs_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PostgresPosition {
    pub database: String,
    pub server_version: String,
    /// `pg_current_wal_lsn()` read inside the exported repeatable-read
    /// transaction. It is informational: the identity of the dump's position is
    /// `exported_snapshot`, and `restore` does not compare the LSN.
    /// Database-to-storage consistency is proved by the per-repository journal
    /// positions in `repositories`.
    pub dump_lsn: String,
    /// The exported repeatable-read snapshot `pg_dump --snapshot` used. It is
    /// the recorded position the repository journal positions were read at.
    #[serde(default)]
    pub exported_snapshot: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FenceRecord {
    pub kind: String,
    pub detail: String,
}

/// How the storage part was captured. `mode` is `copy-on-write` for the online
/// path and `maintenance-fence` for the M4.1 fallback; `mechanism` names the
/// filesystem operation or `whole-root-tar`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub mode: String,
    pub mechanism: String,
    /// The journal position is taken for every repository; this records
    /// whether the snapshot was verified against it.
    pub refs_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigPart {
    pub source: Option<String>,
    pub sha256: String,
    pub bytes: u64,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptionRecord {
    pub key_id: String,
    pub algorithm: String,
    pub kdf: String,
    pub wrap: String,
    pub kdf_memory_kib: u32,
    pub kdf_iterations: u32,
    pub salt: String,
    pub nonce: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u32,
    pub created_unix_seconds: u64,
    pub fence: FenceRecord,
    /// How the storage tree was captured. Absent in archives written before
    /// the online path existed; those were all whole-root tars under the fence.
    #[serde(default)]
    pub snapshot: Option<SnapshotRecord>,
    pub postgres: PostgresPosition,
    pub repositories: Vec<RepositoryPosition>,
    /// Every stored object's key, checksum and size (ADR 0014 §1). Empty for
    /// an archive taken before the object store existed.
    #[serde(default)]
    pub objects: Vec<ObjectRecord>,
    pub config: ConfigPart,
    pub ssh_host_key: Option<Part>,
    /// The installation key that wraps every stored secret: id, algorithm and
    /// wrap method, never key material (ADR 0014 §3). Absent in archives
    /// written before format 3.
    #[serde(default)]
    pub installation: Option<crate::secrets::InstallationRecord>,
    pub parts: Vec<Part>,
    pub encryption: EncryptionRecord,
    pub excluded: Vec<String>,
    pub notes: Vec<String>,
}

impl Manifest {
    pub fn part(&self, name: &str) -> Option<&Part> {
        self.parts.iter().find(|part| part.name == name)
    }
}

// ---------------------------------------------------------------------------
// Encrypted container
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchiveHeader {
    pub magic: String,
    pub format_version: u32,
    pub cipher: String,
    pub kdf: String,
    pub kdf_memory_kib: u32,
    pub kdf_iterations: u32,
    pub salt: String,
    pub nonce: String,
    pub key_id: String,
    pub key_check: String,
    pub plaintext_bytes: u64,
    pub created_unix_seconds: u64,
}

#[derive(Debug)]
pub enum ArchiveError {
    MissingKey,
    WrongKey { key_id: String },
    Corrupt(String),
    Incomplete { expected: u64, found: u64 },
    Io(std::io::Error),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingKey => write!(
                f,
                "no backup key supplied; pass --key-file or --key-stdin (never an argument)"
            ),
            Self::WrongKey { key_id } => write!(
                f,
                "wrong backup key for this archive (archive key id {key_id}); the archive was not \
                 changed"
            ),
            Self::Corrupt(reason) => write!(f, "backup archive is corrupt: {reason}"),
            Self::Incomplete { expected, found } => write!(
                f,
                "backup archive is incomplete: expected {expected} bytes, found {found}"
            ),
            Self::Io(error) => write!(f, "backup archive i/o error: {error}"),
        }
    }
}
impl std::error::Error for ArchiveError {}
impl From<std::io::Error> for ArchiveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct KeyMaterial {
    key: [u8; 32],
    salt: [u8; 16],
    nonce: [u8; 12],
    key_id: String,
    key_check: [u8; 32],
}

fn key_material(passphrase: &[u8]) -> Result<KeyMaterial, ArchiveError> {
    if passphrase.is_empty() {
        return Err(ArchiveError::MissingKey);
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let key = derive_key(passphrase, &salt)?;
    Ok(KeyMaterial {
        key,
        salt,
        nonce,
        key_id: key_id(&key),
        key_check: key_check(&key),
    })
}

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; 32], ArchiveError> {
    let params = argon2::Params::new(KDF_MEMORY_KIB, KDF_ITERATIONS, 1, Some(32))
        .map_err(|error| ArchiveError::Corrupt(format!("invalid KDF parameters: {error}")))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|error| ArchiveError::Corrupt(format!("key derivation failed: {error}")))?;
    Ok(key)
}

/// A stable identifier for the key, derived from it, never the key itself.
fn key_id(key: &[u8; 32]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"githearth-backup-key-id-v1");
    digest.update(key);
    let digest = digest.finalize();
    format!("sha256:{}", hex::encode(&digest[..16]))
}

fn key_check(key: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(b"githearth-backup-key-check-v1");
    mac.finalize().into_bytes().into()
}

fn chunk_nonce(base: &[u8; 12], index: u64) -> [u8; 12] {
    let mut nonce = *base;
    nonce[4..].copy_from_slice(&index.to_be_bytes());
    nonce
}

/// The AAD binds a chunk to its index. Chunk 0 additionally binds the
/// plaintext header bytes, so rewriting `plaintext_bytes`, `salt`, `key_check`,
/// `key_id`, `format_version` or the timestamp breaks authentication instead of
/// changing the failure class or silently changing the payload length.
fn chunk_aad(index: u64, header: &[u8]) -> Vec<u8> {
    let mut aad = format!("githearth-backup-v1:chunk:{index}").into_bytes();
    if index == 0 {
        aad.extend_from_slice(b":header:");
        aad.extend_from_slice(header);
    }
    aad
}

/// Encrypts `payload` to `output` through a private temporary file. The
/// caller's output path is only published after the whole stream is durable.
fn encrypt_with(payload: &Path, output: &Path, material: &KeyMaterial) -> Result<()> {
    let plaintext_bytes = fs::metadata(payload)?.len();
    let header = ArchiveHeader {
        magic: MAGIC.into(),
        format_version: FORMAT_VERSION,
        cipher: CIPHER.into(),
        kdf: KDF.into(),
        kdf_memory_kib: KDF_MEMORY_KIB,
        kdf_iterations: KDF_ITERATIONS,
        salt: BASE64.encode(material.salt),
        nonce: BASE64.encode(material.nonce),
        key_id: material.key_id.clone(),
        key_check: BASE64.encode(material.key_check),
        plaintext_bytes,
        created_unix_seconds: now_unix_seconds(),
    };
    let header_bytes = serde_json::to_vec(&header)?;
    let mut input = fs::File::open(payload)?;
    let temporary = output.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        writer.write_all(MAGIC.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.write_all(&header_bytes)?;
        writer.write_all(b"\n")?;
        let cipher = Aes256Gcm::new_from_slice(&material.key)
            .map_err(|_| anyhow::anyhow!("invalid encryption key length"))?;
        let mut buffer = vec![0u8; CHUNK_BYTES];
        let mut index = 0u64;
        loop {
            let read = read_filled(&mut input, &mut buffer)?;
            if read == 0 {
                break;
            }
            let nonce_bytes = chunk_nonce(&material.nonce, index);
            let nonce = Nonce::from(nonce_bytes);
            let ciphertext = cipher
                .encrypt(
                    &nonce,
                    aes_gcm::aead::Payload {
                        msg: &buffer[..read],
                        aad: &chunk_aad(index, &header_bytes),
                    },
                )
                .map_err(|_| anyhow::anyhow!("encryption failed"))?;
            writer.write_all(&(read as u32).to_le_bytes())?;
            writer.write_all(&ciphertext)?;
            index += 1;
        }
        writer.sync_all()?;
        Ok(())
    })();
    drop(writer);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, output)?;
    Ok(())
}

/// Decrypts a whole archive to `output`, returning its plaintext header.
pub fn decrypt_file(input: &Path, output: &Path, passphrase: &[u8]) -> Result<ArchiveHeader> {
    if passphrase.is_empty() {
        return Err(ArchiveError::MissingKey.into());
    }
    let file = fs::File::open(input)?;
    let mut reader = std::io::BufReader::new(file);
    let mut magic = String::new();
    read_line_limited(&mut reader, &mut magic)?;
    if magic != MAGIC {
        return Err(
            ArchiveError::Corrupt(format!("unrecognized header (expected {MAGIC})")).into(),
        );
    }
    let mut header_text = String::new();
    read_line_limited(&mut reader, &mut header_text)?;
    let header: ArchiveHeader = serde_json::from_str(&header_text)
        .map_err(|_| ArchiveError::Corrupt("the unencrypted header is not valid JSON".into()))?;
    if !(MIN_FORMAT_VERSION..=FORMAT_VERSION).contains(&header.format_version) {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported archive format version {} (this binary reads {MIN_FORMAT_VERSION}..={FORMAT_VERSION})",
            header.format_version
        ))
        .into());
    }
    if header.magic != MAGIC {
        return Err(ArchiveError::Corrupt("unrecognized archive magic".into()).into());
    }
    let salt = BASE64
        .decode(&header.salt)
        .map_err(|_| ArchiveError::Corrupt("invalid KDF salt".into()))?;
    let nonce_bytes: [u8; 12] = BASE64
        .decode(&header.nonce)
        .map_err(|_| ArchiveError::Corrupt("invalid nonce".into()))?
        .try_into()
        .map_err(|_| ArchiveError::Corrupt("invalid nonce length".into()))?;
    let expected_check = BASE64
        .decode(&header.key_check)
        .map_err(|_| ArchiveError::Corrupt("invalid key check".into()))?;
    let key = derive_key(passphrase, &salt)?;
    let actual_check = key_check(&key);
    if !constant_time_eq(&expected_check, &actual_check) {
        return Err(ArchiveError::WrongKey {
            key_id: header.key_id.clone(),
        }
        .into());
    }
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("invalid encryption key length"))?;
    let temporary = output.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        let mut written = 0u64;
        let mut index = 0u64;
        loop {
            if written == header.plaintext_bytes {
                // Any trailing byte means the archive was modified.
                let mut trailing = [0u8; 1];
                if reader.read(&mut trailing)? != 0 {
                    return Err(
                        ArchiveError::Corrupt("trailing data after the payload".into()).into(),
                    );
                }
                break;
            }
            let mut length = [0u8; 4];
            read_exact_or_incomplete(&mut reader, &mut length, written, header.plaintext_bytes)?;
            let plaintext_len = u32::from_le_bytes(length) as usize;
            if plaintext_len == 0 || plaintext_len > CHUNK_BYTES {
                return Err(ArchiveError::Corrupt("invalid chunk length".into()).into());
            }
            let mut ciphertext = vec![0u8; plaintext_len + 16];
            read_exact_or_incomplete(
                &mut reader,
                &mut ciphertext,
                written,
                header.plaintext_bytes,
            )?;
            let nonce = Nonce::from(chunk_nonce(&nonce_bytes, index));
            let plaintext = cipher
                .decrypt(
                    &nonce,
                    aes_gcm::aead::Payload {
                        msg: &ciphertext,
                        aad: &chunk_aad(index, header_text.as_bytes()),
                    },
                )
                .map_err(|_| {
                    ArchiveError::Corrupt(format!(
                        "authentication failed at chunk {index} (right key, damaged ciphertext)"
                    ))
                })?;
            writer.write_all(&plaintext)?;
            written += plaintext.len() as u64;
            if written > header.plaintext_bytes {
                return Err(ArchiveError::Corrupt("payload longer than its header".into()).into());
            }
            index += 1;
        }
        writer.sync_all()?;
        Ok(())
    })();
    drop(writer);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, output)?;
    Ok(header)
}

fn read_filled(reader: &mut dyn Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn read_line_limited(
    reader: &mut dyn std::io::BufRead,
    out: &mut String,
) -> Result<(), ArchiveError> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(ArchiveError::Incomplete {
            expected: 1,
            found: 0,
        });
    }
    if line.len() > 1 << 20 {
        return Err(ArchiveError::Corrupt("header line is too long".into()));
    }
    if line.ends_with(b"\n") {
        line.pop();
    }
    *out =
        String::from_utf8(line).map_err(|_| ArchiveError::Corrupt("header is not UTF-8".into()))?;
    Ok(())
}

fn read_exact_or_incomplete(
    reader: &mut dyn Read,
    buffer: &mut [u8],
    written: u64,
    expected: u64,
) -> Result<(), ArchiveError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(ArchiveError::Incomplete {
                    expected,
                    found: written,
                });
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ArchiveError::Io(error)),
        }
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The storage tar carries `storage_root` verbatim, so private operator files
/// inside it would travel in every backup. The check lives in
/// [`Config::validate`] so `serve` and `migrate` refuse at start too.
pub fn read_key_file(path: &Path) -> Result<Vec<u8>> {
    let mut key =
        fs::read(path).with_context(|| format!("cannot read key file {}", path.display()))?;
    while key
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        key.pop();
    }
    if key.is_empty() {
        bail!("key file {} is empty", path.display());
    }
    Ok(key)
}

pub fn read_key_stdin() -> Result<Vec<u8>> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        bail!(
            "stdin is a terminal, so a key typed interactively would be echoed; \
             pass --key-file or pipe the key to --key-stdin"
        );
    }
    let mut key = Vec::new();
    std::io::stdin().take(1 << 16).read_to_end(&mut key)?;
    while key
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        key.pop();
    }
    if key.is_empty() {
        bail!("no key was read from stdin");
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

struct Staging {
    path: PathBuf,
}

impl Staging {
    fn new(parent: &Path, prefix: &str) -> Result<Self> {
        fs::create_dir_all(parent)?;
        let path = parent.join(format!("{prefix}-{}.tmp", Uuid::new_v4().simple()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Tar helpers
// ---------------------------------------------------------------------------

fn tar_command(action: &str) -> Command {
    let mut command = Command::new("tar");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("LC_ALL", "C")
        .arg(action);
    command
}

fn create_tar(output: &Path, root: &Path, excludes: &[&str]) -> Result<()> {
    let mut command = tar_command("-cf");
    command.arg(output).arg("-C").arg(root);
    for exclude in excludes {
        command.arg("--exclude").arg(exclude);
    }
    command.arg(".");
    let result = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run tar")?;
    if !result.status.success() {
        bail!(
            "tar failed while creating {}: {}",
            output.display(),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(())
}

fn extract_tar(archive: &Path, destination: &Path) -> Result<()> {
    let listing = tar_command("-tf")
        .arg(archive)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run tar")?;
    if !listing.status.success() {
        return Err(ArchiveError::Corrupt(format!(
            "cannot list the archive's tar: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        ))
        .into());
    }
    for entry in String::from_utf8_lossy(&listing.stdout).lines() {
        let path = Path::new(entry);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(ArchiveError::Corrupt(format!("unsafe tar entry {entry:?}")).into());
        }
    }
    // `-t` prints only names, so a symlink or hard link looks like an ordinary
    // entry. The verbose listing names the type in its first column; the
    // storage tree holds only regular files and directories, so anything else
    // (`l` symlink, `h` hard link, device, fifo) is refused rather than
    // extracted and published.
    let types = tar_command("-tvf")
        .arg(archive)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run tar")?;
    if !types.status.success() {
        return Err(ArchiveError::Corrupt(format!(
            "cannot list the archive's tar types: {}",
            String::from_utf8_lossy(&types.stderr).trim()
        ))
        .into());
    }
    for line in String::from_utf8_lossy(&types.stdout).lines() {
        let kind = line.chars().next().unwrap_or('?');
        if kind != '-' && kind != 'd' {
            return Err(ArchiveError::Corrupt(format!(
                "unsafe tar entry type {kind:?} (only regular files and directories are allowed): {line}"
            ))
            .into());
        }
    }
    fs::create_dir_all(destination)?;
    let result = tar_command("-xf")
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run tar")?;
    if !result.status.success() {
        return Err(ArchiveError::Corrupt(format!(
            "cannot extract the archive's tar: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Copy-on-write storage snapshot (online path)
// ---------------------------------------------------------------------------

const SNAPSHOT_PREFIX: &str = ".githearth-backup-snapshot-";

fn copy_on_write_mechanism() -> &'static str {
    if cfg!(target_os = "macos") {
        "clonefile"
    } else {
        "reflink"
    }
}

fn clone_command() -> Command {
    let mut command = Command::new("cp");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("LC_ALL", "C");
    if cfg!(target_os = "macos") {
        command.arg("-c");
    } else {
        command.arg("--reflink=always");
    }
    command.arg("-R");
    command
}

fn clone_path(source: &Path, destination: &Path) -> Result<()> {
    let output = clone_command()
        .arg(source)
        .arg(destination)
        .output()
        .context("cannot run the copy-on-write clone command")?;
    if !output.status.success() {
        bail!(
            "copy-on-write clone failed for {}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Probes the storage filesystem by cloning a small file. A filesystem without
/// copy-on-write support makes the clone fail (or copy rather than share
/// blocks); only a successful clone selects the online path. Any probe failure
/// — including one creating or writing the probe itself — is "unsupported", so
/// `auto` falls back to the proved fence instead of aborting the backup.
pub fn copy_on_write_supported(storage_root: &Path) -> Result<bool> {
    match probe_copy_on_write(storage_root) {
        Ok(supported) => Ok(supported),
        Err(error) => {
            eprintln!(
                "storage snapshot: the copy-on-write probe could not run ({error:#}); using the \
                 maintenance fence"
            );
            Ok(false)
        }
    }
}

fn probe_copy_on_write(storage_root: &Path) -> Result<bool> {
    let probe = storage_root.join(format!(
        "{SNAPSHOT_PREFIX}probe-{}",
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&probe).with_context(|| {
        format!(
            "cannot create the copy-on-write probe directory under {}",
            storage_root.display()
        )
    })?;
    let outcome = (|| -> Result<bool> {
        let source = probe.join("source");
        fs::write(&source, b"githearth copy-on-write probe")?;
        Ok(clone_path(&source, &probe.join("clone")).is_ok())
    })();
    let _ = fs::remove_dir_all(&probe);
    outcome
}

/// Removes snapshot trees left behind by a backup that crashed or was killed.
/// Only entries older than a day are swept: a live backup may be cloning for a
/// long time, and deleting its clone would corrupt the archive it is writing.
fn sweep_stale_snapshots(storage_root: &Path) {
    let Ok(entries) = fs::read_dir(storage_root) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SNAPSHOT_PREFIX)
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A clone of the storage root on the same filesystem, removed on drop. It is
/// a direct child of the storage root so a clone never crosses a mount, and
/// its name is excluded from the clone and from the fallback tar.
struct SnapshotTree {
    path: PathBuf,
}

impl SnapshotTree {
    fn create(storage_root: &Path) -> Result<Self> {
        let path = storage_root.join(format!("{SNAPSHOT_PREFIX}{}", Uuid::new_v4().simple()));
        fs::create_dir(&path).with_context(|| {
            format!(
                "cannot create the storage snapshot directory under {}",
                storage_root.display()
            )
        })?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self { path })
    }
}

impl Drop for SnapshotTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Clone the parts of the storage root that the archive carries. Sockets and
/// scratch trees under `run/`, `tmp/`, `hooks/` and the object store's own
/// locks/tmp/usage/uploads are skipped exactly as the fallback tar skips them.
fn copy_on_write_snapshot(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "run" | "tmp" | "hooks" | "locks")
            || name.starts_with(SNAPSHOT_PREFIX)
        {
            continue;
        }
        if name == "objects" {
            let objects = destination.join("objects");
            fs::create_dir(&objects)?;
            for entry in fs::read_dir(entry.path())? {
                let entry = entry?;
                let child = entry.file_name();
                let child = child.to_string_lossy();
                if matches!(child.as_ref(), "tmp" | "locks" | "usage" | "uploads") {
                    continue;
                }
                clone_path(&entry.path(), &objects.join(entry.file_name()))?;
            }
        } else {
            clone_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

/// The canonical digest of a client-visible ref set: SHA-256 over sorted
/// `ref\0oid\0` entries. It is the expected ref set of the snapshot, so a ref
/// that disappears between the recorded position and the clone changes it,
/// whether or not a journal record named that ref.
fn refs_digest(refs: &HashMap<String, githearth_core::ObjectId>) -> String {
    let mut entries: Vec<(&str, String)> = refs
        .iter()
        .map(|(name, oid)| (name.as_str(), oid.to_string()))
        .collect();
    entries.sort();
    let mut digest = Sha256::new();
    digest.update(b"githearth-refs-v1");
    for (name, oid) in entries {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(oid.as_bytes());
        digest.update([0]);
    }
    format!("sha256:{}", hex::encode(digest.finalize()))
}

/// Records the ref digest of every repository from a live storage root. It is
/// called under the fence or the online gate, before anything is cloned, so it
/// is the expected ref set of the snapshot that follows.
fn record_ref_digests(storage_root: &Path, positions: &mut [RepositoryPosition]) -> Result<()> {
    let storage = Storage::open_existing(storage_root)?;
    for position in positions.iter_mut() {
        let path = storage.repository_path(position.id);
        let refs = if path.is_dir() {
            githearth_git::layout::read_refs(&path).map_err(|error| {
                anyhow::anyhow!(
                    "repository {} refs cannot be read at the recorded position: {error}",
                    position.id
                )
            })?
        } else {
            HashMap::new()
        };
        position.refs_digest = Some(refs_digest(&refs));
    }
    Ok(())
}

/// Proves that every client-visible ref at the recorded position is present in
/// the storage snapshot with the value it had. Two independent checks: the ref
/// digest recorded from the live root before the snapshot, and — for a
/// repository with a journal — every journaled update replayed to the recorded
/// position. The digest is what covers a ref with no journal record (a pre-0005
/// ref, an import or a fork); the replay names the offending journal entry.
fn verify_snapshot_refs(storage_root: &Path, positions: &[RepositoryPosition]) -> Result<()> {
    let storage = Storage::open_existing(storage_root)?;
    for position in positions {
        let path = storage.repository_path(position.id);
        if !path.is_dir() {
            if position.status == "active" {
                bail!(
                    "repository {} is active but the storage snapshot has no directory",
                    position.id
                );
            }
            continue;
        }
        let actual_position = journal::verify(&path).map_err(|error| {
            anyhow::anyhow!(
                "repository {} journal in the snapshot does not verify: {error}",
                position.id
            )
        })?;
        if actual_position != position.journal_position {
            bail!(
                "repository {} journal is at {} in the storage snapshot, but the recorded \
                 position is {}",
                position.id,
                actual_position,
                position.journal_position
            );
        }
        let refs_now = githearth_git::layout::read_refs(&path).map_err(|error| {
            anyhow::anyhow!(
                "repository {} refs in the snapshot cannot be read: {error}",
                position.id
            )
        })?;
        if position.journal_position > 0 {
            verify_journaled_refs(position, &path, &refs_now)?;
        }
        if let Some(expected) = &position.refs_digest {
            let actual = refs_digest(&refs_now);
            if &actual != expected {
                bail!(
                    "repository {} client-visible refs in the storage snapshot are {actual}, but \
                     the backup recorded {expected} at journal position {}; a ref that existed \
                     when the position was taken is missing or moved (the snapshot holds {} ref(s))",
                    position.id,
                    position.journal_position,
                    refs_now.len()
                );
            }
        }
    }
    Ok(())
}

/// Replays one repository's journal up to its recorded position and requires
/// every committed update, including deletion, to be the value in `refs_now`.
fn verify_journaled_refs(
    position: &RepositoryPosition,
    path: &Path,
    refs_now: &HashMap<String, githearth_core::ObjectId>,
) -> Result<()> {
    let journal = githearth_git::journal::Journal::open(path).map_err(|error| {
        anyhow::anyhow!(
            "repository {} journal in the snapshot cannot be read: {error}",
            position.id
        )
    })?;
    let records: Vec<githearth_git::journal::Record> = journal
        .records_after(0)
        .filter(|record| record.sequence <= position.journal_position)
        .cloned()
        .collect();
    let mut expected: HashMap<String, githearth_core::ObjectId> = HashMap::new();
    let mut deleted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut index = 0;
    while index < records.len() {
        let prepared = &records[index];
        if prepared.kind != githearth_git::journal::Kind::Prepared {
            index += 1;
            continue;
        }
        let marker = match records.get(index + 1) {
            Some(next)
                if next.kind != githearth_git::journal::Kind::Prepared
                    && next.operation == prepared.operation =>
            {
                Some(next.kind)
            }
            _ => None,
        };
        match githearth_git::journal::classify(prepared, marker, refs_now) {
            githearth_git::journal::Outcome::Committed
            | githearth_git::journal::Outcome::CommittedMarkerLost => {
                for update in &prepared.updates {
                    if update.new.bytes().all(|byte| byte == b'0') {
                        expected.remove(&update.reference);
                        deleted.insert(update.reference.clone());
                    } else {
                        let oid = update.new.parse().map_err(|_| {
                            anyhow::anyhow!(
                                "repository {} journal names an invalid object id {} for {}",
                                position.id,
                                update.new,
                                update.reference
                            )
                        })?;
                        deleted.remove(&update.reference);
                        expected.insert(update.reference.clone(), oid);
                    }
                }
            }
            githearth_git::journal::Outcome::Aborted
            | githearth_git::journal::Outcome::AbortedMarkerLost => {}
            githearth_git::journal::Outcome::Diverged => bail!(
                "repository {} journal diverged at sequence {}; the snapshot cannot be verified",
                position.id,
                prepared.sequence
            ),
        }
        index += if marker.is_some() { 2 } else { 1 };
    }
    for (reference, oid) in &expected {
        if refs_now.get(reference) != Some(oid) {
            bail!(
                "repository {} ref {reference} is at {} in the storage snapshot, but the journal \
                 at position {} committed {oid}",
                position.id,
                refs_now
                    .get(reference)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "(absent)".to_owned()),
                position.journal_position
            );
        }
    }
    for reference in &deleted {
        if refs_now.contains_key(reference) {
            bail!(
                "repository {} ref {reference} was deleted by journal position {}, but it is still \
                 present in the storage snapshot",
                position.id,
                position.journal_position
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PostgreSQL helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PgUrl {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: String,
    params: Vec<(String, String)>,
}

fn parse_pg_url(url: &str) -> Result<PgUrl> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| anyhow::anyhow!("database URL must be a PostgreSQL URL"))?;
    let (authority, path_and_query) = match rest.split_once('/') {
        Some((authority, rest)) => (authority, rest),
        None => (rest, ""),
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let database = percent_decode(path);
    if database.is_empty() {
        bail!("database URL names no database");
    }
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(userinfo) => match userinfo.split_once(':') {
            Some((user, password)) => (Some(percent_decode(user)), Some(percent_decode(password))),
            None => (Some(percent_decode(userinfo)), None),
        },
        None => (None, None),
    };
    let (host, port) = if hostport.is_empty() {
        (None, None)
    } else if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 literal: [::1]:5432
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("invalid IPv6 host in database URL"))?;
        let port = rest
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok());
        (Some(host.to_owned()), port)
    } else if let Some((host, port)) = hostport.rsplit_once(':') {
        // A bare host without a port has no colon; a colon means a port.
        if port.chars().all(|c| c.is_ascii_digit()) {
            (Some(host.to_owned()), port.parse::<u16>().ok())
        } else {
            (Some(hostport.to_owned()), None)
        }
    } else {
        (Some(hostport.to_owned()), None)
    };
    let mut params = Vec::new();
    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            params.push((percent_decode(key), percent_decode(value)));
        }
    }
    Ok(PgUrl {
        host,
        port,
        user,
        password,
        database,
        params,
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pg_command(url: &PgUrl, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("LC_ALL", "C")
        .env("PGCONNECT_TIMEOUT", "10")
        .env("PGAPPNAME", "githearth-backup")
        .env("PGDATABASE", &url.database)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(host) = &url.host {
        command.env("PGHOST", host);
    }
    if let Some(port) = url.port {
        command.env("PGPORT", port.to_string());
    }
    if let Some(user) = &url.user {
        command.env("PGUSER", user);
    }
    if let Some(password) = &url.password {
        command.env("PGPASSWORD", password);
    }
    for (key, value) in &url.params {
        let name = match key.as_str() {
            "sslmode" => "PGSSLMODE",
            "sslcert" => "PGSSLCERT",
            "sslkey" => "PGSSLKEY",
            "sslrootcert" => "PGSSLROOTCERT",
            "connect_timeout" => "PGCONNECT_TIMEOUT",
            "application_name" => "PGAPPNAME",
            "options" => "PGOPTIONS",
            _ => continue,
        };
        command.env(name, value);
    }
    command
}

fn run_pg(command: &mut Command, what: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("cannot run {what}"))?;
    if !output.status.success() {
        // Never repeat the connection URL. The tool's own diagnostic names the
        // SQL object or the PostgreSQL error, not the credentials.
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.lines().last().unwrap_or("no diagnostic").trim();
        bail!("{what} failed: {detail}");
    }
    Ok(())
}

fn dump_database(url: &str, path: &Path, exported_snapshot: Option<&str>) -> Result<()> {
    let url = parse_pg_url(url)?;
    let mut command = pg_command(&url, "pg_dump");
    command
        .args(["--format=custom", "--no-owner", "--no-acl", "--file"])
        .arg(path);
    if let Some(snapshot) = exported_snapshot {
        // `pg_export_snapshot()`'s id includes spaces and commas; pg_dump
        // accepts it as one `--snapshot=<id>` argument.
        command.arg(format!("--snapshot={snapshot}"));
    }
    run_pg(&mut command, "pg_dump")
}

fn restore_database(url: &str, path: &Path) -> Result<()> {
    let url = parse_pg_url(url)?;
    let mut command = pg_command(&url, "pg_restore");
    // pg_restore insists on a database name; the name is not a secret and the
    // host, user and password still come from the environment.
    command
        .args(["--no-owner", "--no-acl", "--exit-on-error", "--dbname"])
        .arg(&url.database)
        .arg(path);
    run_pg(&mut command, "pg_restore")
}

/// Whether the named destination database exists. Shared with export import,
/// which refuses a destination that `restore` would too.
pub(crate) async fn destination_database_exists(url: &str, name: &str) -> Result<bool> {
    database_exists(url, name).await
}

async fn database_exists(url: &str, name: &str) -> Result<bool> {
    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection URL"))?
        .database("postgres")
        .disable_statement_logging();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .map_err(|_| {
            anyhow::anyhow!("cannot connect to PostgreSQL; check database availability")
        })?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(name)
            .fetch_one(&pool)
            .await?;
    pool.close().await;
    Ok(exists)
}

/// Create the destination database for a fresh import. Shared with `restore`.
/// Returns whether this call created it: a concurrent create is reported as
/// `false`, and the caller must not drop a database it did not create.
pub(crate) async fn destination_create_database(url: &str, name: &str) -> Result<bool> {
    create_database(url, name).await
}

async fn create_database(url: &str, name: &str) -> Result<bool> {
    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection URL"))?
        .database("postgres")
        .disable_statement_logging();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .map_err(|_| {
            anyhow::anyhow!("cannot connect to PostgreSQL; check database availability")
        })?;
    let result = sqlx::query(&format!("CREATE DATABASE {}", quote_ident(name)))
        .execute(&pool)
        .await;
    let created = match result {
        Ok(_) => true,
        // 42P04 is duplicate_database: another process created the name while
        // this one was starting. It is not ours, so it must never be dropped.
        Err(error)
            if error
                .as_database_error()
                .and_then(|database| database.code())
                .as_deref()
                == Some("42P04") =>
        {
            false
        }
        Err(_) => return Err(anyhow::anyhow!("cannot create the destination database")),
    };
    pool.close().await;
    Ok(created)
}

/// Drop a destination database whose import failed. Shared with `restore`.
pub(crate) async fn destination_drop_database(url: &str, name: &str) -> Result<()> {
    drop_database(url, name).await
}

async fn drop_database(url: &str, name: &str) -> Result<()> {
    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection URL"))?
        .database("postgres")
        .disable_statement_logging();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await
        .map_err(|_| {
            anyhow::anyhow!("cannot connect to PostgreSQL to drop the destination database")
        })?;
    // Terminating other backends is best effort: the DROP is the step that
    // must be reported, because a failure leaves a half-restored database.
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(&pool)
    .await;
    let dropped = sqlx::query(&format!("DROP DATABASE IF EXISTS {}", quote_ident(name)))
        .execute(&pool)
        .await;
    pool.close().await;
    dropped.map_err(|error| anyhow::anyhow!("cannot drop the destination database: {error}"))?;
    Ok(())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Checksums and manifest verification
// ---------------------------------------------------------------------------

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn part_for(directory: &Path, name: &str) -> Result<Part> {
    let path = directory.join(name);
    let bytes = fs::metadata(&path)
        .with_context(|| format!("backup part {} is missing", path.display()))?
        .len();
    Ok(Part {
        name: name.to_owned(),
        sha256: sha256_file(&path)?,
        bytes,
    })
}

pub fn read_manifest(directory: &Path) -> Result<Manifest> {
    let path = directory.join("manifest.json");
    let text = fs::read_to_string(&path)
        .map_err(|_| ArchiveError::Corrupt("manifest.json is missing".into()))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|_| ArchiveError::Corrupt("manifest.json is not valid JSON".into()))?;
    if !(MIN_FORMAT_VERSION..=FORMAT_VERSION).contains(&manifest.format_version) {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported manifest format version {} (this binary reads {MIN_FORMAT_VERSION}..={FORMAT_VERSION})",
            manifest.format_version
        ))
        .into());
    }
    if manifest.format_version < 2 && !manifest.objects.is_empty() {
        return Err(ArchiveError::Corrupt("a pre-2 manifest cannot carry objects".into()).into());
    }
    Ok(manifest)
}

/// Verifies every part against its manifest entry. A single flipped byte in
/// any part is caught here, after decryption and before anything is published.
pub fn verify_parts(directory: &Path, manifest: &Manifest) -> Result<()> {
    for part in &manifest.parts {
        let path = directory.join(&part.name);
        let metadata = fs::metadata(&path).map_err(|_| {
            ArchiveError::Corrupt(format!("manifest part {} is missing", part.name))
        })?;
        if metadata.len() != part.bytes {
            return Err(ArchiveError::Corrupt(format!(
                "manifest part {} is {} bytes, expected {}",
                part.name,
                metadata.len(),
                part.bytes
            ))
            .into());
        }
        let actual = sha256_file(&path)?;
        if actual != part.sha256 {
            return Err(ArchiveError::Corrupt(format!(
                "checksum mismatch for manifest part {} (sha256 {actual})",
                part.name
            ))
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

pub struct BackupReport {
    pub fence: crate::maintenance::Fence,
    pub snapshot: String,
    pub output: PathBuf,
    pub bytes: u64,
    pub repositories: usize,
    pub postgres_lsn: String,
    pub key_id: String,
}

pub async fn run_backup(
    config: &Config,
    config_source: Option<&Path>,
    passphrase: &[u8],
    output: &Path,
) -> Result<BackupReport> {
    config.validate()?;
    if passphrase.is_empty() {
        return Err(ArchiveError::MissingKey.into());
    }

    let pool = crate::connect(config).await?;

    // A backup publishes a database dump. Encrypt any plaintext audit IP key
    // first, and refuse when the configured installation key cannot unwrap the
    // existing set: a dump taken outside the envelope is exactly what U28
    // forbids, and an archive whose secrets the operator cannot read is worse
    // than no archive.
    let installation_keys = crate::secrets::InstallationKeySet::from_config(config)?;
    let startup = crate::secrets::startup(
        &pool,
        installation_keys.as_ref(),
        crate::secrets::BACKFILL_BATCH,
    )
    .await
    .context("cannot prove every stored secret is encrypted before the backup")?;
    // A backup manifest records the current key id. An archive taken while a
    // rotation is unfinished also needs the retired key to restore, which the
    // operator is not told. Refuse and let rotation finish; storing every key
    // id would still need both key files at restore, so failing closed is the
    // simpler contract.
    if !startup.retired_key_ids.is_empty() {
        pool.close().await;
        bail!(
            "installation key rotation is incomplete: data keys remain under {}; run \
             `githearth-server secrets rotate` and retry the backup so the archive can be \
             restored with the current key alone",
            startup.retired_key_ids.join(", ")
        );
    }

    // How the storage part is captured is decided once, before anything is
    // written: `auto` probes the storage filesystem for copy-on-write cloning,
    // and a failed probe (or an explicit `fence`) keeps M4.1's maintenance
    // window. The probe happens before the gate so a filesystem that cannot
    // clone never enters the online path.
    let online = match config.backup_snapshot_mode.as_str() {
        "fence" => false,
        _ => copy_on_write_supported(&config.storage_root)?,
    };
    sweep_stale_snapshots(&config.storage_root);

    // Everything that can fail without the gate is done before the gate
    // starts, so a failure here cannot leave the server fenced.
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = Staging::new(parent, ".githearth-backup")?;

    let (fence, mut gate) = if online {
        crate::maintenance::begin_online(config).await?
    } else {
        (crate::maintenance::prove(config).await?, None)
    };
    println!("fence: {} ({})", fence.kind(), fence.detail());
    if online {
        println!("snapshot: copy-on-write (probed on the storage filesystem)");
    } else {
        println!("snapshot: maintenance fence (copy-on-write probe failed or disabled)");
    }

    // Drop the future before touching the gate again: it borrows `gate`
    // mutably, and an operator interrupt must release the gate, not leave the
    // server fenced until someone runs `maintenance stop`.
    let result = {
        let build = build_backup(
            config,
            config_source,
            passphrase,
            output,
            &staging.path,
            &pool,
            installation_keys.as_ref(),
            fence,
            online,
            &mut gate,
        );
        tokio::pin!(build);
        tokio::select! {
            result = &mut build => Some(result),
            _ = interrupted() => None,
        }
    };
    let result = match result {
        Some(result) => result,
        None => {
            if let Some(gate) = gate.take() {
                let _ = gate.release().await;
            }
            pool.close().await;
            bail!(
                "backup was interrupted; the online snapshot gate was released and no archive was \
                 published"
            );
        }
    };
    // If the build failed before releasing the gate, release it here so the
    // server never stays fenced because a backup failed.
    if let Some(gate) = gate.take()
        && let Err(error) = gate.release().await
    {
        pool.close().await;
        return Err(error.context("the online snapshot gate could not be released"));
    }
    pool.close().await;
    result
}

/// Resolves when the operator interrupts the backup, so an interrupted run
/// releases the online snapshot gate instead of leaving the server refusing
/// writes until someone runs `maintenance stop`.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[allow(clippy::too_many_arguments)]
async fn build_backup(
    config: &Config,
    config_source: Option<&Path>,
    passphrase: &[u8],
    output: &Path,
    staging: &Path,
    pool: &PgPool,
    installation_keys: Option<&crate::secrets::InstallationKeySet>,
    fence: crate::maintenance::Fence,
    online: bool,
    gate: &mut Option<crate::maintenance::OnlineGate>,
) -> Result<BackupReport> {
    config.check_private_paths_outside_storage()?;

    // The metadata position and the storage snapshot are taken together: a
    // repeatable-read transaction exports a snapshot for `pg_dump`, records
    // the WAL position, and reads every repository's journal position. The
    // transaction stays open while the dump runs, so the dump is exactly that
    // position even though writers resume before it starts.
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *conn)
        .await
        .context("cannot start the metadata snapshot transaction")?;
    let (database, version, lsn, exported): (String, String, String, String) = sqlx::query_as(
        "SELECT current_database(), current_setting('server_version'), \
                pg_current_wal_lsn()::text, pg_export_snapshot()",
    )
    .fetch_one(&mut *conn)
    .await
    .context("cannot record the PostgreSQL position and export its snapshot")?;
    let postgres = PostgresPosition {
        database,
        server_version: version,
        dump_lsn: lsn,
        exported_snapshot: Some(exported.clone()),
    };

    let before_snapshot = repository_positions(config, &mut conn).await?;
    // The ref digest is read from the live root now, while the gate (or the
    // fence) still holds writers, so it is the expected ref set of everything
    // that follows: the clone, the archive and the restore. It covers a ref
    // that has no journal record, which the journal replay cannot see.
    let mut recorded = before_snapshot.clone();
    record_ref_digests(&config.storage_root, &mut recorded)?;
    let objects_before = object_positions(&config.storage_root)?;

    // Take the storage snapshot. The online path clones the included subtrees
    // with copy-on-write while the brief gate holds writers at the recorded
    // position; the fence path tars the live root later, where no writer can
    // acknowledge a commit.
    let snapshot_tree = if online {
        // A test holds here to attempt a write while the short gate is held,
        // after the recorded position and before the snapshot.
        crate::testseam::hold("BACKUP_ONLINE_GATE").await;
        Some(SnapshotTree::create(&config.storage_root)?)
    } else {
        None
    };
    let snapshot_root = snapshot_tree
        .as_ref()
        .map(|tree| tree.path.clone())
        .unwrap_or_else(|| config.storage_root.clone());
    if online {
        copy_on_write_snapshot(&config.storage_root, &snapshot_root)?;
        // A test holds here to attempt a write after the clone and before the
        // ref check; the gate is still held, so it must be refused.
        crate::testseam::hold("BACKUP_AFTER_SNAPSHOT").await;
    }

    let after_snapshot = repository_positions(config, &mut conn).await?;
    let objects_after = object_positions(&snapshot_root)?;
    if before_snapshot != after_snapshot {
        let changed = before_snapshot
            .iter()
            .zip(&after_snapshot)
            .find(|(before, after)| before != after)
            .map(|(before, _after)| format!("repository {}", before.id))
            .unwrap_or_else(|| "the repository set".to_owned());
        bail!(
            "repository state changed while the storage snapshot was taken ({changed}); \
             refusing to publish an inconsistent backup"
        );
    }
    if objects_before != objects_after {
        bail!(
            "object store inventory changed while the storage snapshot was taken; refusing to \
             publish an inconsistent backup"
        );
    }

    // The snapshot must carry every repository's client-visible refs at the
    // recorded journal position, not just a set of files that happen to exist.
    verify_snapshot_refs(&snapshot_root, &recorded)?;
    let refs_verified = recorded
        .iter()
        .all(|position| position.refs_digest.is_some());
    let repositories = recorded;
    let objects = objects_after;

    // Release the brief gate now. The dump and everything after it run with
    // writers active; they are after the recorded position by construction.
    if let Some(gate) = gate.take() {
        gate.release()
            .await
            .context("the online snapshot gate could not be released after the snapshot")?;
    }

    // A test holds here to make a write land after the gate release and before
    // the dump: the dump is pinned to the exported snapshot, so the write is
    // absent from the archive even though it was acknowledged.
    crate::testseam::hold("BACKUP_BEFORE_DUMP").await;

    // Writers are active again. A mixed-version writer (an older binary that
    // ignores the gate) could have added a plaintext audit key since the check
    // at the start of the command; the dump must never carry one. Re-running
    // the full start-up check also re-proves that every wrapper still opens.
    crate::secrets::startup(pool, installation_keys, crate::secrets::BACKFILL_BATCH)
        .await
        .context(
            "cannot prove every stored secret is encrypted immediately before the dump; refusing \
             to publish a dump that may carry a plaintext secret",
        )?;

    dump_database(
        &config.database_url,
        &staging.join("database.dump"),
        Some(&exported),
    )?;
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
    drop(conn);

    // A test holds here to lift the fence between the dump and the snapshot.
    crate::testseam::hold("BACKUP_AFTER_DUMP").await;

    if online {
        create_tar(&staging.join("storage.tar"), &snapshot_root, &[])?;
    } else {
        // The fence path keeps the operator's maintenance window (or the
        // stopped server) for the whole backup. Re-prove immediately before
        // the storage tar: if maintenance was lifted during the dump, refuse
        // instead of tarring a tree a new writer could be moving.
        let _ = crate::maintenance::prove(config).await?;
        create_tar(
            &staging.join("storage.tar"),
            &config.storage_root,
            &[
                "./run",
                "./tmp",
                "./hooks",
                "./objects/tmp",
                "./objects/locks",
                // Derived or ephemeral: rebuilt after a restore, and an upload
                // is not part of a published object set.
                "./objects/usage",
                "./objects/uploads",
                "./.githearth-backup-snapshot-*",
            ],
        )?;
    }
    drop(snapshot_tree);

    // The effective configuration is the one this backup was taken under.
    let config_text = toml::to_string(config)?;
    fs::write(staging.join("config.toml"), config_text)?;
    fs::set_permissions(
        staging.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )?;

    let host_key_part = if config.ssh_enabled {
        let path = &config.ssh_host_key_path;
        if path.as_os_str().is_empty() {
            bail!(
                "ssh_enabled is true but ssh_host_key_path is empty; refusing an incomplete backup"
            );
        }
        let bytes = fs::read(path).with_context(|| {
            format!(
                "cannot read the SSH host key {}; refusing an incomplete backup",
                path.display()
            )
        })?;
        fs::write(staging.join("ssh_host_key"), bytes)?;
        fs::set_permissions(
            staging.join("ssh_host_key"),
            fs::Permissions::from_mode(0o600),
        )?;
        Some(part_for(staging, "ssh_host_key")?)
    } else {
        None
    };

    let database_part = part_for(staging, "database.dump")?;
    let storage_part = part_for(staging, "storage.tar")?;
    let config_part = ConfigPart {
        source: config_source
            .map(|path| path.display().to_string())
            .filter(|source| !source.is_empty()),
        sha256: sha256_file(&staging.join("config.toml"))?,
        bytes: fs::metadata(staging.join("config.toml"))?.len(),
        note: "effective configuration, including database_url; the archive is encrypted \
               because this holds credentials"
            .into(),
    };

    let mut parts = vec![database_part, storage_part];
    parts.push(part_for(staging, "config.toml")?);
    if let Some(part) = &host_key_part {
        parts.push(part.clone());
    }

    let snapshot = Some(SnapshotRecord {
        mode: if online {
            "copy-on-write".into()
        } else {
            "maintenance-fence".into()
        },
        mechanism: if online {
            copy_on_write_mechanism().into()
        } else {
            "whole-root-tar".into()
        },
        refs_verified,
    });

    let material = key_material(passphrase)?;
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        created_unix_seconds: now_unix_seconds(),
        fence: FenceRecord {
            kind: fence.kind().into(),
            detail: fence.detail().into(),
        },
        snapshot,
        postgres,
        repositories,
        objects,
        config: config_part,
        ssh_host_key: host_key_part,
        installation: installation_keys.map(|keys| keys.record()),
        parts,
        encryption: EncryptionRecord {
            key_id: material.key_id.clone(),
            algorithm: CIPHER.into(),
            kdf: KDF.into(),
            wrap: WRAP.into(),
            kdf_memory_kib: KDF_MEMORY_KIB,
            kdf_iterations: KDF_ITERATIONS,
            salt: BASE64.encode(material.salt),
            nonce: BASE64.encode(material.nonce),
        },
        excluded: vec![
            "run/ (sockets, control token, maintenance state)".into(),
            "tmp/ (scratch directories)".into(),
            "hooks/ (regenerated by serve)".into(),
        ],
        notes: vec![
            "The manifest covers database.dump, storage.tar, config.toml and ssh_host_key by \
             SHA-256. It does not cover itself (the outer AES-256-GCM tag authenticates it) and \
             it deliberately does not contain the encryption key."
                .into(),
            "The PostgreSQL position is the exported repeatable-read snapshot that pg_dump used; \
             the dump is exactly that position. Database-to-storage consistency is additionally \
             proved by the per-repository journal positions, and every repository's \
             client-visible refs at those positions were verified inside the storage snapshot \
             against a SHA-256 digest of the live ref set recorded before the snapshot and against \
             the journal replay."
                .into(),
            "The manifest records the installation key id, algorithm and wrap method only. The \
             installation key itself is never written to the database or the archive; without it \
             the stored secret columns stay unreadable by design."
                .into(),
        ],
    };
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    // Keep the payload outside the staging directory so the payload tar does
    // not contain itself.
    let payload = staging.with_extension("payload.tar");
    let archive = staging.with_extension("archive.ghbackup");
    create_tar(&payload, staging, &[])?;
    encrypt_with(&payload, &archive, &material)?;
    let _ = fs::remove_file(&payload);

    // "A backup is consistent or it does not happen" is enforced here, where
    // the backup happens: read the written archive back, verify every part,
    // extract the storage snapshot and run the same check `restore` runs.
    // Only then is the operator's output path published.
    if let Err(error) = verify_written_archive(&archive, passphrase, staging) {
        let _ = fs::remove_file(&archive);
        return Err(error);
    }
    if let Err(error) = fs::rename(&archive, output) {
        let _ = fs::remove_file(&archive);
        return Err(anyhow::anyhow!(
            "cannot publish the verified backup at {}: {error}",
            output.display()
        ));
    }

    let bytes = fs::metadata(output)?.len();
    let snapshot = manifest
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.mode.clone())
        .unwrap_or_else(|| "legacy-whole-root-tar".into());
    Ok(BackupReport {
        fence,
        snapshot,
        output: output.to_path_buf(),
        bytes,
        repositories: manifest.repositories.len(),
        postgres_lsn: manifest.postgres.dump_lsn,
        key_id: manifest.encryption.key_id,
    })
}

/// Re-reads the archive this command just wrote and refuses to publish it if
/// it is not decryptable, checksums do not match, or the storage snapshot does
/// not pass `Storage::check`.
fn verify_written_archive(archive: &Path, passphrase: &[u8], staging: &Path) -> Result<()> {
    let verify = staging.join("verify");
    fs::create_dir_all(&verify)?;
    let payload = verify.join("payload.tar");
    let header = decrypt_file(archive, &payload, passphrase)
        .context("the archive this backup wrote cannot be decrypted")?;
    let root = verify.join("root");
    extract_tar(&payload, &root)?;
    let manifest = read_manifest(&root)?;
    if manifest.encryption.key_id != header.key_id {
        return Err(ArchiveError::Corrupt(
            "manifest key id does not match the archive header".into(),
        )
        .into());
    }
    verify_parts(&root, &manifest)?;
    let storage_dir = verify.join("storage");
    extract_tar(&root.join("storage.tar"), &storage_dir)?;
    let storage = Storage::open_existing(&storage_dir)?;
    verify_objects(&storage_dir, &manifest.objects)?;
    verify_snapshot_refs(&storage_dir, &manifest.repositories)?;
    let (_, problems) = storage.check()?;
    if let Some(first) = problems.first() {
        bail!(
            "the archive's storage snapshot fails verification: {} problem(s); first: {}: {}",
            problems.len(),
            first.subject,
            first.message
        );
    }
    Ok(())
}

async fn repository_positions(
    config: &Config,
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<RepositoryPosition>> {
    let rows: Vec<(Uuid, String, String, i64, i64)> = sqlx::query_as(
        "SELECT r.id, r.status, r.object_format, r.journal_applied, \
                COALESCE((SELECT max(p.journal_sequence) FROM repository_pushes p \
                          WHERE p.repository_id = r.id), 0) \
         FROM repositories r ORDER BY r.id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let storage = Storage::open_existing(&config.storage_root)?;
    let scanned = storage.scan()?;
    let known: std::collections::HashSet<Uuid> = rows.iter().map(|row| row.0).collect();
    if let Some(extra) = scanned.repositories.iter().find(|id| !known.contains(id)) {
        bail!(
            "repository {extra} exists on disk but has no metadata row; refusing to back up an \
             inconsistent installation"
        );
    }
    let mut positions = Vec::with_capacity(rows.len());
    for (id, status, object_format, journal_applied, max_push) in rows {
        let path = storage.repository_path(id);
        let journal_position = if path.is_dir() {
            journal::verify(&path).map_err(|error| {
                anyhow::anyhow!("repository {id} has an unreadable journal: {error}")
            })?
        } else if status == "active" {
            bail!(
                "repository {id} is active but has no storage directory; refusing to back up an \
                 inconsistent installation"
            );
        } else {
            0
        };
        if max_push as u64 > journal_position {
            bail!(
                "repository {id} has metadata at journal sequence {max_push}, past the storage \
                 journal position {journal_position}; refusing to back up an inconsistent \
                 installation"
            );
        }
        positions.push(RepositoryPosition {
            id,
            status,
            object_format,
            journal_position,
            journal_applied: journal_applied.max(0) as u64,
            refs_digest: None,
        });
    }
    Ok(positions)
}

/// Every object the installation stores, or an empty list when the object
/// store has never been opened (a fresh install before M4.2's first write).
fn object_positions(storage_root: &Path) -> Result<Vec<ObjectRecord>> {
    let root = storage_root.join("objects");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let store = FsObjectStore::open_existing(&root)?;
    Ok(store.inventory()?)
}

/// Verifies a storage snapshot's object store against the manifest: every
/// object's recorded checksum and chunk table, then exact inventory equality.
fn verify_objects(storage_root: &Path, expected: &[ObjectRecord]) -> Result<()> {
    let root = storage_root.join("objects");
    if !root.is_dir() {
        if expected.is_empty() {
            return Ok(());
        }
        return Err(ArchiveError::Corrupt(format!(
            "the archive's manifest lists {} object(s) but the storage snapshot has no object store",
            expected.len()
        ))
        .into());
    }
    let store = FsObjectStore::open_existing(&root)?;
    let problems = store.verify_all()?;
    // An orphan is a payload without a sidecar: the unacknowledged residue of
    // a crash between the payload write and the sidecar rename. It is carried
    // inside the archive's tar so an operator can repair it, but it is not part
    // of the manifest or of the exact inventory comparison (ADR 0014, "Crash
    // consistency"). Every other problem, including a committed object whose
    // payload is missing or whose sidecar is unreadable, is `Corrupt` and
    // refuses the backup: publishing a short object set would lose data
    // silently.
    if let Some(first) = problems
        .iter()
        .find(|problem| problem.kind == ObjectProblemKind::Corrupt)
    {
        let corrupt = problems
            .iter()
            .filter(|problem| problem.kind == ObjectProblemKind::Corrupt)
            .count();
        bail!(
            "the archive's object store fails verification: {corrupt} problem(s); first: {}: {}",
            first.key,
            first.message
        );
    }
    let actual = store.inventory()?;
    if actual != expected {
        let message = if actual.len() != expected.len() {
            format!(
                "the archive's manifest lists {} object(s) but the snapshot holds {}",
                expected.len(),
                actual.len()
            )
        } else {
            let first = expected
                .iter()
                .zip(&actual)
                .find(|(expected, actual)| expected != actual)
                .map(|(expected, actual)| {
                    format!(
                        "object {} ({} vs {})",
                        expected.key, expected.sha256, actual.sha256
                    )
                })
                .unwrap_or_else(|| "the object inventory".to_owned());
            format!("the archive's object inventory disagrees with the manifest at {first}")
        };
        return Err(ArchiveError::Corrupt(message).into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

pub struct RestoreReport {
    pub database: String,
    pub storage_root: PathBuf,
    pub config_path: PathBuf,
    pub repositories: usize,
    pub key_id: String,
}

pub struct RestoreOptions {
    pub archive: PathBuf,
    pub passphrase: Vec<u8>,
    pub database_url: String,
    pub config_path: PathBuf,
    pub storage_root: Option<PathBuf>,
    pub ssh_host_key_path: Option<PathBuf>,
}

/// The key set a restore should use: an environment override wins (the operator
/// restoring to a fresh host has not edited the archived configuration yet),
/// otherwise the paths recorded in the archived configuration. An empty path
/// means no key, which is only valid for an archive that predates the envelope.
fn restored_installation_keys(
    archived: &Config,
) -> Result<Option<crate::secrets::InstallationKeySet>> {
    let env_path = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let current = env_path("GITHEARTH_INSTALLATION_KEY_FILE")
        .unwrap_or_else(|| archived.installation_key_file.clone());
    if current.as_os_str().is_empty() {
        return Ok(None);
    }
    let previous = env_path("GITHEARTH_INSTALLATION_KEY_PREVIOUS_FILE")
        .unwrap_or_else(|| archived.installation_key_previous_file.clone());
    let passphrase = env_path("GITHEARTH_INSTALLATION_KEY_PASSPHRASE_FILE")
        .or_else(|| archived.installation_key_passphrase_file.clone());
    crate::secrets::InstallationKeySet::from_paths(
        &current,
        Some(previous.as_path()),
        passphrase.as_deref(),
    )
}

pub async fn run_restore(options: &RestoreOptions) -> Result<RestoreReport> {
    if options.passphrase.is_empty() {
        return Err(ArchiveError::MissingKey.into());
    }
    let database = parse_pg_url(&options.database_url)?.database;
    if database_exists(&options.database_url, &database).await? {
        bail!(
            "destination database `{database}` already exists; restore needs a fresh database on \
             a fresh host"
        );
    }
    if options.config_path.exists() {
        bail!(
            "destination configuration {} already exists; restore refuses to overwrite it",
            options.config_path.display()
        );
    }

    let staging = Staging::new(std::env::temp_dir().as_path(), ".githearth-restore")?;
    let payload = staging.path.join("payload.tar");
    let header = decrypt_file(&options.archive, &payload, &options.passphrase)?;
    extract_tar(&payload, &staging.path.join("payload"))?;
    let root = staging.path.join("payload");
    let manifest = read_manifest(&root)?;
    verify_parts(&root, &manifest)?;
    if manifest.encryption.key_id != header.key_id {
        return Err(ArchiveError::Corrupt(
            "manifest key id does not match the archive header".into(),
        )
        .into());
    }

    // The staged storage tree is verified before any destination is touched.
    extract_tar(&root.join("storage.tar"), &staging.path.join("storage"))?;
    let staged_storage = staging.path.join("storage");
    let storage = Storage::open_existing(&staged_storage)?;

    let archived: Config = toml::from_str(
        &fs::read_to_string(root.join("config.toml"))
            .context("the archive has no readable config.toml")?,
    )
    .map_err(|_| anyhow::anyhow!("the archived configuration is not valid TOML"))?;
    let installation_keys = restored_installation_keys(&archived)?;
    if let Some(record) = &manifest.installation {
        let keys = installation_keys.as_ref().context(format!(
            "the archive was written with installation key id {}; place that key where \
             installation_key_file points or set GITHEARTH_INSTALLATION_KEY_FILE before restoring",
            record.key_id
        ))?;
        if keys.current().id() != record.key_id {
            bail!(
                "wrong installation key: the archive records key id {}, but the supplied key is \
                 {}. The archive was not changed",
                record.key_id,
                keys.current().id()
            );
        }
    }
    let destination_storage = options
        .storage_root
        .clone()
        .unwrap_or_else(|| archived.storage_root.clone());
    let destination_host_key = options
        .ssh_host_key_path
        .clone()
        .unwrap_or_else(|| archived.ssh_host_key_path.clone());
    if archived.ssh_enabled && manifest.ssh_host_key.is_none() {
        bail!("the archived configuration enables SSH but the archive has no SSH host key");
    }
    if manifest.ssh_host_key.is_some() && destination_host_key.exists() {
        bail!(
            "destination SSH host key {} already exists; restore refuses to overwrite it",
            destination_host_key.display()
        );
    }
    if destination_storage.exists() {
        let empty = fs::read_dir(&destination_storage)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            bail!(
                "destination storage {} is not empty; restore needs a fresh host",
                destination_storage.display()
            );
        }
    }

    // The database is created and restored into a name that did not exist; any
    // failure below drops it again.
    let mut database_created = false;
    let mut storage_published = false;
    let mut config_written = false;
    let mut host_key_written = false;
    let result = async {
        let created = create_database(&options.database_url, &database).await?;
        if !created {
            bail!(
                "the destination database `{database}` appeared while restore was starting; \
                 this restore did not create it and will not restore into or drop it"
            );
        }
        database_created = true;
        restore_database(&options.database_url, &root.join("database.dump"))?;

        let pool = crate::connect(&Config {
            database_url: options.database_url.clone(),
            ..Config::default()
        })
        .await?;
        let verification = verify_restored(&pool, &storage, &manifest).await;
        let verification = match verification {
            Ok(()) => {
                // The restored database must be readable with the installation
                // key the destination holds. This runs before the storage tree
                // or configuration is published; a failure drops the database
                // below, so the restore never leaves unreadable secrets.
                // A pre-format-3 archive has no installation record and no
                // envelope, so it is restored as plaintext even when the
                // operator supplied a destination key.
                if manifest.installation.is_some() {
                    crate::secrets::startup(
                        &pool,
                        installation_keys.as_ref(),
                        crate::secrets::BACKFILL_BATCH,
                    )
                    .await
                    .context("the restored database's secrets do not match the installation key")
                    .map(|_| ())
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error),
        };
        pool.close().await;
        verification?;
        // Every object's recorded checksum, and the manifest's inventory,
        // before the storage tree is published to the destination.
        verify_objects(&staged_storage, &manifest.objects)?;
        verify_snapshot_refs(&staged_storage, &manifest.repositories)?;

        publish_storage(&staged_storage, &destination_storage)?;
        storage_published = true;

        let rewritten = rewrite_configuration(
            &root.join("config.toml"),
            &options.database_url,
            &destination_storage,
            &destination_host_key,
            std::env::var("GITHEARTH_INSTALLATION_KEY_FILE")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .as_deref(),
        )?;
        write_private(&options.config_path, rewritten.as_bytes())?;
        config_written = true;

        if manifest.ssh_host_key.is_some() {
            let bytes = fs::read(root.join("ssh_host_key"))?;
            write_private(&destination_host_key, &bytes)?;
            host_key_written = true;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(error) = result {
        if storage_published {
            let _ = fs::remove_dir_all(&destination_storage);
        }
        if config_written {
            let _ = fs::remove_file(&options.config_path);
        }
        if host_key_written {
            let _ = fs::remove_file(&destination_host_key);
        }
        if database_created
            && let Err(cleanup) = drop_database(&options.database_url, &database).await
        {
            return Err(anyhow::anyhow!(
                "{error}\n\nthe destination database `{database}` could not be dropped \
                 ({cleanup}); it may be partially restored. Inspect it and drop it manually \
                 before retrying the restore"
            ));
        }
        return Err(error);
    }

    Ok(RestoreReport {
        database,
        storage_root: destination_storage,
        config_path: options.config_path.clone(),
        repositories: manifest.repositories.len(),
        key_id: manifest.encryption.key_id,
    })
}

async fn verify_restored(pool: &PgPool, storage: &Storage, manifest: &Manifest) -> Result<()> {
    let rows: Vec<(Uuid, String, i64)> =
        sqlx::query_as("SELECT id, status, journal_applied FROM repositories ORDER BY id")
            .fetch_all(pool)
            .await?;
    let positions: std::collections::HashMap<Uuid, &RepositoryPosition> = manifest
        .repositories
        .iter()
        .map(|position| (position.id, position))
        .collect();
    for (id, status, journal_applied) in &rows {
        let Some(position) = positions.get(id) else {
            if *status == "active" {
                bail!(
                    "restored database has active repository {id}, but the manifest has no journal \
                     position for it"
                );
            }
            continue;
        };
        let path = storage.repository_path(*id);
        if !path.is_dir() {
            if *status == "active" {
                bail!("restored database has active repository {id}, but its storage is missing");
            }
            continue;
        }
        let actual = journal::verify(&path)
            .map_err(|error| anyhow::anyhow!("repository {id} journal does not verify: {error}"))?;
        if actual != position.journal_position {
            bail!(
                "repository {id} journal is at {actual}, but the manifest recorded {}",
                position.journal_position
            );
        }
        if *journal_applied < 0 || *journal_applied as u64 > position.journal_position {
            bail!(
                "repository {id} metadata is applied through {journal_applied}, past the manifest \
                 position {}",
                position.journal_position
            );
        }
        let max_push: Option<i64> = sqlx::query_scalar(
            "SELECT max(journal_sequence) FROM repository_pushes WHERE repository_id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        if let Some(sequence) = max_push
            && sequence as u64 > position.journal_position
        {
            bail!(
                "repository {id} has a metadata push at sequence {sequence}, past the manifest \
                 position {}",
                position.journal_position
            );
        }
    }
    for position in &manifest.repositories {
        if !rows.iter().any(|row| row.0 == position.id)
            && storage.repository_path(position.id).is_dir()
        {
            bail!(
                "manifest repository {} has storage but no metadata row in the restored database",
                position.id
            );
        }
    }
    let (_, problems) = storage.check()?;
    if !problems.is_empty() {
        let first = &problems[0];
        bail!(
            "restored storage failed verification: {} problem(s); first: {}: {}",
            problems.len(),
            first.subject,
            first.message
        );
    }
    Ok(())
}

fn rewrite_configuration(
    source: &Path,
    database_url: &str,
    storage_root: &Path,
    ssh_host_key_path: &Path,
    installation_key_file: Option<&Path>,
) -> Result<String> {
    let text = fs::read_to_string(source)?;
    let mut value: toml::Value = toml::from_str(&text)
        .map_err(|_| anyhow::anyhow!("the archived configuration is not valid TOML"))?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("the archived configuration is not a TOML table"))?;
    table.insert(
        "database_url".into(),
        toml::Value::String(database_url.to_owned()),
    );
    table.insert(
        "storage_root".into(),
        toml::Value::String(storage_root.display().to_string()),
    );
    table.insert(
        "ssh_host_key_path".into(),
        toml::Value::String(ssh_host_key_path.display().to_string()),
    );
    if let Some(path) = installation_key_file {
        table.insert(
            "installation_key_file".into(),
            toml::Value::String(path.display().to_string()),
        );
    }
    let rendered = toml::to_string(&value)?;
    let config: Config = toml::from_str(&rendered)
        .map_err(|_| anyhow::anyhow!("the restored configuration does not parse"))?;
    config
        .validate()
        .map_err(|error| anyhow::anyhow!("the restored configuration is not usable: {error}"))?;
    Ok(rendered)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

fn publish_storage(staged: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if destination.exists() {
        if fs::read_dir(destination)?.next().is_some() {
            bail!(
                "destination storage {} became non-empty during restore",
                destination.display()
            );
        }
        fs::remove_dir(destination)?;
    }
    match fs::rename(staged, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Different filesystem: copy the verified tree, then remove it.
            copy_tree(staged, destination)?;
            fs::remove_dir_all(staged)?;
            Ok(())
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let target = destination.join(entry.file_name());
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing to publish a symlink from restored storage: {}",
                entry.path().display()
            );
        }
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(metadata.permissions().mode()),
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Normalises an address for a status line, never for a connection.
pub fn shown(address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        if address.is_ipv4() {
            SocketAddr::from(([127, 0, 0, 1], address.port()))
        } else {
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], address.port()))
        }
    } else {
        address
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrites only the plaintext header of an archive, keeping its body.
    fn rewrite_header(archive: &Path, target: &Path, edit: impl FnOnce(&mut ArchiveHeader)) {
        let bytes = fs::read(archive).unwrap();
        let first = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        let second = first
            + 1
            + bytes[first + 1..]
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap();
        let mut header: ArchiveHeader = serde_json::from_slice(&bytes[first + 1..second]).unwrap();
        edit(&mut header);
        let mut rewritten = bytes[..=first].to_vec();
        rewritten.extend(serde_json::to_vec(&header).unwrap());
        rewritten.extend_from_slice(&bytes[second..]);
        fs::write(target, rewritten).unwrap();
    }

    /// Stages an archive whose `storage.tar` contains a malformed repository
    /// directory, so `Storage::check` has something to report.
    fn archive_with_unhealthy_storage(directory: &Path) -> PathBuf {
        let staging = directory.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let source = directory.join("storage");
        fs::create_dir_all(source.join("repositories/malformed")).unwrap();
        create_tar(&staging.join("storage.tar"), &source, &[]).unwrap();
        fs::write(staging.join("database.dump"), b"not a real dump").unwrap();
        fs::write(staging.join("config.toml"), b"storage_root = \"/tmp\"\n").unwrap();
        let parts = ["database.dump", "storage.tar", "config.toml"]
            .into_iter()
            .map(|name| part_for(&staging, name).unwrap())
            .collect();
        let material = key_material(b"storage-check").unwrap();
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            created_unix_seconds: 0,
            fence: FenceRecord {
                kind: "not-running".into(),
                detail: "test".into(),
            },
            snapshot: None,
            postgres: PostgresPosition {
                database: "d".into(),
                server_version: "16".into(),
                dump_lsn: "0/0".into(),
                exported_snapshot: None,
            },
            repositories: Vec::new(),
            objects: Vec::new(),
            config: ConfigPart {
                source: None,
                sha256: sha256_file(&staging.join("config.toml")).unwrap(),
                bytes: fs::metadata(staging.join("config.toml")).unwrap().len(),
                note: String::new(),
            },
            ssh_host_key: None,
            installation: None,
            parts,
            encryption: EncryptionRecord {
                key_id: material.key_id.clone(),
                algorithm: CIPHER.into(),
                kdf: KDF.into(),
                wrap: WRAP.into(),
                kdf_memory_kib: KDF_MEMORY_KIB,
                kdf_iterations: KDF_ITERATIONS,
                salt: BASE64.encode(material.salt),
                nonce: BASE64.encode(material.nonce),
            },
            excluded: Vec::new(),
            notes: Vec::new(),
        };
        fs::write(
            staging.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let payload = directory.join("payload.tar");
        create_tar(&payload, &staging, &[]).unwrap();
        let archive = directory.join("archive.ghbackup");
        encrypt_with(&payload, &archive, &material).unwrap();
        archive
    }

    #[test]
    fn a_probe_that_cannot_run_falls_back_to_the_fence() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("no-such-storage-root");
        // The probe directory cannot be created, so `auto` must choose the
        // fence path rather than abort the backup.
        assert!(!copy_on_write_supported(&missing).unwrap());
    }

    #[test]
    fn a_written_archive_whose_storage_snapshot_is_unhealthy_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let archive = archive_with_unhealthy_storage(directory.path());
        let error = verify_written_archive(
            &archive,
            b"storage-check",
            &directory.path().join("staging"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("storage snapshot fails verification"),
            "{error}"
        );
    }

    #[test]
    fn an_edited_header_breaks_chunk_zero_and_a_future_format_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let payload = directory.path().join("payload");
        fs::write(&payload, vec![7u8; 4096]).unwrap();
        let material = key_material(b"header-test").unwrap();
        let archive = directory.path().join("archive.ghbackup");
        encrypt_with(&payload, &archive, &material).unwrap();
        let out = directory.path().join("out");

        // `plaintext_bytes` is part of chunk 0's AAD: rewriting it must be a
        // corruption, not a different length and not a successful decrypt.
        let tampered = directory.path().join("tampered.ghbackup");
        rewrite_header(&archive, &tampered, |header| {
            header.plaintext_bytes += 1;
        });
        let error = decrypt_file(&tampered, &out, b"header-test")
            .unwrap_err()
            .to_string();
        assert!(error.contains("corrupt"), "{error}");
        assert!(
            error.contains("authentication failed at chunk 0"),
            "{error}"
        );

        // A future format is refused rather than parsed as version 1.
        let future = directory.path().join("future.ghbackup");
        rewrite_header(&archive, &future, |header| {
            header.format_version = FORMAT_VERSION + 1;
        });
        let error = decrypt_file(&future, &out, b"header-test")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unsupported archive format version"),
            "{error}"
        );

        // The manifest is checked the same way.
        let root = directory.path().join("manifest");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("manifest.json"),
            serde_json::json!({
                "format_version": FORMAT_VERSION + 1,
                "created_unix_seconds": 0,
                "fence": {"kind": "not-running", "detail": "test"},
                "postgres": {"database": "d", "server_version": "16", "dump_lsn": "0/0"},
                "repositories": [],
                "config": {"source": null, "sha256": "0", "bytes": 0, "note": ""},
                "ssh_host_key": null,
                "parts": [],
                "encryption": {
                    "key_id": "sha256:0",
                    "algorithm": CIPHER,
                    "kdf": KDF,
                    "wrap": WRAP,
                    "kdf_memory_kib": KDF_MEMORY_KIB,
                    "kdf_iterations": KDF_ITERATIONS,
                    "salt": "",
                    "nonce": ""
                },
                "excluded": [],
                "notes": []
            })
            .to_string(),
        )
        .unwrap();
        let error = read_manifest(&root).unwrap_err().to_string();
        assert!(
            error.contains("unsupported manifest format version"),
            "{error}"
        );

        // The untouched archive still decrypts.
        decrypt_file(&archive, &out, b"header-test").unwrap();
    }
}
