use githearth_core::{ObjectId, RepositoryId, RepositoryReader};
use githearth_git::{
    StorageError,
    layout::{ObjectFormat, Storage},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "githearth-test-{}-{}",
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

fn git(work: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(work)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn reads_real_git_push_and_enforces_blob_boundaries() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id: RepositoryId = "0123456789abcdef0123456789abcdef".parse().unwrap();
    let uuid = Uuid::parse_str(&id.to_string()).unwrap();
    let bare = storage
        .create(uuid, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    // Idempotent create: second call with same id and format returns the existing path.
    let bare2 = storage
        .create(uuid, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    assert_eq!(bare, bare2);
    assert!(storage.list_refs(&id).unwrap().is_empty());
    let work = fixture.0.join("work");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Fixture"]);
    git(&work, &["config", "user.email", "fixture@example.invalid"]);
    let bytes = b"GitHearth\0binary\n";
    fs::write(work.join("example.bin"), bytes).unwrap();
    git(&work, &["add", "example.bin"]);
    git(&work, &["commit", "-m", "Fixture"]);
    git(&work, &["push", bare.to_str().unwrap(), "main"]);
    let commit: ObjectId = git(&work, &["rev-parse", "HEAD"]).parse().unwrap();
    let blob: ObjectId = git(&work, &["rev-parse", "HEAD:example.bin"])
        .parse()
        .unwrap();
    let refs = storage.list_refs(&id).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "refs/heads/main");
    assert_eq!(refs[0].target, commit);
    assert_eq!(storage.read_blob(&id, &blob, bytes.len()).unwrap(), bytes);
    assert!(matches!(
        storage.read_blob(&id, &blob, 1),
        Err(StorageError::TooLarge { .. })
    ));
    assert!(storage.read_blob(&id, &commit, 1024).is_err());
    let absent: ObjectId = "f".repeat(40).parse().unwrap();
    assert!(storage.read_blob(&id, &absent, 1024).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_repository_symlinks() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id: RepositoryId = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse().unwrap();
    // Place a symlink at the expected sharded repository path.
    let repo_path = root
        .join("repositories")
        .join("aa")
        .join(format!("{id}.git"));
    fs::create_dir_all(repo_path.parent().unwrap()).unwrap();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, &repo_path).unwrap();
    assert!(storage.list_refs(&id).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_repository_escaping_root() {
    let fixture = Fixture::new();
    let root = fixture.0.join("storage");
    let storage = Storage::open(&root).unwrap();
    let id: RepositoryId = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
    // Create a repo outside, then symlink the sharded path to it via a chain that escapes.
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    // Place the sharded dir structure and symlink the .git to outside.
    let shard = root.join("repositories").join("bb");
    fs::create_dir_all(&shard).unwrap();
    let link = shard.join(format!("{id}.git"));
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert!(storage.list_refs(&id).is_err());
}

#[test]
fn unknown_repository_is_reported() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id: RepositoryId = "cccccccccccccccccccccccccccccccc".parse().unwrap();
    assert!(storage.list_refs(&id).is_err());
}

#[test]
fn trash_repository_is_not_readable() {
    let fixture = Fixture::new();
    let storage = Storage::open(fixture.0.join("storage")).unwrap();
    let id: RepositoryId = "dddddddddddddddddddddddddddddddd".parse().unwrap();
    let uuid = Uuid::parse_str(&id.to_string()).unwrap();
    storage
        .create(uuid, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    // Move the repository to trash.
    storage.trash(uuid).unwrap();
    // The reader only looks in repositories/, so the trashed repo is not found.
    assert!(storage.list_refs(&id).is_err());
}
