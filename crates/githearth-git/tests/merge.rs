use githearth_git::StorageError;
use githearth_git::merge::{
    Candidate, Identity, Method, Request, build_candidate, update_ref_expected,
};
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
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(dir)
        .args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_repo(repo: &Path, args: &[&str]) -> String {
    String::from_utf8(git_repo_output(repo, args).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn git_repo_raw(repo: &Path, args: &[&str]) -> String {
    String::from_utf8(git_repo_output(repo, args).stdout).unwrap()
}

fn git_repo_output(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .arg("--git-dir")
        .arg(repo)
        .args(args);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "githearth-merge-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main", "."]);
        Self { dir }
    }

    fn new_sha256() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "githearth-merge-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        git(
            &dir,
            &["init", "-q", "-b", "main", "--object-format=sha256", "."],
        );
        Self { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.join(".git")
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.dir.join(name), content).unwrap();
    }

    fn run(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    fn oid(&self, rev: &str) -> String {
        self.run(&["rev-parse", rev])
    }

    fn commit_all(&self, message: &str) -> String {
        self.run(&["add", "-A"]);
        self.run(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture Author",
            "-c",
            "user.email=author@example.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ]);
        self.oid("HEAD")
    }

    fn commit_all_as(&self, name: &str, email: &str, date: &str, message: &str) -> String {
        self.run(&["add", "-A"]);
        let user_name = format!("user.name={name}");
        let user_email = format!("user.email={email}");
        git_env(
            &self.dir,
            &[("GIT_AUTHOR_DATE", date), ("GIT_COMMITTER_DATE", date)],
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                &user_name,
                "-c",
                &user_email,
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
        self.oid("HEAD")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

fn identity() -> Identity {
    Identity {
        name: "Merge Bot".into(),
        email: "bot@example.invalid".into(),
        time: 1700000001,
        offset: "+0000".into(),
    }
}

fn build(
    repo: &Path,
    method: Method,
    target: &str,
    source: &str,
) -> Result<Candidate, StorageError> {
    let author = identity();
    let committer = identity();
    build_candidate(
        repo,
        None,
        &Request {
            method,
            target,
            source,
            message: "merge message",
            author: &author,
            committer: &committer,
        },
        Duration::from_secs(30),
    )
}

fn ready(candidate: Candidate) -> String {
    match candidate {
        Candidate::Ready { oid } => oid,
        other => panic!("expected Ready, got {other:?}"),
    }
}

fn tree_of(repo: &Path, oid: &str) -> String {
    git_repo(repo, &["rev-parse", &format!("{oid}^{{tree}}")])
}

fn tree_entries(repo: &Path, tree: &str) -> Vec<String> {
    git_repo(repo, &["ls-tree", "-r", "--name-only", tree])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn commit_fields(repo: &Path, oid: &str) -> Vec<String> {
    let out = git_repo_raw(
        repo,
        &[
            "log",
            "-1",
            "-z",
            "--format=%an%x00%ae%x00%at%x00%cn%x00%ce%x00%B",
            oid,
        ],
    );
    let mut fields: Vec<String> = out.split('\0').map(str::to_owned).collect();
    while fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    fields
}

/// A clean-merge shape: `base` diverges into `side` (adds `side.txt`) and
/// `main` (adds `main.txt`). Returns (fixture, target, source).
fn diverged_fixture() -> (Fixture, String, String) {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("side.txt", "side\n");
    f.commit_all("side change");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("main.txt", "main\n");
    f.commit_all("main change");
    let target = f.oid("HEAD");
    (f, target, source)
}

#[test]
fn merge_clean_writes_commit_with_two_parents_and_merged_tree() {
    let (f, target, source) = diverged_fixture();
    let oid = ready(build(&f.repo(), Method::Merge, &target, &source).unwrap());

    assert_eq!(git_repo(&f.repo(), &["cat-file", "-t", &oid]), "commit");
    assert_eq!(
        git_repo(&f.repo(), &["log", "-1", "--format=%P", &oid]),
        format!("{target} {source}")
    );
    let entries = tree_entries(&f.repo(), &tree_of(&f.repo(), &oid));
    for name in ["base.txt", "side.txt", "main.txt"] {
        assert!(
            entries.contains(&name.to_owned()),
            "tree lacks {name}: {entries:?}"
        );
    }
    let fields = commit_fields(&f.repo(), &oid);
    assert_eq!(fields[0], "Merge Bot");
    assert_eq!(fields[1], "bot@example.invalid");
    assert_eq!(fields[5], "merge message");
}

#[test]
fn merge_conflict_returns_paths_and_leaves_refs_unchanged() {
    let f = Fixture::new();
    f.write("f.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("f.txt", "side\n");
    f.commit_all("side edit");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("f.txt", "main\n");
    f.commit_all("main edit");
    let target = f.oid("HEAD");

    let refs_before = f.run(&["for-each-ref", "--format=%(objectname) %(refname)"]);
    let candidate = build(&f.repo(), Method::Merge, &target, &source).unwrap();
    assert_eq!(candidate, Candidate::Conflicts(vec!["f.txt".to_owned()]));
    assert_eq!(
        f.run(&["for-each-ref", "--format=%(objectname) %(refname)"]),
        refs_before
    );
}

#[test]
fn squash_has_one_parent_and_the_merge_tree() {
    let (f, target, source) = diverged_fixture();
    let merge_oid = ready(build(&f.repo(), Method::Merge, &target, &source).unwrap());
    let squash_oid = ready(build(&f.repo(), Method::Squash, &target, &source).unwrap());

    assert_eq!(
        git_repo(&f.repo(), &["log", "-1", "--format=%P", &squash_oid]),
        target
    );
    assert_eq!(
        tree_of(&f.repo(), &squash_oid),
        tree_of(&f.repo(), &merge_oid)
    );
    let fields = commit_fields(&f.repo(), &squash_oid);
    assert_eq!(fields[5], "merge message");
}

#[test]
fn rebase_linear_preserves_author_date_message_and_uses_request_committer() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("a.txt", "one\n");
    f.commit_all_as(
        "Alice",
        "alice@example.invalid",
        "1700000100 +0000",
        "first",
    );
    f.write("b.txt", "two\n");
    f.commit_all_as(
        "Alice",
        "alice@example.invalid",
        "1700000200 +0000",
        "second",
    );
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("m.txt", "main\n");
    f.commit_all("main change");
    let target = f.oid("HEAD");

    let oid = ready(build(&f.repo(), Method::Rebase, &target, &source).unwrap());

    let first_oid = git_repo(&f.repo(), &["log", "-1", "--format=%P", &oid]);
    let first = commit_fields(&f.repo(), &first_oid);
    let second = commit_fields(&f.repo(), &oid);

    assert_eq!(first[0], "Alice");
    assert_eq!(first[1], "alice@example.invalid");
    assert_eq!(first[2], "1700000100");
    assert_eq!(first[3], "Merge Bot");
    assert_eq!(first[4], "bot@example.invalid");
    assert_eq!(first[5], "first\n");
    assert_eq!(
        git_repo(&f.repo(), &["log", "-1", "--format=%P", &first_oid]),
        target
    );

    assert_eq!(second[0], "Alice");
    assert_eq!(second[2], "1700000200");
    assert_eq!(second[3], "Merge Bot");
    assert_eq!(second[5], "second\n");
    assert_eq!(
        git_repo(&f.repo(), &["log", "-1", "--format=%P", &oid]),
        first_oid
    );

    let entries = tree_entries(&f.repo(), &tree_of(&f.repo(), &oid));
    for name in ["base.txt", "a.txt", "b.txt", "m.txt"] {
        assert!(
            entries.contains(&name.to_owned()),
            "tree lacks {name}: {entries:?}"
        );
    }
}

#[test]
fn rebase_skips_an_already_applied_commit() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("a.txt", "x\n");
    f.commit_all("add a");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("a.txt", "x\n");
    f.commit_all("add a too");
    let target = f.oid("HEAD");

    assert_eq!(
        build(&f.repo(), Method::Rebase, &target, &source).unwrap(),
        Candidate::UpToDate
    );
}

#[test]
fn rebase_conflict_in_the_middle_returns_conflicting_paths() {
    let f = Fixture::new();
    f.write("f.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("a.txt", "one\n");
    f.commit_all("add a");
    f.write("f.txt", "side\n");
    f.commit_all("edit f");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("f.txt", "main\n");
    f.commit_all("edit f");
    let target = f.oid("HEAD");

    assert_eq!(
        build(&f.repo(), Method::Rebase, &target, &source).unwrap(),
        Candidate::Conflicts(vec!["f.txt".to_owned()])
    );
}

#[test]
fn rebase_refuses_a_merge_commit_in_range() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "a"]);
    f.write("a.txt", "a\n");
    f.commit_all("A");
    f.run(&["checkout", "-q", "main"]);
    f.write("m.txt", "m\n");
    f.commit_all("M");
    git_env(
        &f.dir,
        &[
            ("GIT_AUTHOR_DATE", "1700000100 +0000"),
            ("GIT_COMMITTER_DATE", "1700000100 +0000"),
        ],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture Author",
            "-c",
            "user.email=author@example.invalid",
            "merge",
            "--no-ff",
            "-q",
            "a",
            "-m",
            "merge a",
        ],
    );
    let source = f.oid("HEAD");

    assert_eq!(
        build(&f.repo(), Method::Rebase, &base, &source).unwrap(),
        Candidate::RebaseMergeCommit
    );
}

#[test]
fn fast_forward_is_ready_when_target_is_an_ancestor_otherwise_not() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("a.txt", "a\n");
    f.commit_all("A");
    let source = f.oid("HEAD");

    assert_eq!(
        build(&f.repo(), Method::FastForward, &base, &source).unwrap(),
        Candidate::Ready {
            oid: source.clone()
        }
    );

    f.run(&["checkout", "-q", "main"]);
    f.write("m.txt", "m\n");
    f.commit_all("M");
    let target = f.oid("HEAD");
    assert_eq!(
        build(&f.repo(), Method::FastForward, &target, &source).unwrap(),
        Candidate::NotFastForward
    );
}

#[test]
fn up_to_date_is_returned_for_every_method_when_source_is_an_ancestor() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("a.txt", "a\n");
    f.commit_all("A");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.run(&["merge", "--ff-only", "side"]);
    f.write("m.txt", "m\n");
    f.commit_all("M");
    let target = f.oid("HEAD");

    for method in [
        Method::Merge,
        Method::Squash,
        Method::Rebase,
        Method::FastForward,
    ] {
        assert_eq!(
            build(&f.repo(), method, &target, &source).unwrap(),
            Candidate::UpToDate,
            "method {method:?}"
        );
    }
}

#[test]
fn unrelated_histories_are_reported() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    let target = f.oid("HEAD");
    f.run(&["checkout", "-q", "--orphan", "other"]);
    f.run(&["rm", "-rf", "-q", "."]);
    f.write("other.txt", "other\n");
    f.commit_all("orphan root");
    let source = f.oid("HEAD");

    for method in [Method::Merge, Method::Squash, Method::Rebase] {
        assert_eq!(
            build(&f.repo(), method, &target, &source).unwrap(),
            Candidate::Unrelated,
            "method {method:?}"
        );
    }
    assert_eq!(
        build(&f.repo(), Method::FastForward, &target, &source).unwrap(),
        Candidate::NotFastForward
    );
}

#[test]
fn identical_inputs_produce_identical_candidate_oids() {
    let (f, target, source) = diverged_fixture();
    let first = ready(build(&f.repo(), Method::Merge, &target, &source).unwrap());
    let second = ready(build(&f.repo(), Method::Merge, &target, &source).unwrap());
    assert_eq!(first, second);
}

#[test]
fn invalid_oids_are_rejected() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    let head = f.oid("HEAD");
    let repo = f.repo();

    assert!(matches!(
        build(&repo, Method::Merge, "zzz", &head),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        build(&repo, Method::Merge, &"0".repeat(40), &head),
        Err(StorageError::Invalid(_))
    ));
}

#[test]
fn identity_validation_rejects_forbidden_fields() {
    let good = identity();
    let cases: Vec<(Identity, Identity)> = vec![
        (
            Identity {
                name: "bad\nname".into(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            Identity {
                email: "bad<mail>".into(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            Identity {
                offset: "0000".into(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            Identity {
                offset: "+00:00".into(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            Identity {
                offset: "+0000 x".into(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            Identity {
                offset: String::new(),
                ..good.clone()
            },
            good.clone(),
        ),
        (
            good.clone(),
            Identity {
                offset: "-5".into(),
                ..good.clone()
            },
        ),
    ];
    for (author, committer) in cases {
        let result = build_candidate(
            Path::new("/nonexistent"),
            None,
            &Request {
                method: Method::Merge,
                target: &"a".repeat(40),
                source: &"b".repeat(40),
                message: "merge",
                author: &author,
                committer: &committer,
            },
            Duration::from_secs(30),
        );
        assert!(
            matches!(result, Err(StorageError::Invalid(_))),
            "{author:?} / {committer:?} should be rejected"
        );
    }
}

#[test]
fn sha256_repositories_work() {
    let f = Fixture::new_sha256();
    f.write("base.txt", "base\n");
    f.commit_all("base");
    f.run(&["checkout", "-q", "-b", "side"]);
    f.write("side.txt", "side\n");
    f.commit_all("side change");
    let source = f.oid("HEAD");
    f.run(&["checkout", "-q", "main"]);
    f.write("main.txt", "main\n");
    f.commit_all("main change");
    let target = f.oid("HEAD");

    let oid = ready(build(&f.repo(), Method::Merge, &target, &source).unwrap());
    assert_eq!(oid.len(), 64);
    assert_eq!(git_repo(&f.repo(), &["cat-file", "-t", &oid]), "commit");
    assert_eq!(
        git_repo(&f.repo(), &["log", "-1", "--format=%P", &oid]),
        format!("{target} {source}")
    );
}

#[test]
fn a_zero_timeout_is_reported_as_timeout() {
    let (f, target, source) = diverged_fixture();
    let author = identity();
    let committer = identity();
    let result = build_candidate(
        &f.repo(),
        None,
        &Request {
            method: Method::Merge,
            target: &target,
            source: &source,
            message: "merge",
            author: &author,
            committer: &committer,
        },
        Duration::ZERO,
    );
    assert!(matches!(result, Err(StorageError::Timeout)));
}

/// A hooks directory whose `reference-transaction` script appends its state,
/// the `GITHEARTH_*` environment variables and stdin to a log file.
struct StubHooks {
    dir: PathBuf,
    log: PathBuf,
}

impl StubHooks {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "githearth-merge-hooks-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("hook.log");
        let script = format!(
            "#!/bin/sh\nLOG={}\n{{\n  printf 'state=%s\\n' \"$1\"\n  printf 'operation=%s\\n' \"$GITHEARTH_OPERATION\"\n  printf 'actor=%s\\n' \"$GITHEARTH_ACTOR\"\n  printf 'merge=%s\\n' \"$GITHEARTH_MERGE\"\n  cat\n  printf -- '---\\n'\n}} >> \"$LOG\"\nexit 0\n",
            log.display()
        );
        let hook = dir.join("reference-transaction");
        fs::write(&hook, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir, log }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    fn read(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for StubHooks {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn update_ref_expected_moves_branch_and_journals_through_hooks() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    f.write("child.txt", "child\n");
    let child = f.commit_all("child");
    git(&f.dir, &["branch", "target", &base]);

    let hooks = StubHooks::new();
    let op = Uuid::new_v4();
    let actor = Uuid::new_v4();
    update_ref_expected(
        &f.repo(),
        hooks.path(),
        "refs/heads/target",
        &child,
        &base,
        op,
        actor,
        Duration::from_secs(10),
    )
    .unwrap();

    assert_eq!(f.oid("target"), child);

    let log = hooks.read();
    assert!(log.contains("state=prepared"), "{log}");
    assert!(log.contains("state=committed"), "{log}");
    assert!(log.contains(&format!("operation={op}")), "{log}");
    assert!(log.contains(&format!("actor={actor}")), "{log}");
    assert!(log.contains("merge=1"), "{log}");
    assert!(
        log.contains(&format!("{base} {child} refs/heads/target")),
        "{log}"
    );
}

#[test]
fn update_ref_expected_mismatch_returns_ref_moved_and_leaves_ref() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    f.write("one.txt", "one\n");
    f.commit_all("one");
    f.write("two.txt", "two\n");
    let two = f.commit_all("two");

    let hooks = StubHooks::new();
    let result = update_ref_expected(
        &f.repo(),
        hooks.path(),
        "refs/heads/main",
        &two,
        &base,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Duration::from_secs(10),
    );
    assert!(matches!(result, Err(StorageError::RefMoved(_))));
    assert_eq!(f.oid("main"), two, "ref must not move");
}

#[test]
fn update_ref_expected_on_a_deleted_branch_returns_ref_moved() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    let hooks = StubHooks::new();
    let result = update_ref_expected(
        &f.repo(),
        hooks.path(),
        "refs/heads/gone",
        &base,
        &base,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Duration::from_secs(10),
    );
    assert!(
        matches!(result, Err(StorageError::RefMoved(_))),
        "{result:?}"
    );
}

#[test]
fn update_ref_expected_rejects_invalid_ref_and_oid_before_git() {
    let f = Fixture::new();
    f.write("base.txt", "base\n");
    let base = f.commit_all("base");
    f.write("child.txt", "child\n");
    let child = f.commit_all("child");

    let hooks = StubHooks::new();
    let op = Uuid::new_v4();
    let actor = Uuid::new_v4();

    for (reference, new, old) in [
        ("refs/tags/v1", child.as_str(), base.as_str()),
        ("refs/heads/main", "zzz", base.as_str()),
        ("refs/heads/main", child.as_str(), "zzz"),
        ("refs/heads/", child.as_str(), base.as_str()),
        // A newline or space would inject a second `update-ref --stdin` command.
        (
            "refs/heads/x 0 0\nupdate refs/heads/main",
            child.as_str(),
            base.as_str(),
        ),
        ("refs/heads/a b", child.as_str(), base.as_str()),
    ] {
        assert!(
            matches!(
                update_ref_expected(
                    &f.repo(),
                    hooks.path(),
                    reference,
                    new,
                    old,
                    op,
                    actor,
                    Duration::from_secs(10),
                ),
                Err(StorageError::Invalid(_))
            ),
            "{reference}"
        );
    }
    assert_eq!(hooks.read(), "", "no git process, so no hook invocations");
}
