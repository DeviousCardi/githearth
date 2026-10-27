use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

const ID: &str = "0123456789abcdef0123456789abcdef";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir() -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("githearth-cli-{}-{counter}", std::process::id()));
    fs::create_dir(&path).unwrap();
    path
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        Self(temp_dir())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_githearth"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn git(work: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
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
fn usage_is_reported_without_arguments() {
    for args in [vec![], vec!["init"], vec!["init", "/tmp"]] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "githearth: usage: githearth <init|refs|blob> <storage-root> <repository-id> [object-id]\n"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn incorrect_argument_counts_are_rejected() {
    let temp = TempDir::new();
    let root = temp.path().join("storage");
    let root = root.to_str().unwrap();
    for args in [
        vec!["init", root, ID, "extra"],
        vec!["refs", root, ID, "extra"],
        vec!["blob", root, ID],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "githearth: incorrect number of arguments\n"
        );
    }
}

#[test]
fn unknown_command_is_rejected() {
    let temp = TempDir::new();
    let root = temp.path().join("storage");
    let output = cli(&["frobnicate", root.to_str().unwrap(), ID]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "githearth: unknown command; expected init, refs, or blob\n"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_identifiers_are_rejected() {
    let temp = TempDir::new();
    let root = temp.path().join("storage");
    let root = root.to_str().unwrap();
    for id in ["", "abc", "0123456789ABCDEF0123456789ABCDEF", "../escape"] {
        let output = cli(&["init", root, id]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "githearth: repository ID must be 32 lowercase hexadecimal characters\n"
        );
    }
    let output = cli(&["blob", root, ID, "not-an-object"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "githearth: object ID must be a full lowercase SHA-1 or SHA-256 hexadecimal ID\n"
    );
}

#[test]
fn missing_repository_is_reported() {
    let temp = TempDir::new();
    let root = temp.path().join("storage");
    let root = root.to_str().unwrap();
    let absent = "f".repeat(40);
    for args in [vec!["refs", root, ID], vec!["blob", root, ID, &absent]] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(1));
        let message = stderr(&output);
        assert!(
            message.starts_with("githearth: filesystem or process error:"),
            "{message}"
        );
        assert!(message.contains("No such file or directory"), "{message}");
    }
}

#[test]
fn unusable_storage_root_is_reported() {
    let temp = TempDir::new();
    let file = temp.path().join("storage-file");
    fs::write(&file, b"not a directory").unwrap();
    let output = cli(&["init", file.to_str().unwrap(), ID]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.starts_with("githearth: filesystem or process error:"),
        "{message}"
    );
}

#[test]
fn init_creates_once_and_reports_duplicates() {
    let temp = TempDir::new();
    let storage = temp.path().join("storage");
    let output = cli(&["init", storage.to_str().unwrap(), ID]);
    assert!(output.status.success(), "{}", stderr(&output));
    let created = PathBuf::from(stdout(&output).trim());
    assert!(created.is_dir());
    assert_eq!(
        created.file_name().unwrap().to_str().unwrap(),
        format!("{ID}.git")
    );
    assert_eq!(
        created.parent().unwrap(),
        fs::canonicalize(&storage).unwrap()
    );

    let duplicate = cli(&["init", storage.to_str().unwrap(), ID]);
    assert_eq!(duplicate.status.code(), Some(1));
    let message = stderr(&duplicate);
    assert!(
        message.starts_with("githearth: filesystem or process error:"),
        "{message}"
    );
    assert!(message.contains("File exists"), "{message}");
}

#[test]
fn refs_and_blobs_are_read_from_a_real_repository() {
    let temp = TempDir::new();
    let storage = temp.path().join("storage");
    let root = storage.to_str().unwrap();
    let output = cli(&["init", root, ID]);
    assert!(output.status.success(), "{}", stderr(&output));
    let bare = PathBuf::from(stdout(&output).trim());

    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "user.name", "Fixture"]);
    git(&work, &["config", "user.email", "fixture@example.invalid"]);
    let bytes = b"GitHearth\0binary\n";
    fs::write(work.join("example.bin"), bytes).unwrap();
    git(&work, &["add", "example.bin"]);
    git(&work, &["commit", "-m", "Fixture"]);
    git(&work, &["tag", "v1"]);
    git(
        &work,
        &["push", bare.to_str().unwrap(), "main", "refs/tags/v1"],
    );
    let commit = git(&work, &["rev-parse", "HEAD"]);
    let blob = git(&work, &["rev-parse", "HEAD:example.bin"]);

    let output = cli(&["refs", root, ID]);
    assert!(output.status.success(), "{}", stderr(&output));
    let refs = stdout(&output);
    assert!(
        refs.lines()
            .any(|line| line == format!("{commit} refs/heads/main")),
        "{refs}"
    );
    assert!(
        refs.lines()
            .any(|line| line == format!("{commit} refs/tags/v1")),
        "{refs}"
    );

    let output = cli(&["blob", root, ID, &blob]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout, bytes);

    let output = cli(&["blob", root, ID, &commit]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "githearth: invalid repository data: requested object is not a blob\n"
    );

    let absent = "f".repeat(40);
    let output = cli(&["blob", root, ID, &absent]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.starts_with("githearth: Git failed:"), "{message}");
}
