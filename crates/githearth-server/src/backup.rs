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
use githearth_git::{journal, layout::Storage};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, PgPool, postgres::PgPoolOptions};
use std::{
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
const FORMAT_VERSION: u32 = 1;
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
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PostgresPosition {
    pub database: String,
    pub server_version: String,
    /// Advisory only: `pg_current_wal_lsn()` read beside the dump. It is not
    /// the dump transaction's snapshot position and `restore` never compares
    /// it; database-to-storage consistency is proved by the per-repository
    /// journal positions in `repositories`.
    pub dump_lsn: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FenceRecord {
    pub kind: String,
    pub detail: String,
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
    pub postgres: PostgresPosition,
    pub repositories: Vec<RepositoryPosition>,
    pub config: ConfigPart,
    pub ssh_host_key: Option<Part>,
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
    if header.format_version != FORMAT_VERSION {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported archive format version {} (this binary reads {FORMAT_VERSION})",
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

fn dump_database(url: &str, path: &Path) -> Result<()> {
    let url = parse_pg_url(url)?;
    let mut command = pg_command(&url, "pg_dump");
    command
        .args(["--format=custom", "--no-owner", "--no-acl", "--file"])
        .arg(path);
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

/// The advisory PostgreSQL position. It is read beside the dump, not inside
/// the dump's transaction, so it is recorded for an operator and never read by
/// `restore`. See [`PostgresPosition::dump_lsn`].
async fn position(pool: &PgPool) -> Result<PostgresPosition> {
    let (database, version, lsn): (String, String, String) = sqlx::query_as(
        "SELECT current_database(), current_setting('server_version'), \
         pg_current_wal_lsn()::text",
    )
    .fetch_one(pool)
    .await?;
    Ok(PostgresPosition {
        database,
        server_version: version,
        dump_lsn: lsn,
    })
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

async fn create_database(url: &str, name: &str) -> Result<()> {
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
    sqlx::query(&format!("CREATE DATABASE {}", quote_ident(name)))
        .execute(&pool)
        .await
        .map_err(|_| anyhow::anyhow!("cannot create the destination database"))?;
    pool.close().await;
    Ok(())
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
    if manifest.format_version != FORMAT_VERSION {
        return Err(ArchiveError::Corrupt(format!(
            "unsupported manifest format version {} (this binary reads {FORMAT_VERSION})",
            manifest.format_version
        ))
        .into());
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
    if passphrase.is_empty() {
        return Err(ArchiveError::MissingKey.into());
    }
    let fence = crate::maintenance::prove(config).await?;
    println!("fence: {} ({})", fence.kind(), fence.detail());

    let pool = crate::connect(config).await?;
    // Advisory only (see `PostgresPosition::dump_lsn`): read beside the dump,
    // never compared by `restore`.
    let postgres = position(&pool).await?;

    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = Staging::new(parent, ".githearth-backup")?;
    let result = build_backup(
        config,
        config_source,
        passphrase,
        output,
        &staging.path,
        &pool,
        postgres,
        fence,
    )
    .await;
    pool.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn build_backup(
    config: &Config,
    config_source: Option<&Path>,
    passphrase: &[u8],
    output: &Path,
    staging: &Path,
    pool: &PgPool,
    postgres: PostgresPosition,
    fence: crate::maintenance::Fence,
) -> Result<BackupReport> {
    // Dump the database first, so the storage snapshot that follows includes
    // every journal record that references a row in the dump.
    dump_database(&config.database_url, &staging.join("database.dump"))?;
    // A test holds here to lift the fence between the dump and the snapshot.
    crate::testseam::hold("BACKUP_AFTER_DUMP").await;

    // Re-prove immediately before the storage snapshot. If the operator
    // stopped the server's maintenance mode while the dump was running, this
    // refuses instead of taking a snapshot that could race a new write.
    let _ = crate::maintenance::prove(config).await?;

    // Sample the repository state *before* the snapshot. A journal-moving
    // writer during the tar would otherwise produce an archive whose
    // `storage.tar` and manifest disagree, discovered only at restore.
    let before_snapshot = repository_positions(config, pool).await?;
    create_tar(
        &staging.join("storage.tar"),
        &config.storage_root,
        &["./run", "./tmp", "./hooks"],
    )?;
    let after_snapshot = repository_positions(config, pool).await?;
    if before_snapshot != after_snapshot {
        let changed = before_snapshot
            .iter()
            .zip(&after_snapshot)
            .find(|(before, after)| before != after)
            .map(|(before, _after)| format!("repository {}", before.id))
            .unwrap_or_else(|| "the repository set".to_owned());
        bail!(
            "repository state changed during the storage snapshot ({changed}); refusing to \
             publish an inconsistent backup"
        );
    }
    let repositories = after_snapshot;

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

    let material = key_material(passphrase)?;
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        created_unix_seconds: now_unix_seconds(),
        fence: FenceRecord {
            kind: fence.kind().into(),
            detail: fence.detail().into(),
        },
        postgres,
        repositories,
        config: config_part,
        ssh_host_key: host_key_part,
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
            "The PostgreSQL position is advisory: `dump_lsn` is pg_current_wal_lsn() read beside \
             the dump, not the dump's snapshot position, and restore does not compare it. \
             Database-to-storage consistency is proved by the per-repository journal positions."
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
    Ok(BackupReport {
        fence,
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

async fn repository_positions(config: &Config, pool: &PgPool) -> Result<Vec<RepositoryPosition>> {
    let rows: Vec<(Uuid, String, String, i64)> = sqlx::query_as(
        "SELECT id, status, object_format, journal_applied FROM repositories ORDER BY id",
    )
    .fetch_all(pool)
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
    for (id, status, object_format, journal_applied) in rows {
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
        positions.push(RepositoryPosition {
            id,
            status,
            object_format,
            journal_position,
            journal_applied: journal_applied.max(0) as u64,
        });
    }
    Ok(positions)
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
        create_database(&options.database_url, &database).await?;
        database_created = true;
        restore_database(&options.database_url, &root.join("database.dump"))?;

        let pool = crate::connect(&Config {
            database_url: options.database_url.clone(),
            ..Config::default()
        })
        .await?;
        let verification = verify_restored(&pool, &storage, &manifest).await;
        pool.close().await;
        verification?;

        publish_storage(&staged_storage, &destination_storage)?;
        storage_published = true;

        let rewritten = rewrite_configuration(
            &root.join("config.toml"),
            &options.database_url,
            &destination_storage,
            &destination_host_key,
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
            postgres: PostgresPosition {
                database: "d".into(),
                server_version: "16".into(),
                dump_lsn: "0/0".into(),
            },
            repositories: Vec::new(),
            config: ConfigPart {
                source: None,
                sha256: sha256_file(&staging.join("config.toml")).unwrap(),
                bytes: fs::metadata(staging.join("config.toml")).unwrap().len(),
                note: String::new(),
            },
            ssh_host_key: None,
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
