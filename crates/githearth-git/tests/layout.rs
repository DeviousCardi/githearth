use githearth_git::layout::{
    ObjectFormat, Storage, StorageOptions, check_git_version, detect_object_format, parse_version,
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "githearth-layout-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn git_config(repo: &Path, key: &str) -> String {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(repo)
        .args(["config", key])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn options(hooks: &Path) -> StorageOptions {
    StorageOptions {
        hooks_path: hooks.to_path_buf(),
        max_input_size: 1024 * 1024,
    }
}

fn repo_dir(root: &Path, id: Uuid) -> PathBuf {
    let s = id.simple().to_string();
    root.join("repositories")
        .join(&s[..2])
        .join(format!("{s}.git"))
}

fn tmp_dir(root: &Path, op: Uuid) -> PathBuf {
    root.join("tmp").join(op.simple().to_string())
}

#[test]
fn version_parsing_accepts_supported_releases_and_rejects_old_or_garbage() {
    let v = parse_version("git version 2.43.0").unwrap();
    assert_eq!((v.major, v.minor), (2, 43));
    let v = parse_version("git version 2.36.0.windows.1").unwrap();
    assert_eq!((v.major, v.minor), (2, 36));
    assert!(parse_version("git version 2.35.9").is_err());
    assert!(parse_version("garbage").is_err());
    assert!(parse_version("").is_err());
    assert!(check_git_version().is_ok());
}

#[test]
fn create_sets_config_for_sha1_and_sha256() {
    let fixture = Fixture::new();
    let hooks = fixture.0.join("hooks");
    let storage = Storage::open_with(fixture.0.join("storage"), options(&hooks)).unwrap();
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let repo_id = Uuid::new_v4();
        let path = storage.create(repo_id, Uuid::new_v4(), format).unwrap();
        assert_eq!(detect_object_format(&path).unwrap(), format);
        assert_eq!(
            git_config(&path, "core.fsync"),
            "objects,derived-metadata,reference"
        );
        assert_eq!(git_config(&path, "core.fsyncMethod"), "fsync");
        assert_eq!(
            git_config(&path, "core.hooksPath"),
            hooks.display().to_string()
        );
        assert_eq!(git_config(&path, "receive.fsckObjects"), "true");
        assert_eq!(git_config(&path, "transfer.fsckObjects"), "true");
        assert_eq!(git_config(&path, "receive.denyNonFastForwards"), "false");
        assert_eq!(git_config(&path, "receive.denyDeletes"), "false");
        assert_eq!(git_config(&path, "receive.maxInputSize"), "1048576");
        assert_eq!(git_config(&path, "gc.auto"), "0");
        assert!(path.join("githearth").is_dir());
    }
}

#[test]
fn create_is_idempotent_and_refuses_mismatched_format() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let repo_id = Uuid::new_v4();
    let first = storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let second = storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    assert_eq!(first, second);
    assert!(
        storage
            .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha256)
            .is_err()
    );
}

#[test]
fn create_removes_leftover_tmp_before_reuse() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let repo_id = Uuid::new_v4();
    let op = Uuid::new_v4();
    // Simulate a crashed attempt that left a partial tmp directory behind.
    let stale = tmp_dir(&root, op);
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("junk"), b"partial").unwrap();
    let path = storage.create(repo_id, op, ObjectFormat::Sha1).unwrap();
    assert!(path.is_dir());
    assert!(!stale.join("junk").exists());
    assert_eq!(path, repo_dir(&root, repo_id).canonicalize().unwrap());
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_root() {
    let fixture = Fixture::new();
    let target = fixture.0.join("real");
    fs::create_dir(&target).unwrap();
    let link = fixture.0.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(Storage::open(&link).is_err());
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_repository_dir() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let repo_id = Uuid::new_v4();
    storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let repo = repo_dir(&root, repo_id);
    fs::remove_dir_all(&repo).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, &repo).unwrap();
    // Any operation resolving the repository path must refuse the symlink.
    assert!(storage.trash(repo_id).is_err());
    assert!(storage.orphan(repo_id).is_err());
}

#[test]
fn trash_restore_orphan_purge_scan_clean_tmp() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let live = Uuid::new_v4();
    let gone = Uuid::new_v4();
    storage
        .create(live, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    storage
        .create(gone, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    // scan finds both.
    let scan = storage.scan().unwrap();
    assert_eq!(scan.repositories.len(), 2);
    assert!(scan.malformed.is_empty());

    // trash and restore.
    let trashed = storage.trash(gone).unwrap();
    assert!(trashed.starts_with(root.join("trash")));
    assert!(storage.scan().unwrap().repositories.contains(&live));
    assert!(!storage.scan().unwrap().repositories.contains(&gone));
    let restored = storage.restore(gone, &trashed).unwrap();
    assert_eq!(restored, repo_dir(&root, gone).canonicalize().unwrap());
    assert!(storage.scan().unwrap().repositories.contains(&gone));

    // restore refuses a path outside trash/.
    assert!(storage.restore(gone, &fixture.0.join("elsewhere")).is_err());

    // orphan moves a live repository out of repositories/.
    let orphaned = storage.orphan(live).unwrap();
    assert!(orphaned.starts_with(root.join("orphans")));
    assert!(!storage.scan().unwrap().repositories.contains(&live));

    // trash again to exercise purge.
    let t = storage.trash(gone).unwrap();
    let future = SystemTime::now() + Duration::from_secs(60);
    assert_eq!(storage.purge_trash(future).unwrap(), 1);
    assert!(!t.exists());
    let past = SystemTime::now() - Duration::from_secs(60);
    assert_eq!(storage.purge_trash(past).unwrap(), 0);

    // clean_tmp removes only entries with no live operation.
    let op_live = Uuid::new_v4();
    let op_dead = Uuid::new_v4();
    fs::create_dir_all(tmp_dir(&root, op_live)).unwrap();
    fs::create_dir_all(tmp_dir(&root, op_dead)).unwrap();
    let mut live_ops = HashSet::new();
    live_ops.insert(op_live);
    assert_eq!(storage.clean_tmp(&live_ops).unwrap(), 1);
    assert!(tmp_dir(&root, op_live).is_dir());
    assert!(!tmp_dir(&root, op_dead).exists());
}

#[test]
fn scan_reports_malformed_names() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    // malformed pair directory name.
    fs::create_dir(root.join("repositories").join("zz")).unwrap();
    // malformed repository directory name under a valid pair.
    let good = Uuid::new_v4();
    let s = good.simple().to_string();
    let pair = root.join("repositories").join(&s[..2]);
    fs::create_dir_all(&pair).unwrap();
    fs::create_dir(pair.join("not-a-repo")).unwrap();
    // non-directory entries are skipped silently.
    fs::write(root.join("repositories").join("stray-file"), b"").unwrap();
    fs::write(pair.join("stray-entry"), b"").unwrap();
    let scan = storage.scan().unwrap();
    assert!(scan.repositories.is_empty());
    assert!(!scan.malformed.is_empty());
    assert!(scan.malformed.iter().any(|m| m.contains("zz")));
    assert!(scan.malformed.iter().any(|m| m.contains("not-a-repo")));
}

#[test]
fn open_is_idempotent() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    Storage::open(&root).unwrap();
    Storage::open(&root).unwrap();
}

#[test]
fn detect_rejects_non_repository() {
    let fixture = Fixture::new();
    let empty = fixture.0.join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(detect_object_format(&empty).is_err());
}

#[test]
fn restore_refuses_non_trash_path() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    storage
        .create(a, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let b_path = storage
        .create(b, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    assert!(storage.restore(a, &b_path).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_repository_escaping_root_via_symlink() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let s = id.simple().to_string();
    let pair = root.join("repositories").join(&s[..2]);
    let outside = fixture.0.join("outside");
    fs::rename(&pair, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &pair).unwrap();
    assert!(storage.trash(id).is_err());
}

#[test]
fn purge_trash_keeps_recent_and_ignores_non_directories() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let t = storage.trash(id).unwrap();
    // A recent entry is not old enough to purge.
    let past = SystemTime::now() - Duration::from_secs(60);
    assert_eq!(storage.purge_trash(past).unwrap(), 0);
    assert!(t.exists());
    // A stray non-directory entry is ignored while the old entry is removed.
    fs::write(root.join("trash").join("stray"), b"").unwrap();
    let future = SystemTime::now() + Duration::from_secs(60);
    assert_eq!(storage.purge_trash(future).unwrap(), 1);
    assert!(root.join("trash").join("stray").exists());
}
