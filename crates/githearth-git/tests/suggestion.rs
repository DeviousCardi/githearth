//! Applying a ```suggestion block builds a commit whose tree replaces one line
//! of one blob, leaving everything else untouched.

use githearth_git::StorageError;
use githearth_git::merge::Identity;
use githearth_git::suggestion::{apply_suggestion, suggestion_replacement};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

fn git(dir: &Path, args: &[&str]) -> String {
    git_env(dir, &[], args)
}

fn git_env(dir: &Path, envs: &[(&str, &str)], args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for (k, v) in envs {
        command.env(k, v);
    }
    let out = command.current_dir(dir).args(args).output().expect("git");
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Run git against the bare repository directory.
fn git_repo(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .arg("--git-dir")
        .arg(repo)
        .args(args);
    let out = command.output().expect("git");
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fixture {
    dir: PathBuf,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("githearth-suggestion-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        git(&dir, &["init", "-q", "-b", "main", d]);
        fs::write(dir.join("notes.txt"), "one\ntwo\nthree\n").unwrap();
        git(&dir, &["-C", d, "add", "notes.txt"]);
        git(
            &dir,
            &[
                "-C",
                d,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "root",
            ],
        );
        let head = git(&dir, &["-C", d, "rev-parse", "HEAD"]);
        Fixture { dir, head }
    }

    fn repo(&self) -> PathBuf {
        self.dir.join(".git")
    }

    fn blob(&self, commit: &str, path: &str) -> String {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .arg("--git-dir")
            .arg(self.repo())
            .args(["cat-file", "blob", &format!("{commit}:{path}")]);
        let out = command.output().expect("git");
        assert!(
            out.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn identity(name: &str) -> Identity {
    Identity {
        name: name.to_owned(),
        email: format!("{name}@users.noreply.githearth.invalid"),
        time: 1_700_000_000,
        offset: "+0000".into(),
    }
}

#[test]
fn apply_replaces_one_line_and_keeps_the_rest() {
    let f = Fixture::new();
    let who = identity("applier");
    let commit = apply_suggestion(
        &f.repo(),
        &f.head,
        "notes.txt",
        2,
        b"dos\n",
        b"Apply suggestion\n",
        &who,
        &who,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_ne!(commit, f.head);
    assert_eq!(f.blob(&commit, "notes.txt"), "one\ndos\nthree\n");
    // The new commit has the head as its single parent.
    let parents = git_repo(&f.repo(), &["rev-list", "--parents", "-n", "1", &commit]);
    assert_eq!(parents, format!("{commit} {}", f.head));
}

#[test]
fn apply_can_delete_and_append_lines() {
    let f = Fixture::new();
    let who = identity("applier");
    // Delete line 2.
    let commit = apply_suggestion(
        &f.repo(),
        &f.head,
        "notes.txt",
        2,
        b"",
        b"Apply suggestion\n",
        &who,
        &who,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(f.blob(&commit, "notes.txt"), "one\nthree\n");
    // Append several lines in place of line 1.
    let commit = apply_suggestion(
        &f.repo(),
        &commit,
        "notes.txt",
        1,
        b"alpha\nbeta\ngamma\n",
        b"Apply suggestion\n",
        &who,
        &who,
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(f.blob(&commit, "notes.txt"), "alpha\nbeta\ngamma\nthree\n");
}

#[test]
fn apply_rejects_bad_inputs() {
    let f = Fixture::new();
    let who = identity("applier");
    let timeout = Duration::from_secs(10);

    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "notes.txt",
            0,
            b"x\n",
            b"m\n",
            &who,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "notes.txt",
            99,
            b"x\n",
            b"m\n",
            &who,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "missing.txt",
            1,
            b"x\n",
            b"m\n",
            &who,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));
    // A binary blob is refused.
    fs::write(f.dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    git(&f.dir, &["-C", f.dir.to_str().unwrap(), "add", "bin.dat"]);
    git(
        &f.dir,
        &[
            "-C",
            f.dir.to_str().unwrap(),
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "bin",
        ],
    );
    let head = git(
        &f.dir,
        &["-C", f.dir.to_str().unwrap(), "rev-parse", "HEAD"],
    );
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &head,
            "bin.dat",
            1,
            b"x\n",
            b"m\n",
            &who,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));
}

#[test]
fn apply_rejects_oversized_replacement_and_bad_identity() {
    let f = Fixture::new();
    let who = identity("applier");
    let timeout = Duration::from_secs(10);

    // A replacement larger than the blob limit is refused.
    let oversized = vec![b'a'; githearth_git::browse::MAX_BLOB_BYTES + 1];
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "notes.txt",
            1,
            &oversized,
            b"m\n",
            &who,
            &who,
            timeout
        ),
        Err(StorageError::TooLarge { .. })
    ));

    // A forbidden character in an identity is refused.
    let mut bad = who.clone();
    bad.name = "evil<name".into();
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "notes.txt",
            1,
            b"x\n",
            b"m\n",
            &bad,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));

    // A malformed offset is refused.
    let mut bad_offset = who.clone();
    bad_offset.offset = "00:00".into();
    assert!(matches!(
        apply_suggestion(
            &f.repo(),
            &f.head,
            "notes.txt",
            1,
            b"x\n",
            b"m\n",
            &bad_offset,
            &who,
            timeout
        ),
        Err(StorageError::Invalid(_))
    ));
}

#[test]
fn replacement_parsing_is_pure() {
    assert_eq!(suggestion_replacement("plain"), None);
    assert_eq!(
        suggestion_replacement("```suggestion\nfixed\n```\n"),
        Some("fixed\n".into())
    );
}
