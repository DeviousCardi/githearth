//! S3-shaped object storage, with a durable filesystem backend (ADR 0014 §1).
//!
//! The **interface** is [`ObjectStore`]: put, ranged get, delete, checksum,
//! prefix listing, multipart/resumable upload, append for a log that is still
//! being written, tenant-scoped keys, usage accounting and abandoned-upload
//! cleanup. M5 implements the distributed backend behind this same trait; M6
//! (logs, artifacts, caches) and M7 (LFS, packages) are its first consumers.
//!
//! The filesystem backend ([`FsObjectStore`]) is **key-addressed**, not
//! content-addressed: the caller chooses the key, an overwrite replaces, a
//! delete removes, and prefix listing walks a key namespace. The reasons and
//! the on-disk formats are recorded in ADR 0014 "Open items" 1, 5 and 7.
//!
//! No caller should depend on a node-local path: `FsObjectStore` is the
//! single-node implementation behind the trait, and the path helpers on it are
//! `#[doc(hidden)]` and used only by tests and operators.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// The number of payload bytes covered by one recorded chunk checksum.
/// A ranged read verifies only the chunks it touches.
pub const CHUNK_BYTES: usize = 1 << 20;
/// The largest object the filesystem backend accepts. `put` and `get` take
/// and return a whole `Vec<u8>`, so this is also the worst-case buffer a
/// whole-object read allocates (ADR 0014, "Streaming and size bounds").
/// Multipart uploads assemble at this bound without buffering a whole part.
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// The largest single multipart part. Matches S3's part ceiling.
pub const DEFAULT_MAX_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const META_VERSION: u32 = 1;
const USAGE_VERSION: u32 = 1;
const MAX_KEY_BYTES: usize = 1024;
const MAX_PART_NUMBER: u32 = 10_000;

// ---------------------------------------------------------------------------
// Type names
// ---------------------------------------------------------------------------

/// The organization that owns a key namespace. Every operation is scoped to
/// one tenant, so a prefix, a delete, a backup or a quota cannot cross tenants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(Uuid);

impl TenantId {
    pub fn new(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    fn simple(self) -> String {
        self.0.simple().to_string()
    }
}

impl From<Uuid> for TenantId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A key relative to a tenant prefix. The grammar is deliberately narrower
/// than S3's: ASCII letters, digits, `-`, `_`, `.`, `+` and `=`, in non-empty
/// `/`-separated segments, none of them `.` or `..`, at most 1024 bytes.
/// This makes a key safe as a path without an escaping layer and keeps prefix
/// listing a directory walk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    pub fn parse(value: &str) -> Result<Self, ObjectStoreError> {
        if value.is_empty() || value.len() > MAX_KEY_BYTES {
            return Err(ObjectStoreError::InvalidKey(format!(
                "key must be 1..={MAX_KEY_BYTES} bytes"
            )));
        }
        for segment in value.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." || segment.len() > 255 {
                return Err(ObjectStoreError::InvalidKey(format!(
                    "key segment {segment:?} is not allowed"
                )));
            }
            if !segment.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'=')
            }) {
                return Err(ObjectStoreError::InvalidKey(format!(
                    "key segment {segment:?} contains a character outside [A-Za-z0-9._+=-]"
                )));
            }
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for segment in self.0.split('/') {
            path.push(segment);
        }
        path
    }
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An inclusive byte range. `get` accepts `None` for the whole object.
///
/// Because both ends are inclusive, `ByteRange::new(5, 5)` is the one-byte
/// range at offset 5. A zero-length range is not representable; a caller that
/// wants to know whether an object is empty uses [`ObjectStore::stat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Result<Self, ObjectStoreError> {
        if start > end {
            return Err(ObjectStoreError::InvalidRange { start, end });
        }
        Ok(Self { start, end })
    }
}

/// A multipart upload's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UploadId(Uuid);

impl UploadId {
    pub fn parse(value: &str) -> Result<Self, ObjectStoreError> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| ObjectStoreError::InvalidUpload(format!("{value:?} is not an upload id")))
    }

    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a write recorded: the key's length and checksum, never its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub tenant: TenantId,
    pub key: ObjectKey,
    pub bytes: u64,
    pub sha256: String,
    pub appendable: bool,
    pub updated_unix_seconds: u64,
}

/// One entry from a prefix listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListEntry {
    pub key: ObjectKey,
    pub bytes: u64,
    pub sha256: String,
    pub updated_unix_seconds: u64,
}

/// One object in the installation-wide inventory a backup records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRecord {
    pub tenant: Uuid,
    pub key: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Part metadata, returned by `upload_part` and `list_parts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartMeta {
    pub part_number: u32,
    pub bytes: u64,
    pub sha256: String,
}

/// What one tenant currently stores. `bytes` is what the organization's quota
/// charges: committed object bytes plus the bytes of open uploads' parts, as
/// ADR 0014 §1 requires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub objects: u64,
    pub bytes: u64,
    pub uploads: u64,
    pub upload_bytes: u64,
}

/// An upload the deadline sweep removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbandonedUpload {
    pub tenant: TenantId,
    pub key: ObjectKey,
    pub upload: UploadId,
    pub updated_unix_seconds: u64,
}

/// One integrity problem found by [`ObjectStore::verify`] or
/// [`FsObjectStore::verify_all`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectProblemKind {
    /// The object's bytes or sidecar disagree with the recorded checksum.
    Corrupt,
    /// A payload with no usable sidecar, or a sidecar with no payload. This is
    /// the residue a crash in a two-file write can leave; it is reported for a
    /// repair command but does not fail backup verification (ADR 0014).
    Orphan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectProblem {
    pub key: String,
    pub kind: ObjectProblemKind,
    pub message: String,
}

/// The operations every backend provides. Reads are byte ranges, not whole
/// objects; a caller that wants a whole object asks for `None`.
pub trait ObjectStore {
    fn put(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
    ) -> Result<ObjectMeta, ObjectStoreError>;

    /// Create an object that [`append`](ObjectStore::append) may extend. A log
    /// is created this way before its first line is written.
    fn create_appendable(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<ObjectMeta, ObjectStoreError>;

    /// Append to an appendable object. The append is committed to the recorded
    /// length and checksum only after the bytes are durable.
    fn append(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
    ) -> Result<ObjectMeta, ObjectStoreError>;

    fn get(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Vec<u8>, ObjectStoreError>;

    fn stat(&self, tenant: TenantId, key: &ObjectKey) -> Result<ObjectMeta, ObjectStoreError>;

    /// The checksum recorded at write. It is not a fresh read of the bytes;
    /// [`verify`](ObjectStore::verify) does that.
    fn checksum(&self, tenant: TenantId, key: &ObjectKey) -> Result<String, ObjectStoreError>;

    /// Re-read the object and compare it with its recorded checksum.
    fn verify(&self, tenant: TenantId, key: &ObjectKey) -> Result<(), ObjectStoreError>;

    /// Idempotent: deleting a key that does not exist succeeds.
    fn delete(&self, tenant: TenantId, key: &ObjectKey) -> Result<(), ObjectStoreError>;

    fn list(&self, tenant: TenantId, prefix: &str) -> Result<Vec<ListEntry>, ObjectStoreError>;

    fn create_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<UploadId, ObjectStoreError>;

    fn upload_part(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        part_number: u32,
        data: &[u8],
    ) -> Result<PartMeta, ObjectStoreError>;

    fn list_parts(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<Vec<PartMeta>, ObjectStoreError>;

    /// Assemble the upload once. `parts` names an explicit, strictly
    /// increasing set; `None` assembles every part currently present.
    fn complete_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        parts: Option<&[u32]>,
    ) -> Result<ObjectMeta, ObjectStoreError>;

    /// Remove an upload and its parts. Idempotent.
    fn abort_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<(), ObjectStoreError>;

    fn usage(&self, tenant: TenantId) -> Result<Usage, ObjectStoreError>;

    /// Every tenant's every object. Operator/backup use only; a normal
    /// consumer reads one tenant prefix. A payload without a usable sidecar
    /// (the residue of a crash between the two writes) is skipped rather than
    /// failing the whole inventory; [`FsObjectStore::verify_all`] reports it.
    fn inventory(&self) -> Result<Vec<ObjectRecord>, ObjectStoreError>;

    /// Recompute every tenant's durable byte counter from the objects on
    /// disk. O(all objects); the reconciler calls it once per pass, never on
    /// a write. A no-op when the organization quota is disabled.
    fn reconcile_usage(&self) -> Result<(), ObjectStoreError>;

    /// Remove uploads whose last activity is older than the configured
    /// deadline. Returns what was removed.
    fn cleanup_abandoned(
        &self,
        now_unix_seconds: u64,
    ) -> Result<Vec<AbandonedUpload>, ObjectStoreError>;
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ObjectStoreError {
    NotFound {
        tenant: TenantId,
        key: ObjectKey,
    },
    InvalidKey(String),
    InvalidUpload(String),
    InvalidRange {
        start: u64,
        end: u64,
    },
    RangeNotSatisfiable {
        start: u64,
        end: u64,
        len: u64,
    },
    TooLarge {
        key: String,
        bytes: u64,
        limit: u64,
    },
    ChecksumMismatch {
        key: String,
        expected: String,
        actual: String,
    },
    Corrupt {
        key: String,
        reason: String,
    },
    QuotaExceeded {
        used: u64,
        incoming: u64,
        limit: u64,
    },
    Conflict(String),
    Io(io::Error),
}

impl std::fmt::Display for ObjectStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { tenant, key } => {
                write!(f, "object {key} not found in tenant {tenant}")
            }
            Self::InvalidKey(reason) => write!(f, "invalid object key: {reason}"),
            Self::InvalidUpload(reason) => write!(f, "invalid multipart upload: {reason}"),
            Self::InvalidRange { start, end } => {
                write!(f, "byte range {start}..={end} is inverted")
            }
            Self::RangeNotSatisfiable { start, end, len } => write!(
                f,
                "byte range {start}..={end} is not satisfiable for an object of {len} bytes"
            ),
            Self::TooLarge { key, bytes, limit } => write!(
                f,
                "object {key} would be {bytes} bytes, over the {limit}-byte limit"
            ),
            Self::ChecksumMismatch {
                key,
                expected,
                actual,
            } => write!(
                f,
                "object {key} failed integrity verification: expected sha256 {expected}, read {actual}"
            ),
            Self::Corrupt { key, reason } => write!(f, "object {key} is corrupt: {reason}"),
            Self::QuotaExceeded {
                used,
                incoming,
                limit,
            } => write!(
                f,
                "organization storage quota exceeded: {used} bytes stored, {incoming} incoming, limit {limit}"
            ),
            Self::Conflict(reason) => write!(f, "object store conflict: {reason}"),
            Self::Io(error) => write!(f, "object store i/o error: {error}"),
        }
    }
}

impl std::error::Error for ObjectStoreError {}

impl From<io::Error> for ObjectStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ObjectStoreOptions {
    /// How long an upload may sit without activity before the reconciler
    /// removes it (ADR 0014 §1). The server supplies this from configuration.
    pub upload_deadline: std::time::Duration,
    /// The per-organization byte limit. `0` disables the check, matching how
    /// `repository_quota_bytes` treats `0`.
    pub quota_bytes: u64,
    /// The largest object `put`, `append` or `complete_multipart` accepts.
    pub max_object_bytes: u64,
    /// The largest single multipart part `upload_part` accepts.
    pub max_part_bytes: u64,
}

impl Default for ObjectStoreOptions {
    fn default() -> Self {
        Self {
            upload_deadline: std::time::Duration::from_secs(86_400),
            quota_bytes: 0,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_part_bytes: DEFAULT_MAX_PART_BYTES,
        }
    }
}

/// The single-node backend. Layout:
///
/// ```text
/// <root>/data/<tenant>/<key...>                 payload bytes
/// <root>/meta/<tenant>/<key...>.json            checksum, size, chunk table
/// <root>/uploads/<tenant>/<key...>/<upload id>/ upload manifest + parts
/// <root>/usage/<tenant>.json                    durable byte counter (cache)
/// <root>/tmp/                                   atomic-write scratch
/// <root>/locks/                                 per-key, per-tenant gate and quota flocks
/// ```
///
/// `usage/` is a derived cache: it is excluded from backups, rebuilt lazily,
/// and never treated as an object. The durable records are `data/` and `meta/`.
pub struct FsObjectStore {
    root: PathBuf,
    options: ObjectStoreOptions,
}

struct KeyGuard {
    _file: fs::File,
}

/// A quota decision held across the write it authorised. The counter already
/// includes the reservation; dropping the guard without `commit` reverses it.
struct QuotaReservation<'a> {
    store: &'a FsObjectStore,
    tenant: TenantId,
    delta: i64,
    active: bool,
    committed: bool,
    _gate: Option<KeyGuard>,
}

impl QuotaReservation<'_> {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for QuotaReservation<'_> {
    fn drop(&mut self) {
        if self.active && !self.committed && self.delta != 0 {
            // The counter is a durable cache; reconciliation repairs a failed
            // rollback. A lost rollback can only overcharge, never undercharge.
            let _ = self.store.adjust_usage(self.tenant, -self.delta);
        }
    }
}

impl FsObjectStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        Self::open_with(root, ObjectStoreOptions::default())
    }

    pub fn open_with(
        root: impl AsRef<Path>,
        options: ObjectStoreOptions,
    ) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        for sub in ["data", "meta", "uploads", "usage", "tmp", "locks"] {
            ensure_dir(&root.join(sub))?;
        }
        let store = Self {
            root: root.to_path_buf(),
            options,
        };
        // A crash can interrupt a write after its payload and sidecar are
        // durable but before the quota counter is updated. No writer is
        // running yet, so one scan makes every counter exact again.
        store.reconcile_usage()?;
        Ok(store)
    }

    /// Read-only open. A missing root is refused; a backup without an object
    /// store (an archive taken before M4.2) is handled by the caller.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, ObjectStoreError> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(ObjectStoreError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("object store root {} does not exist", root.display()),
            )));
        }
        Ok(Self {
            root: root.to_path_buf(),
            options: ObjectStoreOptions::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The payload path for a key. Node-local: tests and operators only.
    #[doc(hidden)]
    pub fn data_path(&self, tenant: TenantId, key: &ObjectKey) -> PathBuf {
        self.data_dir(tenant).join(key.relative_path())
    }

    fn tenant_root(&self, tree: &str, tenant: TenantId) -> PathBuf {
        self.root.join(tree).join(tenant.simple())
    }

    fn data_dir(&self, tenant: TenantId) -> PathBuf {
        self.tenant_root("data", tenant)
    }

    fn meta_dir(&self, tenant: TenantId) -> PathBuf {
        self.tenant_root("meta", tenant)
    }

    fn meta_path(&self, tenant: TenantId, key: &ObjectKey) -> PathBuf {
        let mut path = self.meta_dir(tenant).join(key.relative_path());
        let name = path
            .file_name()
            .map(|name| format!("{}.json", name.to_string_lossy()))
            .unwrap_or_else(|| "object.json".to_owned());
        path.set_file_name(name);
        path
    }

    fn upload_dir(&self, tenant: TenantId, key: &ObjectKey, upload: &UploadId) -> PathBuf {
        self.tenant_root("uploads", tenant)
            .join(key.relative_path())
            .join(upload.to_string())
    }

    fn usage_path(&self, tenant: TenantId) -> PathBuf {
        self.root
            .join("usage")
            .join(format!("{}.json", tenant.simple()))
    }

    fn flock_file(
        &self,
        name: &str,
        operation: rustix::fs::FlockOperation,
    ) -> Result<KeyGuard, ObjectStoreError> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("locks").join(name))?;
        loop {
            match rustix::fs::flock(&file, operation) {
                Ok(()) => return Ok(KeyGuard { _file: file }),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(ObjectStoreError::Io(error.into())),
            }
        }
    }

    fn lock_key(&self, tenant: TenantId, key: &ObjectKey) -> Result<KeyGuard, ObjectStoreError> {
        let mut digest = Sha256::new();
        digest.update(tenant.simple().as_bytes());
        digest.update(b"/");
        digest.update(key.as_str().as_bytes());
        let name = hex::encode(digest.finalize());
        self.flock_file(&name, rustix::fs::FlockOperation::LockExclusive)
    }

    /// Writers hold the gate shared; the usage reconciler takes it exclusive.
    /// This stops a scan from overwriting a fresh reservation made by a write
    /// that is still in flight.
    fn write_gate(&self, tenant: TenantId) -> Result<KeyGuard, ObjectStoreError> {
        self.flock_file(
            &format!("gate-{}", tenant.simple()),
            rustix::fs::FlockOperation::LockShared,
        )
    }

    fn quota_lock(&self, tenant: TenantId) -> Result<KeyGuard, ObjectStoreError> {
        self.flock_file(
            &format!("quota-{}", tenant.simple()),
            rustix::fs::FlockOperation::LockExclusive,
        )
    }

    fn read_usage_counter(&self, tenant: TenantId) -> Result<Option<u64>, ObjectStoreError> {
        let path = self.usage_path(tenant);
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoredUsage>(&bytes) {
                Ok(counter)
                    if counter.version == USAGE_VERSION && counter.tenant == tenant.as_uuid() =>
                {
                    Ok(Some(counter.bytes))
                }
                // A counter is a cache. An unreadable one is discarded and
                // rebuilt from the objects, never trusted.
                _ => Ok(None),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_usage_counter(&self, tenant: TenantId, bytes: u64) -> Result<(), ObjectStoreError> {
        let counter = StoredUsage {
            version: USAGE_VERSION,
            tenant: tenant.as_uuid(),
            bytes,
        };
        let encoded = serde_json::to_vec(&counter).map_err(|error| ObjectStoreError::Corrupt {
            key: tenant.to_string(),
            reason: format!("cannot encode the usage counter: {error}"),
        })?;
        write_atomic(&self.usage_path(tenant), &encoded)
    }

    /// The counter, or one scan to build it. The caller holds the quota lock.
    fn current_usage(&self, tenant: TenantId) -> Result<u64, ObjectStoreError> {
        if let Some(bytes) = self.read_usage_counter(tenant)? {
            return Ok(bytes);
        }
        let bytes = self.usage(tenant)?.bytes;
        self.write_usage_counter(tenant, bytes)?;
        Ok(bytes)
    }

    /// Adjust the durable counter. The caller holds the tenant's write gate.
    /// The quota lock makes the read-modify-write atomic between writers.
    fn adjust_usage(&self, tenant: TenantId, delta: i64) -> Result<(), ObjectStoreError> {
        if self.options.quota_bytes == 0 {
            return Ok(());
        }
        let _quota = self.quota_lock(tenant)?;
        let current = self.current_usage(tenant)?;
        let updated = current.saturating_add_signed(delta);
        self.write_usage_counter(tenant, updated)
    }

    /// Reserve `incoming - credit` bytes for a write. The returned guard holds
    /// the tenant's shared write gate until the write finishes, so the
    /// reconciler cannot observe a reservation that is not yet durable.
    fn reserve_quota(
        &self,
        tenant: TenantId,
        incoming: u64,
        credit: u64,
    ) -> Result<QuotaReservation<'_>, ObjectStoreError> {
        if self.options.quota_bytes == 0 {
            return Ok(QuotaReservation {
                store: self,
                tenant,
                delta: 0,
                active: false,
                committed: false,
                _gate: None,
            });
        }
        let gate = self.write_gate(tenant)?;
        let _quota = self.quota_lock(tenant)?;
        let current = self.current_usage(tenant)?;
        let used = current.saturating_sub(credit);
        let total = used.saturating_add(incoming);
        if total > self.options.quota_bytes {
            return Err(ObjectStoreError::QuotaExceeded {
                used,
                incoming,
                limit: self.options.quota_bytes,
            });
        }
        let updated = total;
        self.write_usage_counter(tenant, updated)?;
        Ok(QuotaReservation {
            store: self,
            tenant,
            delta: updated as i64 - current as i64,
            active: true,
            committed: false,
            _gate: Some(gate),
        })
    }

    fn check_object_size(&self, key: &ObjectKey, bytes: u64) -> Result<(), ObjectStoreError> {
        if bytes > self.options.max_object_bytes {
            return Err(ObjectStoreError::TooLarge {
                key: key.to_string(),
                bytes,
                limit: self.options.max_object_bytes,
            });
        }
        Ok(())
    }

    fn check_part_size(&self, key: &ObjectKey, bytes: u64) -> Result<(), ObjectStoreError> {
        if bytes > self.options.max_part_bytes {
            return Err(ObjectStoreError::TooLarge {
                key: key.to_string(),
                bytes,
                limit: self.options.max_part_bytes,
            });
        }
        Ok(())
    }

    /// Refuse a key that collides with an existing key as a path prefix
    /// (`a` and `a/b`), which the filesystem would otherwise report as a raw
    /// `Io(AlreadyExists)`.
    fn check_key_conflict(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<(), ObjectStoreError> {
        let dir = self.data_dir(tenant);
        let path = self.data_path(tenant, key);
        if path.is_dir() {
            return Err(ObjectStoreError::Conflict(format!(
                "key {key} is a prefix of an existing object"
            )));
        }
        let mut parent = path.parent();
        while let Some(current) = parent {
            if current == dir {
                break;
            }
            if current.is_file() {
                return Err(ObjectStoreError::Conflict(format!(
                    "key {key} is below an existing object at {}",
                    current.strip_prefix(&dir).unwrap_or(current).display()
                )));
            }
            parent = current.parent();
        }
        Ok(())
    }

    fn read_meta(&self, tenant: TenantId, key: &ObjectKey) -> Result<StoredMeta, ObjectStoreError> {
        self.read_meta_optional(tenant, key)?
            .ok_or_else(|| ObjectStoreError::NotFound {
                tenant,
                key: key.clone(),
            })
    }

    fn read_meta_optional(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<Option<StoredMeta>, ObjectStoreError> {
        let path = self.meta_path(tenant, key);
        match fs::read(&path) {
            Ok(bytes) => {
                let meta: StoredMeta =
                    serde_json::from_slice(&bytes).map_err(|error| ObjectStoreError::Corrupt {
                        key: key.to_string(),
                        reason: format!("metadata is not valid JSON: {error}"),
                    })?;
                if meta.version != META_VERSION
                    || meta.key != key.as_str()
                    || meta.tenant != tenant.as_uuid()
                {
                    return Err(ObjectStoreError::Corrupt {
                        key: key.to_string(),
                        reason: "metadata does not describe this tenant, key and format".into(),
                    });
                }
                Ok(Some(meta))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_meta(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        meta: &StoredMeta,
    ) -> Result<(), ObjectStoreError> {
        let bytes = serde_json::to_vec(meta).map_err(|error| ObjectStoreError::Corrupt {
            key: key.to_string(),
            reason: format!("cannot encode metadata: {error}"),
        })?;
        write_atomic(&self.meta_path(tenant, key), &bytes)
    }

    fn public_meta(tenant: TenantId, key: &ObjectKey, meta: &StoredMeta) -> ObjectMeta {
        ObjectMeta {
            tenant,
            key: key.clone(),
            bytes: meta.bytes,
            sha256: meta.sha256.clone(),
            appendable: meta.appendable,
            updated_unix_seconds: meta.updated_unix_seconds,
        }
    }

    fn put_internal(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
        appendable: bool,
    ) -> Result<ObjectMeta, ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        self.put_locked(tenant, key, data, appendable)
    }

    /// The write path with the tenant's write gate and the key lock held.
    fn put_locked(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
        appendable: bool,
    ) -> Result<ObjectMeta, ObjectStoreError> {
        self.check_object_size(key, data.len() as u64)?;
        // A put is an explicit overwrite, so it adopts a key whose sidecar is
        // unreadable instead of becoming impossible to repair.
        let existing = match self.read_meta_optional(tenant, key) {
            Ok(existing) => existing,
            Err(ObjectStoreError::Corrupt { .. }) if !appendable => None,
            Err(error) => return Err(error),
        };
        if let Some(meta) = &existing {
            if !meta.appendable && appendable {
                return Err(ObjectStoreError::Conflict(format!(
                    "key {key} already holds an immutable object"
                )));
            }
            if meta.appendable && !appendable {
                return Err(ObjectStoreError::Conflict(format!(
                    "key {key} is a live log; put would replace it with an immutable object"
                )));
            }
        }
        self.check_key_conflict(tenant, key)?;
        let credit = existing.as_ref().map_or(0, |meta| meta.bytes);
        let reservation = self.reserve_quota(tenant, data.len() as u64, credit)?;
        let (sha256, chunks) = digest_bytes(data);
        let now = now_unix_seconds();
        let created = existing
            .as_ref()
            .map_or(now, |meta| meta.created_unix_seconds);
        let data_path = self.data_path(tenant, key);
        write_atomic(&data_path, data)?;
        let meta = StoredMeta {
            version: META_VERSION,
            tenant: tenant.as_uuid(),
            key: key.as_str().to_owned(),
            bytes: data.len() as u64,
            sha256,
            appendable,
            chunks,
            created_unix_seconds: created,
            updated_unix_seconds: now,
        };
        self.write_meta(tenant, key, &meta)?;
        reservation.commit();
        Ok(Self::public_meta(tenant, key, &meta))
    }

    fn require_meta(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<StoredMeta, ObjectStoreError> {
        self.read_meta(tenant, key)
    }

    fn upload_manifest(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<Option<UploadManifest>, ObjectStoreError> {
        let path = self.upload_dir(tenant, key, upload).join("upload.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let manifest: UploadManifest = serde_json::from_slice(&bytes).map_err(|error| {
                    ObjectStoreError::InvalidUpload(format!(
                        "upload {upload} manifest is not valid JSON: {error}"
                    ))
                })?;
                if manifest.upload != upload.0
                    || manifest.tenant != tenant.as_uuid()
                    || manifest.key != key.as_str()
                {
                    return Err(ObjectStoreError::InvalidUpload(format!(
                        "upload {upload} does not belong to this tenant and key"
                    )));
                }
                Ok(Some(manifest))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_upload_manifest(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        created: u64,
    ) -> Result<UploadManifest, ObjectStoreError> {
        let manifest = UploadManifest {
            upload: upload.0,
            tenant: tenant.as_uuid(),
            key: key.as_str().to_owned(),
            created_unix_seconds: created,
            updated_unix_seconds: now_unix_seconds(),
        };
        let bytes = serde_json::to_vec(&manifest).map_err(|error| {
            ObjectStoreError::InvalidUpload(format!("cannot encode upload manifest: {error}"))
        })?;
        write_atomic(
            &self.upload_dir(tenant, key, upload).join("upload.json"),
            &bytes,
        )?;
        Ok(manifest)
    }

    fn part_paths(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        part: u32,
    ) -> (PathBuf, PathBuf) {
        let dir = self.upload_dir(tenant, key, upload);
        (
            dir.join(format!("part-{part:05}")),
            dir.join(format!("part-{part:05}.json")),
        )
    }

    fn read_part_meta(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        part: u32,
    ) -> Result<PartMeta, ObjectStoreError> {
        let (_, meta_path) = self.part_paths(tenant, key, upload, part);
        let bytes = fs::read(&meta_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ObjectStoreError::InvalidUpload(format!("upload {upload} has no part {part}"))
            } else {
                ObjectStoreError::Io(error)
            }
        })?;
        let meta: PartManifest = serde_json::from_slice(&bytes).map_err(|error| {
            ObjectStoreError::InvalidUpload(format!("part {part} metadata is invalid: {error}"))
        })?;
        if meta.part_number != part {
            return Err(ObjectStoreError::InvalidUpload(format!(
                "part {part} metadata names part {}",
                meta.part_number
            )));
        }
        Ok(PartMeta {
            part_number: meta.part_number,
            bytes: meta.bytes,
            sha256: meta.sha256,
        })
    }

    fn clear_upload(&self, tenant: TenantId, key: &ObjectKey, upload: &UploadId) {
        let _ = fs::remove_dir_all(self.upload_dir(tenant, key, upload));
    }

    /// `list_parts` without the key lock, for callers that already hold it.
    fn list_parts_locked(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<Vec<PartMeta>, ObjectStoreError> {
        if self.upload_manifest(tenant, key, upload)?.is_none() {
            return Err(ObjectStoreError::InvalidUpload(format!(
                "upload {upload} does not exist for this tenant and key"
            )));
        }
        let dir = self.upload_dir(tenant, key, upload);
        let mut parts = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(number) = name
                .strip_prefix("part-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .and_then(|number| number.parse::<u32>().ok())
            else {
                continue;
            };
            parts.push(self.read_part_meta(tenant, key, upload, number)?);
        }
        parts.sort_by_key(|part| part.part_number);
        Ok(parts)
    }

    /// Re-read every object and every chunk, and report the two orphan states
    /// a crash between the payload and sidecar writes can leave. Corruption is
    /// `Corrupt`; crash residue is `Orphan`, which backup verification
    /// tolerates (ADR 0014).
    pub fn verify_all(&self) -> Result<Vec<ObjectProblem>, ObjectStoreError> {
        let mut problems = Vec::new();
        // Payloads first: a payload with no usable sidecar is the residue of a
        // crash after the payload write and before the sidecar rename.
        let data_root = self.root.join("data");
        let mut payloads = Vec::new();
        collect_files(&data_root, &mut payloads)?;
        for path in payloads {
            let Ok(relative) = path.strip_prefix(&data_root) else {
                continue;
            };
            let mut components = relative.components();
            let Some(tenant_component) = components.next() else {
                continue;
            };
            let Ok(tenant) = Uuid::parse_str(&tenant_component.as_os_str().to_string_lossy())
            else {
                continue;
            };
            let tenant = TenantId::new(tenant);
            let key_text = components
                .as_path()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let Ok(key) = ObjectKey::parse(&key_text) else {
                problems.push(ObjectProblem {
                    key: key_text,
                    kind: ObjectProblemKind::Corrupt,
                    message: "payload path is not a valid key".into(),
                });
                continue;
            };
            match self.read_meta_optional(tenant, &key) {
                Ok(Some(_)) => {
                    if let Err(error) = self.verify(tenant, &key) {
                        problems.push(ObjectProblem {
                            key: key_text,
                            kind: ObjectProblemKind::Corrupt,
                            message: error.to_string(),
                        });
                    }
                }
                Ok(None) => problems.push(ObjectProblem {
                    key: key_text,
                    kind: ObjectProblemKind::Orphan,
                    message: "payload exists but the metadata sidecar is missing".into(),
                }),
                Err(ObjectStoreError::Corrupt { reason, .. }) => problems.push(ObjectProblem {
                    key: key_text,
                    kind: ObjectProblemKind::Orphan,
                    message: format!(
                        "payload exists but its metadata sidecar is unreadable: {reason}"
                    ),
                }),
                Err(error) => return Err(error),
            }
        }
        // A sidecar with no payload is the residue of a delete or a restore
        // that lost bytes. It is reported, not fatal.
        let mut metas = Vec::new();
        collect_files(&self.root.join("meta"), &mut metas)?;
        for meta_path in metas {
            if meta_path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some((tenant, key)) = self.key_from_meta_path(&meta_path) else {
                problems.push(ObjectProblem {
                    key: meta_path.display().to_string(),
                    kind: ObjectProblemKind::Corrupt,
                    message: "metadata path is not a valid key".into(),
                });
                continue;
            };
            if !self.data_path(tenant, &key).is_file() {
                problems.push(ObjectProblem {
                    key: key.to_string(),
                    kind: ObjectProblemKind::Orphan,
                    message: "metadata exists but the payload is missing".into(),
                });
            }
        }
        Ok(problems)
    }
}

impl ObjectStore for FsObjectStore {
    fn put(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
    ) -> Result<ObjectMeta, ObjectStoreError> {
        self.put_internal(tenant, key, data, false)
    }

    fn create_appendable(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<ObjectMeta, ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        if let Some(meta) = self.read_meta_optional(tenant, key)? {
            if meta.appendable {
                return Ok(Self::public_meta(tenant, key, &meta));
            }
            return Err(ObjectStoreError::Conflict(format!(
                "key {key} already holds an immutable object"
            )));
        }
        self.put_locked(tenant, key, &[], true)
    }

    fn append(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        data: &[u8],
    ) -> Result<ObjectMeta, ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        let meta = self.require_meta(tenant, key)?;
        if !meta.appendable {
            return Err(ObjectStoreError::Conflict(format!(
                "key {key} is not appendable"
            )));
        }
        let bytes = meta.bytes.saturating_add(data.len() as u64);
        self.check_object_size(key, bytes)?;
        let path = self.data_path(tenant, key);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ObjectStoreError::Corrupt {
                        key: key.to_string(),
                        reason: "payload is missing".into(),
                    }
                } else {
                    ObjectStoreError::Io(error)
                }
            })?;
        // Verify the committed base before extending it. Without this an
        // append would digest whatever is on disk and bless corrupt bytes as
        // the new object (ADR 0014, "Append format").
        let length = file.metadata()?.len();
        let committed = committed_length(&meta, length, key)?;
        let actual = digest_prefix(&path, committed)?;
        if actual != meta.sha256 {
            return Err(ObjectStoreError::ChecksumMismatch {
                key: key.to_string(),
                expected: meta.sha256.clone(),
                actual,
            });
        }
        // Charge the append before any byte is written, so an over-quota
        // append leaves neither a tail nor a counter change.
        let reservation = self.reserve_quota(tenant, data.len() as u64, 0)?;
        // A crash between the data fsync and the metadata rename can leave a
        // torn tail. It was never committed, so truncate it before appending.
        file.set_len(meta.bytes)?;
        file.seek(SeekFrom::End(0))?;
        if !data.is_empty() {
            file.write_all(data)?;
        }
        file.sync_all()?;
        drop(file);
        let (sha256, chunks) = digest_file(&path)?;
        let updated = StoredMeta {
            version: META_VERSION,
            tenant: tenant.as_uuid(),
            key: key.as_str().to_owned(),
            bytes,
            sha256,
            appendable: true,
            chunks,
            created_unix_seconds: meta.created_unix_seconds,
            updated_unix_seconds: now_unix_seconds(),
        };
        self.write_meta(tenant, key, &updated)?;
        reservation.commit();
        Ok(Self::public_meta(tenant, key, &updated))
    }

    fn get(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Vec<u8>, ObjectStoreError> {
        let meta = self.require_meta(tenant, key)?;
        let path = self.data_path(tenant, key);
        let length = fs::metadata(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ObjectStoreError::Corrupt {
                        key: key.to_string(),
                        reason: "payload is missing".into(),
                    }
                } else {
                    ObjectStoreError::Io(error)
                }
            })?
            .len();
        // A live log may have an uncommitted torn tail. Reads see the
        // committed prefix only; the tail was never acknowledged (ADR 0014).
        let committed = committed_length(&meta, length, key)?;
        let Some(range) = range else {
            let mut data = Vec::with_capacity(committed as usize);
            fs::File::open(&path)?
                .take(committed)
                .read_to_end(&mut data)?;
            if data.len() as u64 != committed {
                return Err(ObjectStoreError::Corrupt {
                    key: key.to_string(),
                    reason: format!("payload is shorter than the committed length {}", committed),
                });
            }
            let actual = sha256_hex(&data);
            if actual != meta.sha256 {
                return Err(ObjectStoreError::ChecksumMismatch {
                    key: key.to_string(),
                    expected: meta.sha256,
                    actual,
                });
            }
            return Ok(data);
        };
        if range.end >= committed {
            return Err(ObjectStoreError::RangeNotSatisfiable {
                start: range.start,
                end: range.end,
                len: committed,
            });
        }
        // Verify only the chunks the range touches. The chunk table is the
        // reason a ranged read does not have to stream the whole object.
        if meta.chunks.len() == chunk_count(committed) {
            let first = (range.start / CHUNK_BYTES as u64) as usize;
            let last = (range.end / CHUNK_BYTES as u64) as usize;
            let mut file = fs::File::open(&path)?;
            let mut out = Vec::with_capacity((range.end - range.start + 1) as usize);
            let mut offset = first as u64 * CHUNK_BYTES as u64;
            for (index, chunk) in meta
                .chunks
                .iter()
                .enumerate()
                .skip(first)
                .take(last - first + 1)
            {
                let chunk_len = chunk.bytes as usize;
                let mut buffer = vec![0u8; chunk_len];
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut buffer)?;
                let actual = sha256_hex(&buffer);
                if actual != chunk.sha256 || chunk_len as u64 != chunk.bytes {
                    return Err(ObjectStoreError::ChecksumMismatch {
                        key: format!("{key} (chunk {index})"),
                        expected: chunk.sha256.clone(),
                        actual,
                    });
                }
                let chunk_start = offset;
                let chunk_end = offset + chunk.bytes - 1;
                let from = range.start.max(chunk_start) - chunk_start;
                let to = range.end.min(chunk_end) - chunk_start;
                out.extend_from_slice(&buffer[from as usize..=to as usize]);
                offset += chunk.bytes;
            }
            return Ok(out);
        }
        // A metadata file from before the chunk table: verify the whole object
        // once, then slice.
        self.verify(tenant, key)?;
        let mut file = fs::File::open(&path)?;
        file.seek(SeekFrom::Start(range.start))?;
        let mut out = vec![0u8; (range.end - range.start + 1) as usize];
        file.read_exact(&mut out)?;
        Ok(out)
    }

    fn stat(&self, tenant: TenantId, key: &ObjectKey) -> Result<ObjectMeta, ObjectStoreError> {
        let meta = self.require_meta(tenant, key)?;
        Ok(Self::public_meta(tenant, key, &meta))
    }

    fn checksum(&self, tenant: TenantId, key: &ObjectKey) -> Result<String, ObjectStoreError> {
        Ok(self.require_meta(tenant, key)?.sha256)
    }

    fn verify(&self, tenant: TenantId, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let meta = self.require_meta(tenant, key)?;
        let path = self.data_path(tenant, key);
        let length = fs::metadata(&path)?.len();
        let committed = committed_length(&meta, length, key)?;
        let actual = digest_prefix(&path, committed)?;
        if actual != meta.sha256 {
            return Err(ObjectStoreError::ChecksumMismatch {
                key: key.to_string(),
                expected: meta.sha256,
                actual,
            });
        }
        Ok(())
    }

    fn delete(&self, tenant: TenantId, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        let existing = self.read_meta_optional(tenant, key)?;
        let data = self.data_path(tenant, key);
        let meta = self.meta_path(tenant, key);
        // Sidecar first: a crash between the two leaves an orphan payload,
        // which listing, inventory and backup tolerate, rather than an object
        // that `stat` reports and `get` refuses.
        remove_file_if_present(&meta)?;
        remove_file_if_present(&data)?;
        sync_parent(&data)?;
        sync_parent(&meta)?;
        if let Some(meta) = existing {
            self.adjust_usage(tenant, -(meta.bytes as i64))?;
        }
        Ok(())
    }

    fn list(&self, tenant: TenantId, prefix: &str) -> Result<Vec<ListEntry>, ObjectStoreError> {
        validate_prefix(prefix)?;
        let root = self.data_dir(tenant);
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        collect_files(&root, &mut files)?;
        let mut entries = Vec::new();
        for path in files {
            let relative = match path.strip_prefix(&root) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            let key_text = relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !key_text.starts_with(prefix) {
                continue;
            }
            let key = ObjectKey::parse(&key_text)?;
            // A payload without a usable sidecar is crash residue. Skip it;
            // one orphan must not make the whole tenant unlistable (ADR 0014).
            match self.read_meta_optional(tenant, &key) {
                Ok(Some(meta)) => entries.push(ListEntry {
                    key,
                    bytes: meta.bytes,
                    sha256: meta.sha256,
                    updated_unix_seconds: meta.updated_unix_seconds,
                }),
                Ok(None) | Err(ObjectStoreError::Corrupt { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(entries)
    }

    fn create_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
    ) -> Result<UploadId, ObjectStoreError> {
        let upload = UploadId::new();
        let dir = self.upload_dir(tenant, key, &upload);
        ensure_dir(&dir)?;
        self.write_upload_manifest(tenant, key, &upload, now_unix_seconds())?;
        Ok(upload)
    }

    fn upload_part(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        part_number: u32,
        data: &[u8],
    ) -> Result<PartMeta, ObjectStoreError> {
        if part_number == 0 || part_number > MAX_PART_NUMBER {
            return Err(ObjectStoreError::InvalidUpload(format!(
                "part number {part_number} is outside 1..={MAX_PART_NUMBER}"
            )));
        }
        self.check_part_size(key, data.len() as u64)?;
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        let manifest = self.upload_manifest(tenant, key, upload)?.ok_or_else(|| {
            ObjectStoreError::InvalidUpload(format!(
                "upload {upload} does not exist for this tenant and key"
            ))
        })?;
        let credit = self
            .read_part_meta(tenant, key, upload, part_number)
            .map(|meta| meta.bytes)
            .unwrap_or(0);
        let reservation = self.reserve_quota(tenant, data.len() as u64, credit)?;
        let (data_path, meta_path) = self.part_paths(tenant, key, upload, part_number);
        write_atomic(&data_path, data)?;
        let meta = PartManifest {
            part_number,
            bytes: data.len() as u64,
            sha256: sha256_hex(data),
        };
        let bytes = serde_json::to_vec(&meta).map_err(|error| {
            ObjectStoreError::InvalidUpload(format!("cannot encode part metadata: {error}"))
        })?;
        write_atomic(&meta_path, &bytes)?;
        self.write_upload_manifest(tenant, key, upload, manifest.created_unix_seconds)?;
        reservation.commit();
        Ok(PartMeta {
            part_number,
            bytes: meta.bytes,
            sha256: meta.sha256,
        })
    }

    fn list_parts(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<Vec<PartMeta>, ObjectStoreError> {
        // Take the key lock so a concurrent completion or sweep cannot remove
        // the upload between the manifest check and the directory walk.
        let _guard = self.lock_key(tenant, key)?;
        self.list_parts_locked(tenant, key, upload)
    }

    fn complete_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
        parts: Option<&[u32]>,
    ) -> Result<ObjectMeta, ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        if self.upload_manifest(tenant, key, upload)?.is_none() {
            return Err(ObjectStoreError::InvalidUpload(format!(
                "upload {upload} does not exist for this tenant and key"
            )));
        }
        let all_parts = self.list_parts_locked(tenant, key, upload)?;
        let all_bytes = all_parts
            .iter()
            .fold(0u64, |total, part| total.saturating_add(part.bytes));
        let numbers: Vec<u32> = match parts {
            Some(list) => {
                let mut previous = None;
                for number in list {
                    if *number == 0 || *number > MAX_PART_NUMBER {
                        return Err(ObjectStoreError::InvalidUpload(format!(
                            "part number {number} is outside 1..={MAX_PART_NUMBER}"
                        )));
                    }
                    if previous.is_some_and(|previous| *number <= previous) {
                        return Err(ObjectStoreError::InvalidUpload(
                            "part numbers must be strictly increasing and unique".into(),
                        ));
                    }
                    previous = Some(*number);
                }
                list.to_vec()
            }
            None => all_parts.iter().map(|part| part.part_number).collect(),
        };
        if numbers.is_empty() {
            return Err(ObjectStoreError::InvalidUpload(
                "an upload needs at least one part".into(),
            ));
        }
        let mut metas = Vec::with_capacity(numbers.len());
        let mut total = 0u64;
        for number in &numbers {
            let meta = all_parts
                .iter()
                .find(|part| part.part_number == *number)
                .cloned()
                .ok_or_else(|| {
                    ObjectStoreError::InvalidUpload(format!("upload {upload} has no part {number}"))
                })?;
            self.check_part_size(key, meta.bytes)?;
            total = total.saturating_add(meta.bytes);
            metas.push(meta);
        }
        self.check_object_size(key, total)?;
        let existing = self.read_meta_optional(tenant, key)?;
        if let Some(meta) = &existing
            && meta.appendable
        {
            return Err(ObjectStoreError::Conflict(format!(
                "key {key} is a live log; completing an upload would replace it"
            )));
        }
        self.check_key_conflict(tenant, key)?;
        // Completing an upload releases every part (the selected ones become
        // the object, the rest are discarded), so the credit is the existing
        // object plus every part on disk.
        let credit = existing
            .as_ref()
            .map_or(0, |meta| meta.bytes)
            .saturating_add(all_bytes);
        let reservation = self.reserve_quota(tenant, total, credit)?;

        let temporary = self
            .root
            .join("tmp")
            .join(Uuid::new_v4().simple().to_string());
        let result = (|| -> Result<(String, Vec<StoredChunk>), ObjectStoreError> {
            let mut writer = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            // Stream each part through a 1 MiB buffer, hashing as it goes.
            // A part is never allocated whole (ADR 0014, "Streaming").
            let mut buffer = vec![0u8; CHUNK_BYTES];
            for meta in &metas {
                let (data_path, _) = self.part_paths(tenant, key, upload, meta.part_number);
                let mut reader = fs::File::open(&data_path)?;
                let mut hasher = Sha256::new();
                let mut copied = 0u64;
                loop {
                    let read = reader.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                    writer.write_all(&buffer[..read])?;
                    copied = copied.saturating_add(read as u64);
                }
                if copied != meta.bytes {
                    return Err(ObjectStoreError::Corrupt {
                        key: format!("{key} (upload {upload} part {})", meta.part_number),
                        reason: format!(
                            "part is {copied} bytes but its metadata records {}",
                            meta.bytes
                        ),
                    });
                }
                let actual = hex::encode(hasher.finalize());
                if actual != meta.sha256 {
                    return Err(ObjectStoreError::ChecksumMismatch {
                        key: format!("{key} (upload {upload} part {})", meta.part_number),
                        expected: meta.sha256.clone(),
                        actual,
                    });
                }
            }
            writer.sync_all()?;
            drop(writer);
            digest_file(&temporary)
        })();
        let (sha256, chunks) = match result {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        let data_path = self.data_path(tenant, key);
        ensure_parent(&data_path)?;
        if let Err(error) = fs::rename(&temporary, &data_path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        sync_parent(&data_path)?;
        let now = now_unix_seconds();
        let created = existing
            .as_ref()
            .map_or(now, |meta| meta.created_unix_seconds);
        let meta = StoredMeta {
            version: META_VERSION,
            tenant: tenant.as_uuid(),
            key: key.as_str().to_owned(),
            bytes: total,
            sha256,
            appendable: false,
            chunks,
            created_unix_seconds: created,
            updated_unix_seconds: now,
        };
        self.write_meta(tenant, key, &meta)?;
        self.clear_upload(tenant, key, upload);
        reservation.commit();
        Ok(Self::public_meta(tenant, key, &meta))
    }

    fn abort_multipart(
        &self,
        tenant: TenantId,
        key: &ObjectKey,
        upload: &UploadId,
    ) -> Result<(), ObjectStoreError> {
        let _gate = self.write_gate(tenant)?;
        let _guard = self.lock_key(tenant, key)?;
        let dir = self.upload_dir(tenant, key, upload);
        let bytes = upload_part_bytes(&dir)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        self.adjust_usage(tenant, -(bytes as i64))?;
        Ok(())
    }

    fn usage(&self, tenant: TenantId) -> Result<Usage, ObjectStoreError> {
        let mut usage = Usage::default();
        let meta_root = self.meta_dir(tenant);
        if meta_root.is_dir() {
            let mut files = Vec::new();
            collect_files(&meta_root, &mut files)?;
            for path in files {
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Some((meta_tenant, key)) = self.key_from_meta_path(&path) else {
                    continue;
                };
                if meta_tenant != tenant {
                    continue;
                }
                // A sidecar whose payload is gone is a delete/crash remnant,
                // not a stored byte. Counting it would charge the tenant for
                // nothing for the life of the installation. An unreadable
                // sidecar is likewise not chargeable; `verify_all` reports it.
                if !self.data_path(tenant, &key).is_file() {
                    continue;
                }
                let meta = match self.read_meta(tenant, &key) {
                    Ok(meta) => meta,
                    Err(ObjectStoreError::Corrupt { .. }) => continue,
                    Err(error) => return Err(error),
                };
                usage.objects += 1;
                usage.bytes = usage.bytes.saturating_add(meta.bytes);
            }
        }

        let upload_root = self.tenant_root("uploads", tenant);
        if upload_root.is_dir() {
            let mut manifests = Vec::new();
            collect_named(&upload_root, "upload.json", &mut manifests)?;
            usage.uploads = manifests.len() as u64;
            for manifest in manifests {
                let Some(dir) = manifest.parent() else {
                    continue;
                };
                usage.upload_bytes = usage.upload_bytes.saturating_add(upload_part_bytes(dir)?);
            }
        }
        usage.bytes = usage.bytes.saturating_add(usage.upload_bytes);
        Ok(usage)
    }

    fn inventory(&self) -> Result<Vec<ObjectRecord>, ObjectStoreError> {
        let mut records = Vec::new();
        let data_root = self.root.join("data");
        if !data_root.is_dir() {
            return Ok(records);
        }
        for entry in fs::read_dir(&data_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let tenant_text = entry.file_name();
            let Ok(tenant) = Uuid::parse_str(&tenant_text.to_string_lossy()) else {
                continue;
            };
            let tenant = TenantId::new(tenant);
            let mut files = Vec::new();
            collect_files(&entry.path(), &mut files)?;
            for path in files {
                let relative = match path.strip_prefix(entry.path()) {
                    Ok(relative) => relative,
                    Err(_) => continue,
                };
                let key_text = relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let key = ObjectKey::parse(&key_text)?;
                // Skip crash residue (payload with no usable sidecar). The
                // backup tar still carries the bytes; the manifest lists only
                // objects it can verify.
                match self.read_meta_optional(tenant, &key) {
                    Ok(Some(meta)) => records.push(ObjectRecord {
                        tenant: tenant.as_uuid(),
                        key: key_text,
                        bytes: meta.bytes,
                        sha256: meta.sha256,
                    }),
                    Ok(None) | Err(ObjectStoreError::Corrupt { .. }) => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        records.sort_by(|left, right| {
            (left.tenant, left.key.as_str()).cmp(&(right.tenant, right.key.as_str()))
        });
        Ok(records)
    }

    fn reconcile_usage(&self) -> Result<(), ObjectStoreError> {
        if self.options.quota_bytes == 0 {
            return Ok(());
        }
        for tenant in self.known_tenants()? {
            let _gate = self.flock_file(
                &format!("gate-{}", tenant.simple()),
                rustix::fs::FlockOperation::LockExclusive,
            )?;
            let _quota = self.quota_lock(tenant)?;
            let bytes = self.usage(tenant)?.bytes;
            self.write_usage_counter(tenant, bytes)?;
        }
        Ok(())
    }

    fn cleanup_abandoned(
        &self,
        now_unix_seconds: u64,
    ) -> Result<Vec<AbandonedUpload>, ObjectStoreError> {
        let mut cleaned = Vec::new();
        let upload_root = self.root.join("uploads");
        if !upload_root.is_dir() {
            return Ok(cleaned);
        }
        let mut manifests = Vec::new();
        collect_named(&upload_root, "upload.json", &mut manifests)?;
        for path in manifests {
            let Some(dir) = path.parent() else {
                continue;
            };
            let manifest: UploadManifest = match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(manifest) => manifest,
                None => {
                    // A half-written upload has no readable activity time;
                    // the directory's own mtime is the fallback.
                    let updated = fs::metadata(dir)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs())
                        .unwrap_or(0);
                    if !self.expired(now_unix_seconds, updated) {
                        continue;
                    }
                    let tenant = dir
                        .strip_prefix(&upload_root)
                        .ok()
                        .and_then(|relative| relative.components().next())
                        .and_then(|component| {
                            Uuid::parse_str(&component.as_os_str().to_string_lossy()).ok()
                        })
                        .map(TenantId::new);
                    let Some(tenant) = tenant else {
                        let _ = fs::remove_dir_all(dir);
                        continue;
                    };
                    let _gate = self.write_gate(tenant)?;
                    let bytes = upload_part_bytes(dir)?;
                    fs::remove_dir_all(dir)?;
                    self.adjust_usage(tenant, -(bytes as i64))?;
                    continue;
                }
            };
            if !self.expired(now_unix_seconds, manifest.updated_unix_seconds) {
                continue;
            }
            let Ok(key) = ObjectKey::parse(&manifest.key) else {
                let _ = fs::remove_dir_all(dir);
                continue;
            };
            let tenant = TenantId::new(manifest.tenant);
            // Take the tenant gate and the key lock the writers use, then
            // re-read the activity time: a part or a completion that raced
            // the sweep must win.
            let _gate = self.write_gate(tenant)?;
            let _guard = self.lock_key(tenant, &key)?;
            let current: UploadManifest = match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(manifest) => manifest,
                None => continue,
            };
            if !self.expired(now_unix_seconds, current.updated_unix_seconds) {
                continue;
            }
            let bytes = upload_part_bytes(dir)?;
            fs::remove_dir_all(dir)?;
            self.adjust_usage(tenant, -(bytes as i64))?;
            cleaned.push(AbandonedUpload {
                tenant,
                key,
                upload: UploadId(current.upload),
                updated_unix_seconds: current.updated_unix_seconds,
            });
        }
        Ok(cleaned)
    }
}

impl FsObjectStore {
    /// Every tenant that has data, an upload or a usage counter.
    fn known_tenants(&self) -> Result<Vec<TenantId>, ObjectStoreError> {
        let mut tenants = HashSet::new();
        for tree in ["data", "uploads", "usage"] {
            let root = self.root.join(tree);
            if !root.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if let Ok(tenant) = Uuid::parse_str(&entry.file_name().to_string_lossy()) {
                    tenants.insert(tenant);
                }
            }
        }
        let mut tenants: Vec<TenantId> = tenants.into_iter().map(TenantId::new).collect();
        tenants.sort();
        Ok(tenants)
    }

    fn key_from_meta_path(&self, path: &Path) -> Option<(TenantId, ObjectKey)> {
        let relative = path.strip_prefix(self.root.join("meta")).ok()?;
        let mut components = relative.components();
        let tenant = Uuid::parse_str(&components.next()?.as_os_str().to_string_lossy()).ok()?;
        let name = components.as_path().to_string_lossy();
        let key_name = name.strip_suffix(".json")?.replace('\\', "/");
        let key = ObjectKey::parse(&key_name).ok()?;
        Some((TenantId::new(tenant), key))
    }

    fn expired(&self, now_unix_seconds: u64, updated_unix_seconds: u64) -> bool {
        let deadline = self.options.upload_deadline.as_secs();
        now_unix_seconds
            .checked_sub(updated_unix_seconds)
            .is_some_and(|age| age > deadline)
    }
}

// ---------------------------------------------------------------------------
// Stored formats
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredChunk {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredMeta {
    version: u32,
    tenant: Uuid,
    key: String,
    bytes: u64,
    sha256: String,
    appendable: bool,
    chunks: Vec<StoredChunk>,
    created_unix_seconds: u64,
    updated_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UploadManifest {
    upload: Uuid,
    tenant: Uuid,
    key: String,
    created_unix_seconds: u64,
    updated_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PartManifest {
    part_number: u32,
    bytes: u64,
    sha256: String,
}

/// The durable per-tenant byte counter. It is a cache over `data/` and
/// `uploads/`: a stale or unreadable counter is rebuilt, never trusted.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredUsage {
    version: u32,
    tenant: Uuid,
    bytes: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn chunk_count(length: u64) -> usize {
    if length == 0 {
        0
    } else {
        length.div_ceil(CHUNK_BYTES as u64) as usize
    }
}

/// The length a read may trust: the committed length for a live log, whose
/// payload may carry an unacknowledged torn tail, and the exact length for an
/// immutable object, where any difference is damage.
fn committed_length(
    meta: &StoredMeta,
    length: u64,
    key: &ObjectKey,
) -> Result<u64, ObjectStoreError> {
    if length < meta.bytes {
        return Err(ObjectStoreError::Corrupt {
            key: key.to_string(),
            reason: format!(
                "payload is {length} bytes but the committed length is {}",
                meta.bytes
            ),
        });
    }
    if length > meta.bytes && !meta.appendable {
        return Err(ObjectStoreError::Corrupt {
            key: key.to_string(),
            reason: format!(
                "payload is {length} bytes but the recorded length is {}",
                meta.bytes
            ),
        });
    }
    Ok(meta.bytes)
}

/// The whole-object SHA-256 of the first `bytes` bytes.
fn digest_prefix(path: &Path, bytes: u64) -> Result<String, ObjectStoreError> {
    let mut file = fs::File::open(path)?;
    let mut whole = Sha256::new();
    let mut remaining = bytes;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        whole.update(&buffer[..read]);
        remaining -= read as u64;
    }
    if remaining != 0 {
        return Err(ObjectStoreError::Corrupt {
            key: path.display().to_string(),
            reason: format!("payload ended {remaining} bytes before its committed length"),
        });
    }
    Ok(hex::encode(whole.finalize()))
}

/// The payload bytes of every part in an upload directory (not the sidecars).
fn upload_part_bytes(dir: &Path) -> Result<u64, ObjectStoreError> {
    let mut total = 0u64;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("part-") && !name.ends_with(".json") && entry.path().is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

fn digest_bytes(data: &[u8]) -> (String, Vec<StoredChunk>) {
    let sha256 = sha256_hex(data);
    let mut chunks = Vec::with_capacity(chunk_count(data.len() as u64));
    for chunk in data.chunks(CHUNK_BYTES) {
        chunks.push(StoredChunk {
            bytes: chunk.len() as u64,
            sha256: sha256_hex(chunk),
        });
    }
    (sha256, chunks)
}

fn digest_file(path: &Path) -> Result<(String, Vec<StoredChunk>), ObjectStoreError> {
    let mut file = fs::File::open(path)?;
    let mut whole = Sha256::new();
    let mut chunks = Vec::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            match file.read(&mut buffer[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        if filled == 0 {
            break;
        }
        let chunk = &buffer[..filled];
        whole.update(chunk);
        chunks.push(StoredChunk {
            bytes: filled as u64,
            sha256: sha256_hex(chunk),
        });
        if filled < buffer.len() {
            break;
        }
    }
    Ok((hex::encode(whole.finalize()), chunks))
}

fn validate_prefix(prefix: &str) -> Result<(), ObjectStoreError> {
    if prefix.len() > MAX_KEY_BYTES {
        return Err(ObjectStoreError::InvalidKey(
            "prefix is longer than a key".into(),
        ));
    }
    if prefix.is_empty() {
        return Ok(());
    }
    let segments: Vec<&str> = prefix.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        if segment.is_empty() {
            if last {
                // "ci/logs/" is a directory prefix.
                return Ok(());
            }
            return Err(ObjectStoreError::InvalidKey("empty prefix segment".into()));
        }
        if *segment == "." || *segment == ".." {
            return Err(ObjectStoreError::InvalidKey(
                "prefix escapes the tenant".into(),
            ));
        }
        if !segment.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'=')
        }) {
            return Err(ObjectStoreError::InvalidKey(
                "prefix contains a character outside [A-Za-z0-9._+=-]".into(),
            ));
        }
    }
    Ok(())
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => ensure_dir(parent),
        None => Ok(()),
    }
}

/// Write `bytes` to `path` through a private temporary file, fsync it, then
/// rename it into place and fsync the parent directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ObjectStoreError> {
    ensure_parent(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| ObjectStoreError::Io(io::Error::other("path has no parent")))?;
    let temporary = parent.join(format!(".tmp-{}", Uuid::new_v4().simple()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    sync_parent(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), ObjectStoreError> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Every regular file below `root`, not following symlinks.
fn collect_files(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files(&entry.path(), out)?;
        } else if metadata.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Every file named `name` below `root`, not following symlinks.
fn collect_named(root: &Path, name: &str, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_named(&entry.path(), name, out)?;
        } else if metadata.is_file() && entry.file_name().to_string_lossy() == name {
            out.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(path: &str) -> PathBuf {
        std::env::temp_dir().join(format!("githearth-objects-{}-{path}", Uuid::new_v4()))
    }

    #[test]
    fn keys_reject_traversal_and_prefixes_stay_inside() {
        for key in ["", "/a", "a/", "a//b", "a/../b", "a/./b", "a b", "a\\b"] {
            assert!(ObjectKey::parse(key).is_err(), "{key:?} must be refused");
        }
        for key in ["a", "ci/logs/run-1.log", "lfs/aa/bb/oid", "a.b_c-1+2=3"] {
            assert!(ObjectKey::parse(key).is_ok(), "{key:?} must be accepted");
        }
        assert!(validate_prefix("ci/logs/").is_ok());
        assert!(validate_prefix("../x").is_err());
    }

    #[test]
    fn digest_chunks_boundaries() {
        let data = vec![7u8; CHUNK_BYTES + 3];
        let (_, chunks) = digest_bytes(&data);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].bytes, CHUNK_BYTES as u64);
        assert_eq!(chunks[1].bytes, 3);
        assert!(digest_bytes(&[]).1.is_empty());
    }

    #[test]
    fn store_open_is_idempotent_and_visible_on_disk() {
        let root = temp("open");
        let store = FsObjectStore::open(&root).unwrap();
        drop(store);
        let store = FsObjectStore::open(&root).unwrap();
        let tenant = TenantId::new(Uuid::new_v4());
        let key = ObjectKey::parse("ci/hello.txt").unwrap();
        store.put(tenant, &key, b"hello").unwrap();
        assert!(store.data_path(tenant, &key).is_file());
        let _ = fs::remove_dir_all(root);
    }
}
