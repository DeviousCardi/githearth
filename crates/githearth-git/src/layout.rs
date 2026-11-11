//! Durable repository storage layout and lifecycle (ADR 0003 "Storage layout").

use crate::{StorageError, checked, git, journal};
use githearth_core::{GitRef, ObjectId, RepositoryId, RepositoryReader};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn arg(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }
}

pub fn detect_object_format(repo: &Path) -> Result<ObjectFormat, StorageError> {
    let output = checked(
        git()
            .arg("--git-dir")
            .arg(repo)
            .args(["rev-parse", "--show-object-format"])
            .output()?,
    )?;
    match String::from_utf8_lossy(&output).trim() {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        other => Err(StorageError::Invalid(format!(
            "unrecognized object format: {other}"
        ))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
}

pub fn parse_version(text: &str) -> Result<GitVersion, StorageError> {
    let rest = text
        .trim()
        .strip_prefix("git version ")
        .ok_or_else(|| StorageError::Invalid("unrecognized git --version output".into()))?;
    let mut parts = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    let major = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| StorageError::Invalid("unrecognized git --version output".into()))?;
    let minor = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| StorageError::Invalid("unrecognized git --version output".into()))?;
    if major < 2 || (major == 2 && minor < 38) {
        return Err(StorageError::Invalid(format!(
            "git {major}.{minor} is older than 2.38"
        )));
    }
    Ok(GitVersion { major, minor })
}

pub fn check_git_version() -> Result<GitVersion, StorageError> {
    let output = checked(git().arg("--version").output()?)?;
    parse_version(&String::from_utf8_lossy(&output))
}

#[derive(Clone, Debug)]
pub struct StorageOptions {
    pub hooks_path: PathBuf,
    pub max_input_size: usize,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            hooks_path: PathBuf::new(),
            max_input_size: 0,
        }
    }
}

/// A guard that holds the per-repository filesystem lock.
///
/// Callers acquire it with
/// [`tokio::sync::Mutex::lock_owned`](tokio::sync::Mutex::lock_owned)`.await`
/// (async) or
/// [`tokio::sync::Mutex::blocking_lock_owned`](tokio::sync::Mutex::blocking_lock_owned)
/// (sync) and **must** move the guard into the `spawn_blocking` closure that
/// performs the directory operation.  Moving the guard into the blocking
/// worker prevents a cancelled request from dropping the lock while the
/// directory operator is still running.
///
/// **Shortcut:** this lock is single-process only.  If the server ever runs
/// as multiple processes on one node it must be replaced with an
/// inter-process lock.
pub type RepositoryGuard = tokio::sync::OwnedMutexGuard<()>;

/// Durable repository storage.
///
/// All public methods on this type are lock-free — they do **not** acquire
/// the per-repository filesystem lock internally.  Callers are responsible
/// for acquiring the lock via [`Storage::repository_lock`] before calling
/// [`create`](Storage::create), [`trash`](Storage::trash), or
/// [`orphan`](Storage::orphan), and for moving the resulting
/// [`RepositoryGuard`] into the `spawn_blocking` closure that performs the
/// directory operation.
pub struct Storage {
    root: PathBuf,
    options: StorageOptions,
    locks: Mutex<HashMap<Uuid, Weak<tokio::sync::Mutex<()>>>>,
}

fn simple(id: Uuid) -> String {
    id.simple().to_string()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn fsync_tree(root: &Path) -> io::Result<()> {
    fn collect(dir: &Path, dirs: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                collect(&entry.path(), dirs)?;
            }
        }
        dirs.push(dir.to_path_buf());
        Ok(())
    }
    let mut dirs = Vec::new();
    collect(root, &mut dirs)?;
    for dir in dirs {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn fsync_tree(_: &Path) -> io::Result<()> {
    Ok(())
}

fn mkdir0700(path: &Path) -> Result<(), StorageError> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

impl Storage {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with(root, StorageOptions::default())
    }

    pub fn open_with(
        root: impl AsRef<Path>,
        options: StorageOptions,
    ) -> Result<Self, StorageError> {
        fs::create_dir_all(root.as_ref())?;
        if fs::symlink_metadata(root.as_ref())?
            .file_type()
            .is_symlink()
        {
            return Err(StorageError::Invalid(
                "storage root must not be a symlink".into(),
            ));
        }
        let root = fs::canonicalize(root)?;
        for sub in ["repositories", "tmp", "trash", "orphans"] {
            mkdir0700(&root.join(sub))?;
        }
        Ok(Self {
            root,
            options,
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Read-only open: does not create, chmod, or modify anything on disk.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root_ref = root.as_ref();
        if !root_ref.is_dir() {
            return Err(StorageError::Invalid(
                "storage root does not exist or is not a directory".into(),
            ));
        }
        if fs::symlink_metadata(root_ref)?.file_type().is_symlink() {
            return Err(StorageError::Invalid(
                "storage root must not be a symlink".into(),
            ));
        }
        let root = fs::canonicalize(root_ref)?;
        let repos = root.join("repositories");
        if !repos.is_dir() {
            return Err(StorageError::Invalid(
                "storage root has no repositories/ directory".into(),
            ));
        }
        if fs::symlink_metadata(&repos)?.file_type().is_symlink() {
            return Err(StorageError::Invalid(
                "repositories/ must not be a symlink".into(),
            ));
        }
        Ok(Self {
            root,
            options: StorageOptions::default(),
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Return the per-repository lock for `repository`, creating it if the
    /// [`Weak`] is dead or missing.
    ///
    /// Two calls for the same id while any [`Arc`] is alive return the same
    /// mutex ([`Arc::ptr_eq`]); different ids never block each other.
    ///
    /// Dead [`Weak`] entries are pruned while the map lock is held, so the
    /// map never grows without bound.
    ///
    /// The returned lock is **not** acquired by this function — the caller
    /// must acquire it with `.lock_owned().await` (async) or
    /// `.blocking_lock_owned()` (sync) and move the resulting
    /// [`RepositoryGuard`] into the `spawn_blocking` closure.
    ///
    /// # Lock ordering
    ///
    /// The repository lock MUST be acquired **before** any database pool
    /// connection.  Never await this lock while holding a pool connection.
    /// The API follows this order (lock then connection); the reconciler
    /// must do the same to avoid deadlock on a single-connection pool.
    pub fn repository_lock(&self, repository: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Prune dead entries while we hold the lock.
        map.retain(|_, w| w.strong_count() > 0);
        // If a live entry exists, upgrade and return the same Arc.
        if let Some(arc) = map.get(&repository).and_then(|w| w.upgrade()) {
            return arc;
        }
        // Create a new mutex, store a Weak to it, and return the Arc.
        let arc = Arc::new(tokio::sync::Mutex::new(()));
        map.insert(repository, Arc::downgrade(&arc));
        arc
    }

    /// Number of entries currently in the lock map (including dead [`Weak`]
    /// entries that have not yet been pruned).
    #[doc(hidden)]
    pub fn lock_entries(&self) -> usize {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn repository_path(&self, repository: Uuid) -> PathBuf {
        let id = simple(repository);
        self.root
            .join("repositories")
            .join(&id[..2])
            .join(format!("{id}.git"))
    }

    /// Verified canonical path of a repository's Git directory, for read-only
    /// browsing. Errors (including a missing directory) are the caller's to
    /// map; no detail about the filesystem layout is exposed here.
    pub fn read_path(&self, repository: Uuid) -> Result<PathBuf, StorageError> {
        self.verified_dir(&self.repository_path(repository))
    }

    fn tmp_path(&self, operation: Uuid) -> PathBuf {
        self.root.join("tmp").join(simple(operation))
    }

    /// `<dir>/<id>.<secs>.git`, bumping `secs` past any existing entry so two moves of
    /// the same repository within one second cannot collide.
    fn timestamped_path(&self, dir: &str, repository: Uuid, now: u64) -> PathBuf {
        (now..)
            .map(|secs| {
                self.root
                    .join(dir)
                    .join(format!("{}.{}.git", simple(repository), secs))
            })
            .find(|path| !path.exists())
            .expect("an unused timestamp exists")
    }

    fn trash_path(&self, repository: Uuid, now: u64) -> PathBuf {
        self.timestamped_path("trash", repository, now)
    }

    fn orphan_path(&self, repository: Uuid, now: u64) -> PathBuf {
        self.timestamped_path("orphans", repository, now)
    }

    fn verified_dir(&self, path: &Path) -> Result<PathBuf, StorageError> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(StorageError::Invalid(
                "storage path must be a real directory, not a symlink".into(),
            ));
        }
        let canonical = fs::canonicalize(path)?;
        if !canonical.starts_with(&self.root) {
            return Err(StorageError::Invalid(
                "path escaped the configured storage root".into(),
            ));
        }
        Ok(canonical)
    }

    fn ensure_parent(&self, path: &Path) -> Result<PathBuf, StorageError> {
        let parent = path
            .parent()
            .ok_or_else(|| StorageError::Invalid("path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        self.verified_dir(parent)
    }

    fn sync_ancestors(&self, path: &Path) -> Result<(), StorageError> {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == self.root {
                break;
            }
            sync_dir(d)?;
            dir = d.parent();
        }
        Ok(())
    }

    fn configure(&self, repo: &Path) -> Result<(), StorageError> {
        let max_input_size = self.options.max_input_size.to_string();
        let configs = [
            ("core.fsync", "objects,derived-metadata,reference"),
            ("core.fsyncMethod", "fsync"),
            ("receive.fsckObjects", "true"),
            ("transfer.fsckObjects", "true"),
            ("receive.denyNonFastForwards", "false"),
            ("receive.denyDeletes", "false"),
            ("receive.maxInputSize", max_input_size.as_str()),
            ("gc.auto", "0"),
        ];
        for (key, value) in configs {
            checked(
                git()
                    .arg("--git-dir")
                    .arg(repo)
                    .args(["config", key, value])
                    .output()?,
            )?;
        }
        if !self.options.hooks_path.as_os_str().is_empty() {
            checked(
                git()
                    .arg("--git-dir")
                    .arg(repo)
                    .args(["config", "core.hooksPath"])
                    .arg(&self.options.hooks_path)
                    .output()?,
            )?;
        }
        Ok(())
    }

    pub fn create(
        &self,
        repository: Uuid,
        operation: Uuid,
        format: ObjectFormat,
    ) -> Result<PathBuf, StorageError> {
        let final_path = self.repository_path(repository);
        if let Ok(existing) = self.verified_dir(&final_path) {
            if detect_object_format(&existing)? == format {
                return Ok(existing);
            }
            return Err(StorageError::Invalid(
                "existing repository has a different object format".into(),
            ));
        }
        let tmp = self.tmp_path(operation);
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir(&tmp)?;
        checked(
            git()
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "init",
                    "--bare",
                    "--object-format",
                    format.arg(),
                    "--",
                ])
                .arg(&tmp)
                .output()?,
        )?;
        self.configure(&tmp)?;
        fs::create_dir(tmp.join("githearth"))?;
        fsync_tree(&tmp)?;
        self.ensure_parent(&final_path)?;
        fs::rename(&tmp, &final_path)?;
        self.sync_ancestors(&final_path)?;
        self.verified_dir(&final_path)
    }

    /// Copies every head and tag ref (and the objects they reach) from `source`
    /// into `target`, which must already exist (see [`create`](Storage::create)).
    /// The target keeps its own configuration, hooks and empty journal; only Git
    /// objects and branch/tag refs are brought over. Used to create a fork.
    ///
    /// The source's `HEAD` is mirrored too, so a fork of a repository whose
    /// default branch is not `main` still points its default branch at the same
    /// branch instead of a dangling `main` (which would break `git clone`).
    ///
    /// The caller must hold the target repository's lock (and, to be safe
    /// against concurrent pushes, the source's) before calling this.
    pub fn fetch_refs(&self, target: Uuid, source: Uuid) -> Result<(), StorageError> {
        let target_path = self.verified_dir(&self.repository_path(target))?;
        let source_path = self.verified_dir(&self.repository_path(source))?;
        checked(
            git()
                .arg("--git-dir")
                .arg(&target_path)
                // A server-managed copy, not a user push: skip the
                // reference-transaction hook that refuses internal ref writes.
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(["fetch", "--no-write-fetch-head", "--"])
                .arg(&source_path)
                .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"])
                .output()?,
        )?;
        // Mirror the source's HEAD (symbolic ref, or a detached object id) so
        // the fork's default branch matches the parent's.
        let head = std::fs::read_to_string(source_path.join("HEAD"))?;
        std::fs::write(target_path.join("HEAD"), head)?;
        Ok(())
    }

    /// Fetches one ref (as `refs/...`) and the objects it reaches from `source`
    /// into `target` without creating or moving any ref in the target. Used to
    /// bring a fork's head branch into the target so a merge candidate can be
    /// built there (ADR 0008). The caller must hold the target's lock and, to
    /// be safe against a concurrent push, the source's.
    pub fn fetch_ref(
        &self,
        target: Uuid,
        source: Uuid,
        reference: &str,
    ) -> Result<(), StorageError> {
        let target_path = self.verified_dir(&self.repository_path(target))?;
        let source_path = self.verified_dir(&self.repository_path(source))?;
        checked(
            git()
                .arg("--git-dir")
                .arg(&target_path)
                // Objects only, no target ref is written, but keep the hook
                // disabled so an internal ref write can never run admission.
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(["fetch", "--no-write-fetch-head", "--"])
                .arg(&source_path)
                .arg(reference)
                .output()?,
        )?;
        Ok(())
    }

    pub fn trash(&self, repository: Uuid) -> Result<PathBuf, StorageError> {
        let source = self.repository_path(repository);
        self.verified_dir(&source)?;
        let target = self.trash_path(repository, now_seconds());
        fs::rename(&source, &target)?;
        self.sync_ancestors(&target)?;
        Ok(target)
    }

    pub fn restore(&self, repository: Uuid, trashed: &Path) -> Result<PathBuf, StorageError> {
        let canonical = self.verified_dir(trashed)?;
        if canonical.parent() != Some(self.root.join("trash").as_path()) {
            return Err(StorageError::Invalid(
                "trashed path is not inside the trash directory".into(),
            ));
        }
        let target = self.repository_path(repository);
        self.ensure_parent(&target)?;
        fs::rename(&canonical, &target)?;
        self.sync_ancestors(&target)?;
        self.verified_dir(&target)
    }

    pub fn orphan(&self, repository: Uuid) -> Result<PathBuf, StorageError> {
        let source = self.repository_path(repository);
        self.verified_dir(&source)?;
        let target = self.orphan_path(repository, now_seconds());
        fs::rename(&source, &target)?;
        self.sync_ancestors(&target)?;
        Ok(target)
    }

    fn timestamped(name: &str) -> Option<(&str, u64)> {
        let stem = name.strip_suffix(".git")?;
        let (id, secs) = stem.rsplit_once('.')?;
        Some((id, secs.parse().ok()?))
    }

    pub fn purge_trash(&self, older_than: SystemTime) -> Result<usize, StorageError> {
        let cutoff = older_than
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut removed = 0;
        // One unremovable entry must not keep the rest of the trash forever: try them
        // all, then report the first failure.
        let mut first_error = None;
        for entry in fs::read_dir(self.root.join("trash"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if Self::timestamped(&name).is_some_and(|(_, secs)| secs < cutoff) {
                match fs::remove_dir_all(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
        }
        match first_error {
            Some(e) => Err(e.into()),
            None => Ok(removed),
        }
    }

    pub fn upload_pack(
        &self,
        repository: Uuid,
        mode: PackMode,
        protocol: Option<&str>,
    ) -> Result<std::process::Command, StorageError> {
        let path = self.verified_dir(&self.repository_path(repository))?;
        let mut cmd = crate::git();
        cmd.arg("upload-pack").arg("--strict");
        mode.args(&mut cmd);
        cmd.arg(&path);
        if let Some(proto) = protocol {
            validate_git_protocol(proto)?;
            cmd.env("GIT_PROTOCOL", proto);
        }
        Ok(cmd)
    }

    pub fn receive_pack(
        &self,
        repository: Uuid,
        mode: PackMode,
        protocol: Option<&str>,
        hooks_dir: &Path,
    ) -> Result<std::process::Command, StorageError> {
        let path = self.verified_dir(&self.repository_path(repository))?;
        let mut cmd = crate::git();
        // Repositories created before hooks existed still run our hooks.
        if !hooks_dir.as_os_str().is_empty() {
            cmd.arg("-c")
                .arg(format!("core.hooksPath={}", hooks_dir.display()));
        }
        cmd.arg("receive-pack");
        mode.args(&mut cmd);
        cmd.arg(&path);
        if let Some(proto) = protocol {
            validate_git_protocol(proto)?;
            cmd.env("GIT_PROTOCOL", proto);
        }
        Ok(cmd)
    }

    /// Removes stale `*.lock` files under the repository's `refs/` and its
    /// `packed-refs.lock`, left behind by a receive-pack killed mid-update.
    ///
    /// Safe because the caller holds the per-repository lock: pushes and
    /// replay are serialized by it and nothing else writes refs.  Never
    /// follows symlinks; a lock that happens to be a symlink is unlinked.
    pub fn remove_stale_locks(&self, repository: Uuid) -> Result<(), StorageError> {
        let path = self.verified_dir(&self.repository_path(repository))?;
        remove_lock_file(&path.join("packed-refs.lock"))?;
        remove_lock_files_recursively(&path.join("refs"))
    }

    pub fn scan(&self) -> Result<Scan, StorageError> {
        let mut repositories = Vec::new();
        let mut malformed = Vec::new();
        for pair in fs::read_dir(self.root.join("repositories"))? {
            let pair = pair?;
            let pair_name = pair.file_name();
            let pair_name = pair_name.to_string_lossy();
            if !pair.file_type()?.is_dir() {
                malformed.push(format!("repositories/{pair_name}"));
                continue;
            }
            if pair_name.len() != 2
                || !pair_name
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                malformed.push(format!("repositories/{pair_name}"));
                continue;
            }
            for entry in fs::read_dir(pair.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !entry.file_type()?.is_dir() {
                    malformed.push(format!("repositories/{pair_name}/{name}"));
                    continue;
                }
                let stem = match name.strip_suffix(".git") {
                    Some(s) if s.len() == 32 && s.bytes().all(is_lower_hex) => s,
                    _ => {
                        malformed.push(format!("repositories/{pair_name}/{name}"));
                        continue;
                    }
                };
                match Uuid::parse_str(stem) {
                    Ok(id) => repositories.push(id),
                    Err(_) => malformed.push(format!("repositories/{pair_name}/{name}")),
                }
            }
        }
        Ok(Scan {
            repositories,
            malformed,
        })
    }

    /// List every entry under `tmp/`.  Each entry holds the file name and the
    /// parsed operation id, or `None` when the name is not a UUID.
    ///
    /// This is the first phase of two-phase tmp cleanup.  The caller must list
    /// candidates *before* reading the live operation set; see [`clean_tmp`].
    pub fn list_tmp(&self) -> Result<Vec<TmpEntry>, StorageError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Reject anything containing a path separator, or equal to `.`/`..`.
            if name_str.contains(std::path::MAIN_SEPARATOR) || name_str == "." || name_str == ".." {
                continue;
            }

            let operation = Uuid::parse_str(&name_str).ok();
            entries.push(TmpEntry {
                name: name_str.into_owned(),
                operation,
            });
        }
        Ok(entries)
    }

    /// Remove tmp entries whose operation is not in `live_operations`.
    ///
    /// **Call-order invariant:** `candidates` MUST have been obtained via
    /// [`list_tmp`] *before* `live_operations` was read.  This guarantees that
    /// any directory already on disk at listing time belongs to a `creating`
    /// row that the live-set query will observe — because the API commits the
    /// row *before* it creates `tmp/<operation>`.
    ///
    /// Entries whose file name is not a valid UUID are always removable (they
    /// cannot correspond to an in-flight create).  An entry that no longer
    /// exists (e.g. the create finished and renamed it) is silently skipped
    /// (`NotFound` is not an error).  Returns the number of entries actually
    /// removed.
    pub fn clean_tmp(
        &self,
        candidates: &[TmpEntry],
        live_operations: &HashSet<Uuid>,
    ) -> Result<usize, StorageError> {
        let mut removed = 0;
        for candidate in candidates {
            let live = candidate
                .operation
                .is_some_and(|id| live_operations.contains(&id));
            if live {
                continue;
            }
            let path = self.root.join("tmp").join(&candidate.name);
            match fs::remove_dir_all(&path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // The entry disappeared between list_tmp and now — the
                    // create completed and renamed it, or another reconciler
                    // pass cleaned it.  Not an error.
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(removed)
    }

    /// Create a fresh scratch directory under `tmp/` for read-only commands
    /// that must write objects without touching any repository (e.g. compare's
    /// merge-tree). Its UUID name is removed on drop and, if leaked, by the
    /// existing two-phase [`clean_tmp`] (which removes UUID-named entries that
    /// are not live `creating` operations). If `clean_tmp` removes it while
    /// still in use, any Git command writing through it fails instead of
    /// producing a wrong answer.
    pub fn scratch_dir(&self) -> Result<ScratchDir, StorageError> {
        let path = self.tmp_path(Uuid::new_v4());
        fs::create_dir(&path)?;
        Ok(ScratchDir { path })
    }
}

/// A fresh scratch directory under `<root>/tmp/`; removed on drop.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A single entry under `tmp/` returned by [`Storage::list_tmp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmpEntry {
    /// Raw file name (a single path component, never `.` or `..`).
    pub name: String,
    /// Parsed operation id, or `None` when the name is not a valid UUID.
    pub operation: Option<Uuid>,
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// Unlink one lock file, tolerating its absence.
fn remove_lock_file(path: &Path) -> Result<(), StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Recursively unlink `*.lock` files.  Symlinked directories are not followed
/// (their metadata is read without following); a `.lock` symlink is unlinked.
fn remove_lock_files_recursively(dir: &Path) -> Result<(), StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            remove_lock_files_recursively(&entry.path())?;
        } else if entry.file_name().to_string_lossy().ends_with(".lock") {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Sum of regular-file lengths in `dir`, recursing into subdirectories.
/// Symlinks are not followed; `excluded_subdir` (if Some) is skipped entirely.
pub fn dir_size(dir: &Path, excluded_subdir: Option<&Path>) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let ft = metadata.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if excluded_subdir.is_some_and(|ex| entry.path() == ex) {
                continue;
            }
            total += dir_size(&entry.path(), None)?;
        } else {
            total += metadata.len();
        }
    }
    Ok(total)
}

/// How `upload-pack` / `receive-pack` talk to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackMode {
    /// Smart HTTP: the ref advertisement only.
    Advertise,
    /// Smart HTTP: one stateless request/response after the advertisement.
    Rpc,
    /// SSH: one bidirectional session, advertisement first.
    Session,
}

impl PackMode {
    fn args(self, cmd: &mut std::process::Command) {
        match self {
            PackMode::Advertise => cmd.args(["--stateless-rpc", "--advertise-refs"]),
            PackMode::Rpc => cmd.arg("--stateless-rpc"),
            PackMode::Session => cmd,
        };
    }
}

pub fn validate_git_protocol(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 256 {
        return Err(StorageError::Invalid(
            "GIT_PROTOCOL must be 1..256 bytes".into(),
        ));
    }
    for part in value.split(':') {
        match part {
            "version=0" | "version=1" | "version=2" => {}
            other => {
                return Err(StorageError::Invalid(format!(
                    "unsupported GIT_PROTOCOL component: {other}"
                )));
            }
        }
    }
    Ok(())
}

/// Current `refs/heads/*` and `refs/tags/*` as `refname -> ObjectId`, for
/// journal classification.
pub fn read_refs(repo_path: &Path) -> Result<HashMap<String, ObjectId>, StorageError> {
    let output = checked(
        git()
            .arg("--git-dir")
            .arg(repo_path)
            .args(["for-each-ref", "--format=%(objectname) %(refname)"])
            .args(["refs/heads/", "refs/tags/"])
            .output()?,
    )?;
    let text = String::from_utf8(output)
        .map_err(|e| StorageError::Invalid(format!("non-UTF-8 ref output: {e}")))?;
    let mut refs = HashMap::new();
    for line in text.lines() {
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| StorageError::Invalid("unexpected ref record".into()))?;
        refs.insert(
            name.to_owned(),
            oid.parse()
                .map_err(|e: githearth_core::InvalidId| StorageError::Invalid(e.to_string()))?,
        );
    }
    Ok(refs)
}

impl Storage {
    fn run(&self, repository: &RepositoryId, args: &[&str]) -> Result<Vec<u8>, StorageError> {
        let uuid = Uuid::parse_str(&repository.to_string())
            .map_err(|e| StorageError::Invalid(e.to_string()))?;
        let path = self.verified_dir(&self.repository_path(uuid))?;
        checked(git().arg("--git-dir").arg(&path).args(args).output()?)
    }
}

impl RepositoryReader for Storage {
    type Error = StorageError;

    fn list_refs(&self, repository: &RepositoryId) -> Result<Vec<GitRef>, StorageError> {
        let bytes = self.run(
            repository,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads/",
                "refs/tags/",
            ],
        )?;
        let text = String::from_utf8(bytes).map_err(|e| StorageError::Invalid(e.to_string()))?;
        text.lines()
            .map(|line| {
                let (oid, name) = line
                    .split_once(' ')
                    .ok_or_else(|| StorageError::Invalid("unexpected ref record".into()))?;
                Ok(GitRef {
                    name: name.to_owned(),
                    target: oid.parse().map_err(|e: githearth_core::InvalidId| {
                        StorageError::Invalid(e.to_string())
                    })?,
                })
            })
            .collect()
    }

    fn read_blob(
        &self,
        repository: &RepositoryId,
        object: &ObjectId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let oid = object.to_string();
        if self.run(repository, &["cat-file", "-t", &oid])? != b"blob\n" {
            return Err(StorageError::Invalid(
                "requested object is not a blob".into(),
            ));
        }
        let size_bytes = self.run(repository, &["cat-file", "-s", &oid])?;
        let size: u64 = String::from_utf8_lossy(&size_bytes)
            .trim()
            .parse()
            .map_err(|_| StorageError::Invalid("invalid blob size".into()))?;
        if size > max_bytes as u64 {
            return Err(StorageError::TooLarge {
                size,
                limit: max_bytes,
            });
        }
        let data = self.run(repository, &["cat-file", "blob", &oid])?;
        if data.len() as u64 != size {
            return Err(StorageError::Invalid("blob length mismatch".into()));
        }
        Ok(data)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Scan {
    pub repositories: Vec<Uuid>,
    pub malformed: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Problem {
    pub subject: String,
    pub message: String,
}

impl Storage {
    pub fn check(&self) -> Result<(usize, Vec<Problem>), StorageError> {
        let mut problems = Vec::new();
        let scan = self.scan()?;
        for m in &scan.malformed {
            problems.push(Problem {
                subject: m.clone(),
                message: "malformed entry".to_owned(),
            });
        }
        for repo_id in &scan.repositories {
            let repo_path = self.repository_path(*repo_id);
            let subject = repo_id.simple().to_string();
            let output = git()
                .arg("--git-dir")
                .arg(&repo_path)
                .args(["fsck", "--full", "--no-progress", "--no-dangling"])
                .output()?;
            if !output.status.success() {
                let first_line = String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("git fsck failed")
                    .to_owned();
                problems.push(Problem {
                    subject: subject.clone(),
                    message: first_line,
                });
            }
            match journal::verify(&repo_path) {
                Ok(_) => {}
                Err(e) => problems.push(Problem {
                    subject: subject.clone(),
                    message: format!("journal: {e}"),
                }),
            }
        }
        Ok((scan.repositories.len(), problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn dir_size_nested_files() {
        let tmp = std::env::temp_dir().join(format!("githearth-dirsize-{}", Uuid::new_v4()));
        fs::create_dir_all(tmp.join("a/b")).unwrap();
        fs::write(tmp.join("a/b/c.txt"), [0u8; 100]).unwrap();
        fs::write(tmp.join("a/b/d.txt"), [0u8; 50]).unwrap();
        assert_eq!(dir_size(&tmp, None).unwrap(), 150);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_empty_dir() {
        let tmp = std::env::temp_dir().join(format!("githearth-dirsize-{}", Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        assert_eq!(dir_size(&tmp, None).unwrap(), 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_symlink_not_followed() {
        let tmp = std::env::temp_dir().join(format!("githearth-dirsize-{}", Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("real.txt"), [0u8; 42]).unwrap();
        std::os::unix::fs::symlink(tmp.join("real.txt"), tmp.join("link.txt")).unwrap();
        assert_eq!(dir_size(&tmp, None).unwrap(), 42);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_excluded_subdir() {
        let tmp = std::env::temp_dir().join(format!("githearth-dirsize-{}", Uuid::new_v4()));
        fs::create_dir_all(tmp.join("keep")).unwrap();
        fs::create_dir_all(tmp.join("skip")).unwrap();
        fs::write(tmp.join("keep/a.txt"), [0u8; 100]).unwrap();
        fs::write(tmp.join("skip/b.txt"), [0u8; 999]).unwrap();
        assert_eq!(dir_size(&tmp, Some(&tmp.join("skip"))).unwrap(), 100);
        let _ = fs::remove_dir_all(&tmp);
    }
}
