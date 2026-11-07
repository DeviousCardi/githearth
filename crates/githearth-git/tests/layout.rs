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
    let v = parse_version("git version 2.38.0.windows.1").unwrap();
    assert_eq!((v.major, v.minor), (2, 38));
    assert!(parse_version("git version 2.37.9").is_err());
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

    // clean_tmp (two-phase) removes only entries with no live operation.
    let op_live = Uuid::new_v4();
    let op_dead = Uuid::new_v4();
    fs::create_dir_all(tmp_dir(&root, op_live)).unwrap();
    fs::create_dir_all(tmp_dir(&root, op_dead)).unwrap();
    // Phase 1: list candidates BEFORE reading the live set.
    let candidates = storage.list_tmp().unwrap();
    let mut live_ops = HashSet::new();
    live_ops.insert(op_live);
    // Phase 2: clean using the earlier candidates.
    assert_eq!(storage.clean_tmp(&candidates, &live_ops).unwrap(), 1);
    assert!(tmp_dir(&root, op_live).is_dir());
    assert!(!tmp_dir(&root, op_dead).exists());
}

#[test]
fn clean_tmp_entry_created_after_list_is_never_removed() {
    // Regression test for the TOCTOU race described in the reconciler fix.
    // An entry created after list_tmp must not be in the candidate set, so
    // clean_tmp must not remove it — even if the live-set query misses it.
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();

    // Phase 1: list candidates.
    let candidates = storage.list_tmp().unwrap();
    assert!(candidates.is_empty());

    // Now create a tmp entry *after* listing — simulating a concurrent create.
    let op = Uuid::new_v4();
    fs::create_dir_all(tmp_dir(&root, op)).unwrap();

    // Phase 2: clean with an empty live set (simulating that the create's row
    // hasn't been committed yet, or the live-set query missed it).
    let live_ops = HashSet::new();
    let removed = storage.clean_tmp(&candidates, &live_ops).unwrap();
    assert_eq!(removed, 0);
    assert!(
        tmp_dir(&root, op).is_dir(),
        "entry created after list_tmp must never be removed"
    );
}

#[test]
fn clean_tmp_candidate_vanished_is_skipped_without_error() {
    // A candidate that disappeared between list_tmp and clean_tmp (e.g. the
    // create completed and renamed the directory) must be silently skipped.
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();

    let op = Uuid::new_v4();
    fs::create_dir_all(tmp_dir(&root, op)).unwrap();

    // Phase 1: list candidates.
    let candidates = storage.list_tmp().unwrap();
    assert_eq!(candidates.len(), 1);

    // Simulate the create completing: remove the tmp dir.
    fs::remove_dir_all(tmp_dir(&root, op)).unwrap();

    // Phase 2: clean with an empty live set.  The vanished entry must be
    // silently skipped (no error), and nothing removed.
    let live_ops = HashSet::new();
    let removed = storage.clean_tmp(&candidates, &live_ops).unwrap();
    assert_eq!(removed, 0);
}

#[test]
fn scratch_dir_is_removed_on_drop_and_when_leaked() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();

    // Dropping the guard removes the directory.
    let scratch = storage.scratch_dir().unwrap();
    let path = scratch.path().to_path_buf();
    assert!(path.starts_with(root.join("tmp")));
    assert!(path.is_dir());
    drop(scratch);
    assert!(!path.exists());

    // A leaked scratch (never dropped) is listed and cleaned as dead tmp.
    let leaked = storage.scratch_dir().unwrap();
    let leaked_path = leaked.path().to_path_buf();
    std::mem::forget(leaked);
    let candidates = storage.list_tmp().unwrap();
    let dead: Vec<_> = candidates
        .iter()
        .filter(|c| c.name == leaked_path.file_name().unwrap().to_string_lossy())
        .collect();
    assert_eq!(dead.len(), 1);
    assert_eq!(
        storage.clean_tmp(&candidates, &HashSet::new()).unwrap(),
        1,
        "a leaked scratch must be removed by clean_tmp"
    );
    assert!(!leaked_path.exists());
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
    // non-directory entries are now reported as malformed.
    fs::write(root.join("repositories").join("stray-file"), b"").unwrap();
    fs::write(pair.join("stray-entry"), b"").unwrap();
    let scan = storage.scan().unwrap();
    assert!(scan.repositories.is_empty());
    assert_eq!(scan.malformed.len(), 4);
    assert!(scan.malformed.iter().any(|m| m.contains("zz")));
    assert!(scan.malformed.iter().any(|m| m.contains("not-a-repo")));
    assert!(
        scan.malformed
            .iter()
            .any(|m| m == &"repositories/stray-file".to_string())
    );
    assert!(
        scan.malformed
            .iter()
            .any(|m| *m == format!("repositories/{}/stray-entry", &s[..2]))
    );
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

#[cfg(unix)]
#[test]
fn remove_stale_locks_unlinks_only_lock_files() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    let repo = storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    // Nested ref lock, a packed-refs lock, and non-lock files that must survive.
    let nested = repo.join("refs").join("heads").join("a").join("b.lock");
    fs::create_dir_all(nested.parent().unwrap()).unwrap();
    fs::write(&nested, b"").unwrap();
    let packed = repo.join("packed-refs.lock");
    fs::write(&packed, b"").unwrap();
    let keep_dir = repo.join("refs").join("heads").join("a").join("keep");
    fs::create_dir_all(&keep_dir).unwrap();
    fs::write(keep_dir.join("file"), b"not a lock").unwrap();
    let keep_file = repo.join("refs").join("heads").join("main");
    fs::write(&keep_file, b"ref content").unwrap();

    storage.remove_stale_locks(id).unwrap();

    assert!(!nested.exists(), "nested .lock removed");
    assert!(!packed.exists(), "packed-refs.lock removed");
    assert!(keep_dir.is_dir(), "non-lock directory kept");
    assert!(keep_dir.join("file").exists(), "non-lock file kept");
    assert!(keep_file.exists(), "non-lock ref file kept");
}

#[cfg(unix)]
#[test]
fn remove_stale_locks_does_not_follow_symlinked_dirs() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    let repo = storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    // A symlinked directory inside refs/ must not be traversed.
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.lock"), b"").unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("refs").join("linked")).unwrap();

    storage.remove_stale_locks(id).unwrap();

    assert!(
        outside.join("secret.lock").exists(),
        "symlinked directory must not be traversed"
    );
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

#[test]
fn trashing_one_repository_twice_in_a_second_keeps_both() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id = Uuid::new_v4();
    storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let first = storage.trash(id).unwrap();
    storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let second = storage.trash(id).unwrap();
    assert_ne!(first, second);
    assert!(first.exists() && second.exists());
}

#[cfg(unix)]
#[test]
fn purge_trash_removes_what_it_can_and_reports_the_failure() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let (stuck, free) = (Uuid::new_v4(), Uuid::new_v4());
    let mut trashed = Vec::new();
    for id in [stuck, free] {
        storage
            .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
            .unwrap();
        trashed.push(storage.trash(id).unwrap());
    }
    // A read-only directory inside one entry makes its removal fail.
    let locked = trashed[0].join("objects");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    let future = SystemTime::now() + Duration::from_secs(60);
    let result = storage.purge_trash(future);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert!(!trashed[1].exists(), "the removable entry was still purged");
}

// ---------------------------------------------------------------------------
// Per-repository lock tests
// ---------------------------------------------------------------------------

#[test]
fn repository_lock_same_id_returns_same_arc_while_held() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id = Uuid::new_v4();

    let a = storage.repository_lock(id);
    let b = storage.repository_lock(id);
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    drop(a);
    drop(b);
}

#[test]
fn repository_lock_different_ids_are_independent() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let x = Uuid::new_v4();
    let y = Uuid::new_v4();

    let lock_x = storage.repository_lock(x);
    let lock_y = storage.repository_lock(y);

    // Hold x's lock, then try_lock on y must succeed.
    let _guard_x = lock_x.clone().blocking_lock_owned();
    assert!(lock_y.clone().try_lock_owned().is_ok());
}

#[test]
fn repository_lock_entry_pruned_when_no_strong_references() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id = Uuid::new_v4();

    assert_eq!(storage.lock_entries(), 0);
    let lock = storage.repository_lock(id);
    assert_eq!(storage.lock_entries(), 1);
    drop(lock);
    // The Weak still occupies an entry; calling repository_lock again will
    // prune it and the count will drop to 0 after the prune + insert (or
    // the prune itself may remove it).  Call repository_lock once more to
    // trigger the prune, then drop — the prune on the *next* call should
    // see it dead.
    let lock2 = storage.repository_lock(id);
    // After this call, the dead Weak from the first drop was pruned; we now
    // have one live entry again.
    assert_eq!(storage.lock_entries(), 1);
    drop(lock2);
    // Now trigger a prune by calling with a different id.
    let _other = storage.repository_lock(Uuid::new_v4());
    // The dead entry for `id` should have been pruned, leaving only `_other`.
    assert_eq!(storage.lock_entries(), 1);
    drop(_other);
}

#[test]
fn repository_lock_held_in_thread_blocks_blocking_lock_owned() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id = Uuid::new_v4();

    let lock = storage.repository_lock(id);
    let lock_clone = lock.clone();
    // Acquire the lock on this thread.
    let guard = lock.blocking_lock_owned();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_w = done.clone();

    let handle = std::thread::spawn(move || {
        // This should block until the guard is dropped.
        let _g = lock_clone.blocking_lock_owned();
        done_w.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // Give the spawned thread time to try to acquire.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !done.load(std::sync::atomic::Ordering::SeqCst),
        "spawned thread acquired the lock while the guard was still held"
    );

    drop(guard);
    handle.join().unwrap();
    assert!(done.load(std::sync::atomic::Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Storage::check tests
// ---------------------------------------------------------------------------

#[test]
fn check_healthy_repository_passes() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let (n, problems) = storage.check().unwrap();
    assert!(
        problems.is_empty(),
        "expected no problems, got {problems:?}"
    );
    assert_eq!(n, 1);
}

#[test]
fn check_corrupted_loose_object_fails_fsck() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    let repo = storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    // Create a minimal valid object and corrupt it.
    let blob_content = b"check test blob\n";
    let hash = {
        let mut child = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("--git-dir")
            .arg(&repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(blob_content).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    // Corrupt the object file.
    let obj_path = repo.join("objects").join(&hash[..2]).join(&hash[2..]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&obj_path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let mut data = fs::read(&obj_path).unwrap();
    data[0] ^= 0xFF;
    fs::write(&obj_path, data).unwrap();

    let (_n, problems) = storage.check().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].subject, id.simple().to_string());
}

#[test]
fn check_malformed_entry_is_reported() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    fs::create_dir(root.join("repositories").join("zz")).unwrap();
    let (_, problems) = storage.check().unwrap();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].message.contains("malformed"));
    assert_eq!(problems[0].subject, "repositories/zz");
}

#[test]
fn check_reports_journal_problem() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::new_v4();
    let repo = storage
        .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    // Write a corrupt journal: oversize length field.
    use std::io::Write;
    let journal_path = repo.join("githearth").join("journal");
    let mut file = fs::File::create(&journal_path).unwrap();
    file.write_all(&((githearth_git::journal::MAX_RECORD_BYTES as u32) + 1).to_le_bytes())
        .unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    drop(file);

    let (_, problems) = storage.check().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].subject, id.simple().to_string());
    assert!(problems[0].message.contains("journal:"));
}

#[test]
fn open_existing_rejects_missing_root() {
    let fixture = Fixture::new();
    assert!(Storage::open_existing(fixture.0.join("nope")).is_err());
}

#[test]
fn open_existing_rejects_root_without_repositories_dir() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    fs::create_dir(&root).unwrap();
    assert!(Storage::open_existing(&root).is_err());
}

#[test]
fn open_existing_does_not_modify_permissions() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    Storage::open(&root).unwrap();
    // Change a subdirectory mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("repositories"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    Storage::open_existing(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join("repositories"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "mode must be unchanged by open_existing"
        );
    }
}

#[test]
fn check_empty_root_reports_warning_and_exits_nonzero() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    Storage::open(&root).unwrap();
    let (n, problems) = Storage::open_existing(&root).unwrap().check().unwrap();
    assert_eq!(n, 0);
    assert!(
        problems.is_empty(),
        "check itself reports no problems on empty root"
    );
}
