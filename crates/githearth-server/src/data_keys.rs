//! Per-account and per-organization data keys, and crypto-shredding
//! (M11a increment 8A, case U36; ADR 0015 "Deletion", ADR 0014 §3).
//!
//! # The hierarchy
//!
//! An account or organization has one **owner data key**: a random 32-byte key
//! wrapped by the installation key. Its stable id is derived from the owner:
//! SHA-256 over `domain || kind || 0x00 || 16 raw UUID bytes`, with the UUID
//! version and variant bits set (`UUIDv5`-shaped, not SHA-1 UUIDv5). That id is
//! the reference the account and organization rows carry
//! (`accounts.data_key_id`, `organizations.data_key_id`, owned by the
//! identity-schema lane) and it survives a restore without a lookup table.
//!
//! A secret that belongs to an owner is encrypted under a fresh per-secret
//! data key, and that data key is wrapped **by the owner key** rather than
//! directly by the installation key (`secret_data_keys.owner_key_id`).
//! Installation-level secrets — the audit IP key — keep the M4.2 shape and are
//! the "pre-key" data a backup predating this increment cannot shred.
//!
//! # Why the owner key is not in the database
//!
//! Crypto-shredding only works against an older backup if the backup cannot
//! carry the key. The installation key is already outside every archive; the
//! owner keys join it in an operator-custody directory (`data_key_directory`,
//! default: a `data-keys` directory beside `installation_key_file`). The
//! database carries only the owner key **id** and the secret ciphertext. An
//! older backup therefore contains the row, the ciphertext and the per-secret
//! wrapped data key, but not the owner key that opens it — provided no retained
//! copy of the custody directory still holds that key (see the honest limits
//! below and `docs/operations/secrets-at-rest.md`).
//!
//! Deleting an owner key file and writing a tombstone is the shred. A tombstone
//! is written before the key is removed, so an interrupted shred is still
//! effective: the loader treats a key with a tombstone as shredded even if the
//! file removal did not finish. A tombstone also stops a later `ensure` from
//! minting a replacement key for a deleted owner.
//!
//! The honest limits are recorded in `docs/operations/secrets-at-rest.md` and
//! in the restore output: a pre-key backup (all secrets wrapped directly under
//! the installation key) cannot be shredded at all, and plaintext profile
//! columns are bounded by retention, not by this key.

use crate::secrets::{self, InstallationKeySet};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const KEY_FILE_VERSION: u32 = 1;
const KEY_BYTES: usize = 32;
const FILE_LIMIT: u64 = 8192;
const OWNER_ID_DOMAIN: &[u8] = b"githearth-owner-data-key-v1\0";

/// Which kind of row owns a data key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerKind {
    Account,
    Organization,
}

impl OwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Organization => "organization",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "account" => Ok(Self::Account),
            "organization" => Ok(Self::Organization),
            other => bail!("unknown data-key owner kind {other:?}"),
        }
    }
}

/// The stable reference for an account's data key.
pub fn account_data_key_id(account: Uuid) -> Uuid {
    id_for(OwnerKind::Account, account)
}

/// The stable reference for an organization's data key.
pub fn organization_data_key_id(organization: Uuid) -> Uuid {
    id_for(OwnerKind::Organization, organization)
}

pub fn id_for(kind: OwnerKind, owner: Uuid) -> Uuid {
    // SHA-256 with the version/variant bits set, rather than the `uuid` crate's
    // v5 feature: the id must be deterministic and stable but does not need to
    // interoperate with another UUIDv5 producer. Changing the domain or the
    // encoding would orphan every existing key, so both are part of the
    // contract.
    let mut digest = Sha256::new();
    digest.update(OWNER_ID_DOMAIN);
    digest.update(kind.as_str().as_bytes());
    digest.update([0]);
    digest.update(owner.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.finalize()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// A present, opened owner key. The 32 bytes never leave this process except
/// as an AEAD key; `Debug` is redacted.
#[derive(Clone)]
pub struct OwnerKey {
    pub id: Uuid,
    pub owner_kind: OwnerKind,
    pub owner_id: Uuid,
    pub key: [u8; KEY_BYTES],
    pub kek_id: String,
    pub wrap_method: String,
}

impl std::fmt::Debug for OwnerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerKey")
            .field("id", &self.id)
            .field("owner_kind", &self.owner_kind)
            .field("owner_id", &self.owner_id)
            .field("kek_id", &self.kek_id)
            .field("wrap_method", &self.wrap_method)
            .field("key", &"[redacted]")
            .finish()
    }
}

/// What the custody directory knows about a key id.
pub enum State {
    Present(OwnerKey),
    Shredded,
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct KeyFile {
    version: u32,
    key_id: Uuid,
    owner_kind: String,
    owner_id: Uuid,
    kek_id: String,
    algorithm: String,
    wrap_method: String,
    /// The owner key wrapped under the installation key. Base64 because TOML
    /// has no byte string; it is ciphertext, never the key.
    wrapped_key: String,
    created_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ShredFile {
    version: u32,
    key_id: Uuid,
    owner_kind: String,
    owner_id: Uuid,
    /// Whether a key existed when the tombstone was written. `false` is the
    /// pre-key case: there was nothing to shred, which is recorded rather than
    /// pretended away.
    had_key: bool,
    /// Metadata of the destroyed key, so a backup manifest can still name the
    /// algorithm and wrap method it had. Never key material.
    #[serde(default)]
    kek_id: String,
    #[serde(default)]
    algorithm: String,
    #[serde(default)]
    wrap_method: String,
    shredded_unix_seconds: u64,
}

/// One key as a backup manifest may carry it: id, owner, algorithm and wrap
/// method, never key material.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataKeyRecord {
    pub key_id: Uuid,
    pub owner_kind: String,
    pub owner_id: Uuid,
    /// The installation key id that wraps this owner key, or the id recorded
    /// when it was shredded. Metadata only.
    pub kek_id: String,
    pub algorithm: String,
    pub wrap_method: String,
    pub shredded: bool,
    /// False only for a pre-key tombstone: the owner was deleted before it ever
    /// had a key, so there was nothing to destroy. A manifest must not pretend
    /// otherwise.
    #[serde(default = "key_was_present")]
    pub had_key: bool,
}

fn key_was_present() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct ShredReport {
    pub key_id: Uuid,
    pub owner_kind: OwnerKind,
    pub owner_id: Uuid,
    /// A key was present and destroyed.
    pub had_key: bool,
    /// The tombstone already existed; this call changed nothing.
    pub already_shredded: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RewrapReport {
    pub rewrapped: u64,
    pub remaining: u64,
}

#[derive(Clone, Debug, Default)]
pub struct KeyHealth {
    /// Every owner key the directory knows, present or shredded.
    pub records: Vec<DataKeyRecord>,
    /// Present keys still wrapped by a non-current installation key id.
    pub retired_kek_ids: Vec<String>,
    /// Present keys wrapped by an installation key that is not configured.
    pub unconfigured_kek_ids: Vec<String>,
    /// Present keys whose wrapper does not open.
    pub damaged_key_ids: Vec<Uuid>,
    pub shredded_keys: u64,
}

fn key_path(directory: &Path, key_id: Uuid) -> PathBuf {
    directory.join(format!("{key_id}.key"))
}

fn tombstone_path(directory: &Path, key_id: Uuid) -> PathBuf {
    directory.join(format!("{key_id}.shredded"))
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn ensure_directory(directory: &Path) -> Result<()> {
    if directory.as_os_str().is_empty() {
        bail!(
            "no data-key directory is configured; set data_key_directory \
             (GITHEARTH_DATA_KEY_DIRECTORY) outside storage_root"
        );
    }
    if directory.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create data-key directory {}", directory.display()))?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect data-key directory {}", directory.display()))?;
    Ok(())
}

/// Refuses an empty path before any read. `Path::new("")` joins to `./file`,
/// which would silently make the process working directory the key store.
fn require_directory(directory: &Path) -> Result<()> {
    if directory.as_os_str().is_empty() {
        bail!(
            "no data-key directory is configured; set data_key_directory \
             (GITHEARTH_DATA_KEY_DIRECTORY) outside storage_root"
        );
    }
    Ok(())
}

fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot inspect data-key file {}", path.display()))?;
    if metadata.len() > FILE_LIMIT {
        bail!("data-key file {} is larger than a key file", path.display());
    }
    fs::read(path).with_context(|| format!("cannot read data-key file {}", path.display()))
}

fn read_key_file(path: &Path) -> Result<KeyFile> {
    secrets::check_private_file(path, "data key file")?;
    let bytes = read_limited(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("data-key file {} is not UTF-8", path.display()))?;
    let file: KeyFile = toml::from_str(text)
        .with_context(|| format!("data-key file {} is not valid", path.display()))?;
    if file.version != KEY_FILE_VERSION {
        bail!(
            "data-key file {} has unsupported version {}",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

fn read_shred_file(path: &Path) -> Result<ShredFile> {
    secrets::check_private_file(path, "data-key tombstone")?;
    let bytes = read_limited(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("data-key tombstone {} is not UTF-8", path.display()))?;
    let file: ShredFile = toml::from_str(text)
        .with_context(|| format!("data-key tombstone {} is not valid", path.display()))?;
    if file.version != KEY_FILE_VERSION {
        bail!(
            "data-key tombstone {} has unsupported version {}",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

/// Writes `contents` to a fresh temp file in the same directory and renames it
/// into place, so a reader sees the old file or the new one, never a torn one.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = write_temporary(path, contents)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("cannot publish {}", path.display()));
    }
    Ok(())
}

/// Publishes `contents` only if `path` does not exist. Returns `false` when
/// another writer won the race; the caller adopts the existing file instead of
/// overwriting a key that may already protect ciphertext. A hard link is used
/// because it is atomic and never replaces an existing name on the same
/// filesystem.
fn publish_new(path: &Path, contents: &[u8]) -> Result<bool> {
    let temporary = write_temporary(path, contents)?;
    let result = match fs::hard_link(&temporary, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot publish {}", path.display())),
    };
    let _ = fs::remove_file(&temporary);
    result
}

fn write_temporary(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data-key".into());
    let temporary = directory.join(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("cannot create {}", temporary.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    Ok(temporary)
}

fn open_key_file(keys: &InstallationKeySet, file: &KeyFile, path: &Path) -> Result<OwnerKey> {
    let kind = OwnerKind::parse(&file.owner_kind)?;
    if file.algorithm != secrets::ALGORITHM {
        bail!(
            "data key {} uses algorithm {:?}, this binary writes {}",
            file.key_id,
            file.algorithm,
            secrets::ALGORITHM
        );
    }
    let wrapped = BASE64.decode(&file.wrapped_key).with_context(|| {
        format!(
            "data-key file {} has an invalid wrapped key",
            path.display()
        )
    })?;
    let key = keys
        .get(&file.kek_id)
        .with_context(|| {
            format!(
                "data key {} is wrapped by installation key {}, which is not configured; add it as \
                 installation_key_previous_file (rotation) or restore the correct installation key",
                file.key_id, file.kek_id
            )
        })?
        .unwrap_recorded(file.key_id, &file.wrap_method, &wrapped)
        .with_context(|| format!("data key {} cannot be opened", file.key_id))?;
    Ok(OwnerKey {
        id: file.key_id,
        owner_kind: kind,
        owner_id: file.owner_id,
        key,
        kek_id: file.kek_id.clone(),
        wrap_method: file.wrap_method.clone(),
    })
}

/// Reads the custody directory's state for one key id. A tombstone wins over a
/// present file, so an interrupted shred is still a shred.
pub fn state(keys: &InstallationKeySet, directory: &Path, key_id: Uuid) -> Result<State> {
    require_directory(directory)?;
    let tombstone = tombstone_path(directory, key_id);
    if tombstone.exists() {
        let _ = read_shred_file(&tombstone)?;
        return Ok(State::Shredded);
    }
    let path = key_path(directory, key_id);
    if !path.exists() {
        return Ok(State::Missing);
    }
    let file = read_key_file(&path)?;
    if file.key_id != key_id {
        bail!(
            "data-key file {} names key {}, expected {}",
            path.display(),
            file.key_id,
            key_id
        );
    }
    Ok(State::Present(open_key_file(keys, &file, &path)?))
}

/// A cheap state that does not unwrap: used where only presence matters.
fn shredded(directory: &Path, key_id: Uuid) -> bool {
    tombstone_path(directory, key_id).exists()
}

/// Creates the owner's data key if it does not exist and returns its stable id.
/// Refuses to recreate a shredded key: deletion is permanent.
pub fn ensure(
    keys: &InstallationKeySet,
    directory: &Path,
    kind: OwnerKind,
    owner: Uuid,
) -> Result<Uuid> {
    let key_id = id_for(kind, owner);
    let tombstone = tombstone_path(directory, key_id);
    if tombstone.exists() {
        let record = read_shred_file(&tombstone)?;
        bail!(
            "data key {key_id} for {} {owner} was shredded{}; refusing to create a new key for a \
             deleted owner",
            kind.as_str(),
            if record.had_key {
                ""
            } else {
                " (no key existed)"
            }
        );
    }
    let path = key_path(directory, key_id);
    if path.exists() {
        let file = read_key_file(&path)?;
        if file.key_id != key_id || file.owner_id != owner || file.owner_kind != kind.as_str() {
            bail!(
                "data-key file {} does not belong to {} {owner}",
                path.display(),
                kind.as_str()
            );
        }
        // Prove it opens before handing the reference to a caller that is
        // about to encrypt with it.
        let _ = open_key_file(keys, &file, &path)?;
        return Ok(key_id);
    }
    ensure_directory(directory)?;
    let mut key = [0u8; KEY_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
    let wrapped = keys.current().wrap(key_id, &key)?;
    let file = KeyFile {
        version: KEY_FILE_VERSION,
        key_id,
        owner_kind: kind.as_str().into(),
        owner_id: owner,
        kek_id: keys.current().id().into(),
        algorithm: secrets::ALGORITHM.into(),
        wrap_method: keys.current().method().into(),
        wrapped_key: BASE64.encode(wrapped),
        created_unix_seconds: now_unix_seconds(),
    };
    let text = toml::to_string(&file).context("cannot encode a data-key descriptor")?;
    if !publish_new(&path, text.as_bytes())? {
        // Another writer created the key between our check and publish. Adopt
        // its key; overwriting would strand the ciphertext it already protects.
        let existing = read_key_file(&path)?;
        if existing.key_id != key_id
            || existing.owner_id != owner
            || existing.owner_kind != kind.as_str()
        {
            bail!(
                "data-key file {} does not belong to {} {owner}",
                path.display(),
                kind.as_str()
            );
        }
        let _ = open_key_file(keys, &existing, &path)?;
    }
    // A concurrent shred wins: the tombstone is authoritative, so remove the
    // key we just created and refuse to hand it to a caller.
    if tombstone.exists() {
        let _ = fs::remove_file(&path);
        bail!(
            "data key {key_id} for {} {owner} was shredded while it was being created",
            kind.as_str()
        );
    }
    Ok(key_id)
}

/// Creates the account's key if needed and returns the reference the account
/// row stores (`accounts.data_key_id`).
pub fn ensure_account_data_key(
    keys: &InstallationKeySet,
    directory: &Path,
    account: Uuid,
) -> Result<Uuid> {
    ensure(keys, directory, OwnerKind::Account, account)
}

/// Creates the organization's key if needed and returns the reference the
/// organization row stores (`organizations.data_key_id`).
pub fn ensure_organization_data_key(
    keys: &InstallationKeySet,
    directory: &Path,
    organization: Uuid,
) -> Result<Uuid> {
    ensure(keys, directory, OwnerKind::Organization, organization)
}

/// Checks a stored `accounts.data_key_id` / `organizations.data_key_id`
/// reference against the id this installation derives for the owner. The
/// reference is informational and is never the shred input; callers use this
/// where the row is loaded. `None` is a pre-key row and is valid.
pub fn verify_reference(kind: OwnerKind, owner: Uuid, stored: Option<Uuid>) -> Result<()> {
    let expected = id_for(kind, owner);
    match stored {
        None => Ok(()),
        Some(stored) if stored == expected => Ok(()),
        Some(stored) => bail!(
            "{} {owner} carries data key reference {stored}, but this installation derives \
             {expected}; refusing a reference that does not belong to the owner",
            kind.as_str()
        ),
    }
}

/// Shreds the key a stored reference names, but only after the descriptor (or
/// tombstone) proves that key belongs to the owner. A reference that names no
/// key is refused rather than writing a tombstone for a phantom owner — the
/// failure mode where a deletion silently deletes nothing.
pub fn shred_by_reference(directory: &Path, kind: OwnerKind, key_id: Uuid) -> Result<ShredReport> {
    require_directory(directory)?;
    let tombstone = tombstone_path(directory, key_id);
    if tombstone.exists() {
        let record = read_shred_file(&tombstone)?;
        if record.key_id != key_id || record.owner_kind != kind.as_str() {
            bail!(
                "tombstone {} does not name {} key {key_id}",
                tombstone.display(),
                kind.as_str()
            );
        }
        verify_reference(kind, record.owner_id, Some(key_id))?;
        return Ok(ShredReport {
            key_id,
            owner_kind: kind,
            owner_id: record.owner_id,
            had_key: record.had_key,
            already_shredded: true,
        });
    }
    let path = key_path(directory, key_id);
    if !path.exists() {
        bail!(
            "no data key {key_id} in {}; refusing to write a tombstone for a reference that \
             names no key",
            directory.display()
        );
    }
    let file = read_key_file(&path)?;
    if file.key_id != key_id || file.owner_kind != kind.as_str() {
        bail!(
            "data-key file {} does not name {} key {key_id}; refusing to shred it",
            path.display(),
            kind.as_str()
        );
    }
    verify_reference(kind, file.owner_id, Some(key_id))?;
    shred(directory, kind, file.owner_id)
}

/// Destroys the account's key and every secret it protects, in the live
/// database and in every older backup whose custody copy no longer holds the
/// key. Idempotent.
pub fn shred_account_data_key(directory: &Path, account: Uuid) -> Result<ShredReport> {
    shred(directory, OwnerKind::Account, account)
}

/// Destroys the organization's key and every secret it protects, in the live
/// database and in every older backup whose custody copy no longer holds the
/// key. Idempotent.
pub fn shred_organization_data_key(directory: &Path, organization: Uuid) -> Result<ShredReport> {
    shred(directory, OwnerKind::Organization, organization)
}

/// Destroys the owner's data key. The tombstone is written first, then the key
/// file is removed; after this call every secret under the key is unreadable in
/// the live database and in every backup whose custody copy no longer holds the
/// key.
pub fn shred(directory: &Path, kind: OwnerKind, owner: Uuid) -> Result<ShredReport> {
    let key_id = id_for(kind, owner);
    let tombstone = tombstone_path(directory, key_id);
    if tombstone.exists() {
        let record = read_shred_file(&tombstone)?;
        return Ok(ShredReport {
            key_id,
            owner_kind: kind,
            owner_id: owner,
            had_key: record.had_key,
            already_shredded: true,
        });
    }
    let path = key_path(directory, key_id);
    let had_key = path.exists();
    // Keep the metadata a manifest may name, but a damaged descriptor is still
    // destroyed rather than preserved.
    let metadata = if had_key {
        read_key_file(&path).ok()
    } else {
        None
    };
    if let Some(file) = &metadata
        && (file.key_id != key_id || file.owner_id != owner || file.owner_kind != kind.as_str())
    {
        bail!(
            "data-key file {} does not belong to {} {owner}; refusing to shred the wrong key",
            path.display(),
            kind.as_str()
        );
    }
    ensure_directory(directory)?;
    let record = ShredFile {
        version: KEY_FILE_VERSION,
        key_id,
        owner_kind: kind.as_str().into(),
        owner_id: owner,
        had_key,
        kek_id: metadata
            .as_ref()
            .map(|file| file.kek_id.clone())
            .unwrap_or_default(),
        algorithm: metadata
            .as_ref()
            .map(|file| file.algorithm.clone())
            .unwrap_or_else(|| secrets::ALGORITHM.into()),
        wrap_method: metadata
            .as_ref()
            .map(|file| file.wrap_method.clone())
            .unwrap_or_default(),
        shredded_unix_seconds: now_unix_seconds(),
    };
    let text = toml::to_string(&record).context("cannot encode a data-key tombstone")?;
    if !publish_new(&tombstone, text.as_bytes())? {
        // Another shred won. The tombstone is authoritative; make sure no key
        // file survives it and report the existing tombstone.
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        let existing = read_shred_file(&tombstone)?;
        return Ok(ShredReport {
            key_id,
            owner_kind: kind,
            owner_id: owner,
            had_key: existing.had_key,
            already_shredded: true,
        });
    }
    if had_key {
        fs::remove_file(&path)
            .with_context(|| format!("cannot remove shredded data key {}", path.display()))?;
    }
    Ok(ShredReport {
        key_id,
        owner_kind: kind,
        owner_id: owner,
        had_key,
        already_shredded: false,
    })
}

/// Every key the custody directory knows about, metadata only, suitable for a
/// backup manifest. Tombstoned keys are listed with `shredded: true`.
pub fn records(directory: &Path) -> Result<Vec<DataKeyRecord>> {
    require_directory(directory)?;
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("cannot list data-key directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let is_key = path.extension().is_some_and(|ext| ext == "key");
        let is_shred = path.extension().is_some_and(|ext| ext == "shredded");
        if !is_key && !is_shred {
            continue;
        }
        let key_id = match path.file_stem().and_then(|stem| stem.to_str()) {
            Some(stem) => match Uuid::parse_str(stem) {
                Ok(id) => id,
                Err(_) => continue,
            },
            None => continue,
        };
        if shredded(directory, key_id) {
            // The tombstone is authoritative even if a key file survived an
            // interrupted removal.
            if out
                .iter()
                .any(|record: &DataKeyRecord| record.key_id == key_id)
            {
                continue;
            }
            let record = read_shred_file(&tombstone_path(directory, key_id))?;
            out.push(DataKeyRecord {
                key_id,
                owner_kind: record.owner_kind,
                owner_id: record.owner_id,
                kek_id: record.kek_id,
                algorithm: record.algorithm,
                wrap_method: record.wrap_method,
                shredded: true,
                had_key: record.had_key,
            });
            continue;
        }
        if is_shred || !is_key {
            continue;
        }
        let file = read_key_file(&path)?;
        if file.key_id != key_id {
            bail!(
                "data-key file {} names key {}, expected {}",
                path.display(),
                file.key_id,
                key_id
            );
        }
        out.push(DataKeyRecord {
            key_id: file.key_id,
            owner_kind: file.owner_kind,
            owner_id: file.owner_id,
            kek_id: file.kek_id,
            algorithm: file.algorithm,
            wrap_method: file.wrap_method,
            shredded: false,
            had_key: true,
        });
    }
    out.sort_by_key(|record| record.key_id);
    Ok(out)
}

/// Checks every present owner key against the configured installation keys.
/// It never returns key bytes. Used by `startup`, `status`, `backup` and
/// `doctor`.
pub fn health(keys: &InstallationKeySet, directory: &Path) -> Result<KeyHealth> {
    require_directory(directory)?;
    if !directory.is_dir() {
        return Ok(KeyHealth::default());
    }
    let mut health = KeyHealth::default();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("cannot list data-key directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "key") {
            continue;
        }
        let Some(key_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        if shredded(directory, key_id) {
            health.shredded_keys += 1;
            continue;
        }
        let file = read_key_file(&path)?;
        if file.key_id != key_id {
            bail!(
                "data-key file {} names key {}, expected {}",
                path.display(),
                file.key_id,
                key_id
            );
        }
        health.records.push(DataKeyRecord {
            key_id: file.key_id,
            owner_kind: file.owner_kind.clone(),
            owner_id: file.owner_id,
            kek_id: file.kek_id.clone(),
            algorithm: file.algorithm.clone(),
            wrap_method: file.wrap_method.clone(),
            shredded: false,
            had_key: true,
        });
        match keys.get(&file.kek_id) {
            None => health.unconfigured_kek_ids.push(file.kek_id.clone()),
            Some(_) => match open_key_file(keys, &file, &path) {
                Ok(opened) => {
                    health.records.last_mut().expect("just pushed").kek_id = opened.kek_id.clone();
                    if file.kek_id != keys.current().id()
                        || file.wrap_method != keys.current().method()
                    {
                        health.retired_kek_ids.push(file.kek_id.clone());
                    }
                }
                Err(_) => health.damaged_key_ids.push(file.key_id),
            },
        }
    }
    // Tombstones are records too, but `health` reports them by the count above;
    // a manifest asks `records` for the full list.
    health.shredded_keys = shed_count(directory)?;
    health.retired_kek_ids.sort();
    health.retired_kek_ids.dedup();
    health.unconfigured_kek_ids.sort();
    health.unconfigured_kek_ids.dedup();
    Ok(health)
}

fn shed_count(directory: &Path) -> Result<u64> {
    if !directory.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|ext| ext == "shredded")
        {
            count += 1;
        }
    }
    Ok(count)
}

/// Re-wraps every present owner key under the current installation key. Only
/// the 32-byte owner key moves between wrappers; secret ciphertext is
/// untouched. Returns the number re-wrapped and how many still need it.
pub fn rewrap(keys: &InstallationKeySet, directory: &Path) -> Result<RewrapReport> {
    let mut report = RewrapReport::default();
    require_directory(directory)?;
    if !directory.is_dir() {
        return Ok(report);
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("cannot list data-key directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "key") {
            continue;
        }
        let Some(key_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        if shredded(directory, key_id) {
            continue;
        }
        let file = read_key_file(&path)?;
        let source = keys.get(&file.kek_id).with_context(|| {
            format!(
                "data key {} is under installation key {}, which is not configured; add it as \
                 installation_key_previous_file before rotating",
                file.key_id, file.kek_id
            )
        })?;
        let wrapped = BASE64
            .decode(&file.wrapped_key)
            .with_context(|| format!("data-key file {} has an invalid wrapper", path.display()))?;
        let material = source
            .unwrap_recorded(file.key_id, &file.wrap_method, &wrapped)
            .with_context(|| format!("data key {} cannot be opened", file.key_id))?;
        if file.kek_id == keys.current().id() && file.wrap_method == keys.current().method() {
            continue;
        }
        let rewrapped = keys.current().wrap(file.key_id, &material)?;
        let updated = KeyFile {
            kek_id: keys.current().id().into(),
            wrap_method: keys.current().method().into(),
            wrapped_key: BASE64.encode(rewrapped),
            ..file
        };
        let text = toml::to_string(&updated).context("cannot encode a data-key descriptor")?;
        // A shred between the check above and this rename writes the tombstone
        // and removes the key file; renaming would recreate key material on
        // disk. The tombstone is authoritative, so drop the rewrite instead.
        if tombstone_path(directory, key_id).exists() {
            let _ = fs::remove_file(&path);
            continue;
        }
        write_atomic(&path, text.as_bytes())?;
        report.rewrapped += 1;
    }
    report.remaining = records(directory)?
        .into_iter()
        .filter(|record| !record.shredded)
        .filter(|record| {
            record.kek_id != keys.current().id() || record.wrap_method != keys.current().method()
        })
        .count() as u64;
    Ok(report)
}

/// The 32-byte owner key, for the dump/archive tests. It is a read of the
/// operator's custody file, not an API and not a production read path.
#[doc(hidden)]
pub fn test_key_bytes(
    keys: &InstallationKeySet,
    directory: &Path,
    key_id: Uuid,
) -> Result<Vec<u8>> {
    match state(keys, directory, key_id)? {
        State::Present(key) => Ok(key.key.to_vec()),
        State::Shredded => bail!("data key {key_id} was shredded"),
        State::Missing => bail!("data key {key_id} is missing"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets;

    fn installation(directory: &Path) -> (InstallationKeySet, PathBuf) {
        let key = directory.join("installation.key");
        secrets::generate_raw_key_file(&key).unwrap();
        let keys = InstallationKeySet::from_paths(&key, None, None)
            .unwrap()
            .unwrap();
        (keys, directory.join("data-keys"))
    }

    #[test]
    fn the_reference_is_stable_and_owner_kind_specific() {
        // The identity-schema lane pins the same two vectors in
        // `tests/identity.rs`; both lanes must produce exactly these.
        assert_eq!(
            account_data_key_id(Uuid::nil()),
            Uuid::parse_str("1ceb9657-767e-57c7-8761-8f47e3bb7126").unwrap()
        );
        assert_eq!(
            organization_data_key_id(Uuid::nil()),
            Uuid::parse_str("d115e2d0-1551-5d38-a8ba-d2047adc4a7c").unwrap()
        );
        let account = Uuid::new_v4();
        let organization = Uuid::new_v4();
        assert_eq!(account_data_key_id(account), account_data_key_id(account));
        assert_eq!(
            id_for(OwnerKind::Account, account),
            account_data_key_id(account)
        );
        assert_ne!(
            account_data_key_id(account),
            organization_data_key_id(account)
        );
        assert_ne!(
            account_data_key_id(account),
            account_data_key_id(organization)
        );
        assert_ne!(
            account_data_key_id(account),
            organization_data_key_id(organization)
        );
    }

    #[test]
    fn ensure_writes_a_wrapped_key_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let id = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();
        assert_eq!(id, account_data_key_id(account));
        let path = key_path(&directory, id);
        assert!(path.is_file());
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the custody file is private");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(keys.current().id()));
        assert!(
            !text.lines().any(|line| line.starts_with("key = ")),
            "the descriptor holds no plaintext key"
        );
        // The wrapped owner key opens under the installation key.
        let first = match state(&keys, &directory, id).unwrap() {
            State::Present(key) => key.key,
            _ => panic!("the key must be present"),
        };
        let again = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();
        assert_eq!(again, id);
        let second = match state(&keys, &directory, id).unwrap() {
            State::Present(key) => key.key,
            _ => panic!("the key must be present"),
        };
        assert_eq!(first, second, "ensure does not replace an existing key");
    }

    #[test]
    fn shred_removes_the_key_and_a_later_ensure_refuses_to_resurrect_it() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let organization = Uuid::new_v4();
        let id = ensure(&keys, &directory, OwnerKind::Organization, organization).unwrap();
        let report = shred(&directory, OwnerKind::Organization, organization).unwrap();
        assert!(report.had_key);
        assert!(!report.already_shredded);
        assert!(!key_path(&directory, id).exists());
        assert!(tombstone_path(&directory, id).is_file());
        assert!(matches!(
            state(&keys, &directory, id).unwrap(),
            State::Shredded
        ));
        let error = ensure(&keys, &directory, OwnerKind::Organization, organization).unwrap_err();
        assert!(error.to_string().contains("was shredded"), "{error}");
        let again = shred(&directory, OwnerKind::Organization, organization).unwrap();
        assert!(again.already_shredded);
        assert!(again.had_key);
    }

    #[test]
    fn shredding_a_pre_key_owner_records_that_there_was_nothing_to_shred() {
        let temp = tempfile::tempdir().unwrap();
        let (_keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let report = shred(&directory, OwnerKind::Account, account).unwrap();
        assert!(!report.had_key);
        assert!(!report.already_shredded);
        assert!(tombstone_path(&directory, report.key_id).is_file());
    }

    #[test]
    fn an_interrupted_shred_is_still_effective() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let id = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();
        // Simulate a crash after the tombstone is written and before the key
        // file is removed.
        let record = ShredFile {
            version: KEY_FILE_VERSION,
            key_id: id,
            owner_kind: "account".into(),
            owner_id: account,
            had_key: true,
            kek_id: keys.current().id().into(),
            algorithm: secrets::ALGORITHM.into(),
            wrap_method: keys.current().method().into(),
            shredded_unix_seconds: now_unix_seconds(),
        };
        fs::write(
            tombstone_path(&directory, id),
            toml::to_string(&record).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            tombstone_path(&directory, id),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(key_path(&directory, id).exists());
        assert!(matches!(
            state(&keys, &directory, id).unwrap(),
            State::Shredded
        ));
        assert!(ensure(&keys, &directory, OwnerKind::Account, account).is_err());
    }

    #[test]
    fn records_carry_key_metadata_only() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let organization = Uuid::new_v4();
        let account_key = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();
        let organization_key =
            ensure(&keys, &directory, OwnerKind::Organization, organization).unwrap();
        shred(&directory, OwnerKind::Organization, organization).unwrap();
        let records = records(&directory).unwrap();
        assert_eq!(records.len(), 2);
        let account_record = records
            .iter()
            .find(|record| record.key_id == account_key)
            .unwrap();
        assert_eq!(account_record.owner_kind, "account");
        assert_eq!(account_record.owner_id, account);
        assert_eq!(account_record.algorithm, secrets::ALGORITHM);
        assert_eq!(account_record.wrap_method, keys.current().method());
        assert!(!account_record.shredded);
        assert!(account_record.had_key);
        let organization_record = records
            .iter()
            .find(|record| record.key_id == organization_key)
            .unwrap();
        assert!(organization_record.shredded);
        assert!(organization_record.had_key);
        let encoded = serde_json::to_string(&records).unwrap();
        assert!(!encoded.contains("wrapped_key"));
        assert!(!encoded.contains(&BASE64.encode([0u8; 32])));
    }

    #[test]
    fn rotation_rewraps_owner_keys_without_changing_them() {
        let temp = tempfile::tempdir().unwrap();
        let (old, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let id = ensure(&old, &directory, OwnerKind::Account, account).unwrap();
        let before = test_key_bytes(&old, &directory, id).unwrap();

        let new_path = temp.path().join("installation.new.key");
        secrets::generate_raw_key_file(&new_path).unwrap();
        let new = InstallationKeySet::from_paths(
            &new_path,
            Some(temp.path().join("installation.key").as_path()),
            None,
        )
        .unwrap()
        .unwrap();
        let report = rewrap(&new, &directory).unwrap();
        assert_eq!(report.rewrapped, 1);
        assert_eq!(report.remaining, 0);
        assert_eq!(test_key_bytes(&new, &directory, id).unwrap(), before);
        let only_new = InstallationKeySet::from_paths(&new_path, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            test_key_bytes(&only_new, &directory, id).unwrap(),
            before,
            "after rotation the old key is not needed"
        );
    }

    #[test]
    fn a_pre_key_tombstone_is_recorded_as_having_no_key() {
        let temp = tempfile::tempdir().unwrap();
        let (_keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let report = shred(&directory, OwnerKind::Account, account).unwrap();
        assert!(!report.had_key);
        let records = records(&directory).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].shredded);
        assert!(
            !records[0].had_key,
            "a manifest must not pretend a key existed"
        );
    }

    #[test]
    fn an_empty_directory_is_refused_instead_of_using_the_process_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, _directory) = installation(temp.path());
        let key_id = account_data_key_id(Uuid::new_v4());
        let empty = Path::new("");
        assert!(state(&keys, empty, key_id).is_err());
        assert!(records(empty).is_err());
        assert!(health(&keys, empty).is_err());
    }

    #[test]
    fn a_descriptor_renamed_to_another_id_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let id = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();
        let renamed = key_path(&directory, Uuid::new_v4());
        fs::rename(key_path(&directory, id), &renamed).unwrap();
        let error = records(&directory).unwrap_err().to_string();
        assert!(error.contains("names key"), "{error}");
        assert!(health(&keys, &directory).is_err());
    }

    #[test]
    fn a_stored_reference_must_be_the_derived_id() {
        let account = Uuid::new_v4();
        let derived = account_data_key_id(account);
        verify_reference(OwnerKind::Account, account, None).unwrap();
        verify_reference(OwnerKind::Account, account, Some(derived)).unwrap();
        let error =
            verify_reference(OwnerKind::Account, account, Some(Uuid::new_v4())).unwrap_err();
        assert!(error.to_string().contains("does not belong"), "{error}");
    }

    #[test]
    fn shred_by_reference_refuses_a_phantom_key_and_shreds_the_real_one() {
        let temp = tempfile::tempdir().unwrap();
        let (keys, directory) = installation(temp.path());
        let account = Uuid::new_v4();
        let id = ensure(&keys, &directory, OwnerKind::Account, account).unwrap();

        // An app lane that passes the wrong value where an owner UUID belongs
        // derives a phantom id. It must not write a tombstone that deletes
        // nothing while the real key survives.
        let phantom = account_data_key_id(Uuid::new_v4());
        let error = shred_by_reference(&directory, OwnerKind::Account, phantom).unwrap_err();
        assert!(error.to_string().contains("names no key"), "{error}");
        assert!(!tombstone_path(&directory, phantom).exists());
        assert!(key_path(&directory, id).exists());

        let report = shred_by_reference(&directory, OwnerKind::Account, id).unwrap();
        assert!(report.had_key);
        assert_eq!(report.owner_id, account);
        assert!(!key_path(&directory, id).exists());
    }
}
