//! Maintenance of one repository (ADR 0013): repack, grace-bounded pruning,
//! keep points and fault injection. These tests need only the local `git`
//! binary; the durable job, locking and database accounting live in
//! `githearth-server/tests/housekeeping.rs`.

use githearth_git::StorageError;
use githearth_git::layout::{ObjectFormat, Storage, StorageOptions};
use githearth_git::maintenance::{Deferred, FaultPoint, Faults, KeepPoints, Plan, run};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const HOUR: Duration = Duration::from_secs(3600);

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    /// A bare repository created through the storage layer, so it carries the
    /// production configuration (`gc.auto = 0`, fsync settings).
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "githearth-maintenance-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let storage = Storage::open_with(
            dir.join("storage"),
            StorageOptions {
                hooks_path: PathBuf::new(),
                max_input_size: 1 << 20,
            },
        )
        .unwrap();
        let repo = storage
            .create(Uuid::new_v4(), Uuid::new_v4(), ObjectFormat::Sha1)
            .unwrap();
        Self { dir, repo }
    }

    fn git(&self, args: &[&str]) -> std::process::Output {
        git_in(&self.repo, args, None)
    }

    fn git_input(&self, args: &[&str], input: &str) -> std::process::Output {
        git_in(&self.repo, args, Some(input))
    }

    fn git_ok(&self, args: &[&str]) -> String {
        let output = self.git(args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn has_object(&self, oid: &str) -> bool {
        self.git(&["cat-file", "-e", oid]).status.success()
    }

    fn commit(&self, reference: &str, parent: Option<&str>, content: &str) -> String {
        let path = self.dir.join(format!("blob-{}", Uuid::new_v4()));
        fs::write(&path, content).unwrap();
        let blob = self.git_ok(&["hash-object", "-w", path.to_str().unwrap()]);
        let tree = self.git_input(
            &["mktree"],
            &format!("100644 blob {blob}\tfile-{content}\n"),
        );
        assert!(tree.status.success(), "mktree");
        let tree = String::from_utf8(tree.stdout).unwrap().trim().to_owned();
        let mut args = vec!["commit-tree", tree.as_str(), "-m", content];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        let oid = self.git_ok(&args);
        self.git_ok(&["update-ref", reference, &oid]);
        oid
    }

    /// A commit no ref reaches: it exists only until its grace period runs
    /// out.
    fn dangling_commit(&self, content: &str) -> String {
        let reference = format!("refs/heads/scratch-{}", Uuid::new_v4().simple());
        let oid = self.commit(&reference, None, content);
        self.git_ok(&["update-ref", "-d", &reference]);
        oid
    }

    /// Ages every file under `objects/` (loose objects, packs, indexes) by
    /// setting its modification time to now minus `age`.
    fn age_objects(&self, age: Duration) {
        let cutoff = SystemTime::now().checked_sub(age).expect("age is in range");
        set_mtime_tree(&self.repo.join("objects"), cutoff);
    }

    fn objects_size(&self) -> u64 {
        githearth_git::layout::dir_size(&self.repo.join("objects"), None).unwrap()
    }

    fn clone_and_list(&self) -> std::process::Output {
        let into = self.dir.join(format!("clone-{}", Uuid::new_v4()));
        let output = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("clone")
            .arg("--quiet")
            .arg("--")
            .arg(&self.repo)
            .arg(&into)
            .output()
            .unwrap();
        let _ = fs::remove_dir_all(&into);
        output
    }

    fn fsck(&self) {
        let output = self.git(&["fsck", "--full", "--no-dangling"]);
        assert!(
            output.status.success(),
            "fsck failed after maintenance: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git_in(repo: &Path, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .arg("--git-dir")
        .arg(repo)
        .args(args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn set_mtime_tree(dir: &Path, cutoff: SystemTime) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            set_mtime_tree(&path, cutoff);
        } else {
            fs::File::open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(cutoff))
                .unwrap();
        }
    }
}

fn plan(grace: Duration) -> Plan {
    Plan::new(grace).with_timeout(Duration::from_secs(60))
}

/// A "now" grace: everything on disk is already past it.
fn immediate() -> Duration {
    Duration::from_secs(0)
}

fn expect_ok(repo: &Path, plan: &Plan) -> githearth_git::maintenance::Report {
    match run(repo, plan) {
        Ok(report) => report,
        Err(error) => panic!("maintenance run: {error}"),
    }
}

#[test]
fn a_deleted_branch_frees_space_only_after_the_grace_period() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "main");
    // A branch with real history, then deleted: its objects are unreachable
    // but not yet old enough to remove.
    let tip = fx.commit("refs/heads/gone", Some(&main), "branch-content");
    // Only the objects the branch introduces: its commit, tree and blob.
    let branch_objects: Vec<String> = git_objects_reachable(&fx.repo, &tip)
        .into_iter()
        .filter(|oid| !git_objects_reachable(&fx.repo, &main).contains(oid))
        .collect();
    assert_eq!(branch_objects.len(), 3);
    fx.git_ok(&["update-ref", "-d", "refs/heads/gone"]);
    for oid in &branch_objects {
        assert!(fx.has_object(oid));
    }

    // First run packs history (an unpacked repository grows by the pack index
    // and multi-pack index before any run can shrink it).
    let first = expect_ok(&fx.repo, &plan(HOUR));
    assert!(first.repacked && first.pruned);
    assert_eq!(
        first.expired, 0,
        "the branch's objects are younger than the grace period"
    );
    for oid in &branch_objects {
        assert!(fx.has_object(oid), "kept before the grace period: {oid}");
    }
    let packed = fx.objects_size();

    // Still young with a generous grace: kept again.
    expect_ok(&fx.repo, &plan(HOUR));
    for oid in &branch_objects {
        assert!(fx.has_object(oid), "kept before the grace period: {oid}");
    }
    assert_eq!(fx.objects_size(), packed);

    // Past the grace period: removed, and the repository shrinks.
    fx.age_objects(HOUR * 2);
    let report = expect_ok(&fx.repo, &plan(HOUR));
    assert_eq!(report.expired, branch_objects.len());
    for oid in &branch_objects {
        assert!(!fx.has_object(oid), "removed after the grace period: {oid}");
    }
    assert!(
        report.bytes_after < report.bytes_before,
        "freed space: {} -> {}",
        report.bytes_before,
        report.bytes_after
    );
    assert_eq!(
        report.freed_bytes(),
        report.bytes_before - report.bytes_after
    );
    assert!(fx.has_object(&main), "reachable history survives");
    fx.fsck();
    let clone = fx.clone_and_list();
    assert!(
        clone.status.success(),
        "clone after maintenance: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
}

#[test]
fn a_packed_deleted_branch_gets_a_fresh_grace_period() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "main");
    let tip = fx.commit("refs/heads/packed", Some(&main), "packed-content");
    let branch_objects: Vec<String> = git_objects_reachable(&fx.repo, &tip)
        .into_iter()
        .filter(|oid| !git_objects_reachable(&fx.repo, &main).contains(oid))
        .collect();
    // Pack everything first, so the branch's objects live only in a pack, and
    // make that pack look a year old.
    let pack_output = fx.git(&["repack", "-a", "-d", "-q"]);
    assert!(pack_output.status.success());
    let pack_dir = fx.repo.join("objects").join("pack");
    set_mtime_tree(&pack_dir, SystemTime::now() - HOUR * 24 * 365);
    for oid in &branch_objects {
        assert!(!loose_exists(&fx.repo, oid), "packed, not loose: {oid}");
    }
    fx.git_ok(&["update-ref", "-d", "refs/heads/packed"]);

    // The pack's age is not the objects' age: the run loosens them and
    // restarts their grace period, so they are kept.
    let first = expect_ok(&fx.repo, &plan(HOUR));
    assert!(first.repacked && first.pruned);
    assert_eq!(
        first.expired, 0,
        "the pack's age must not shorten the grace period"
    );
    for oid in &branch_objects {
        assert!(
            fx.has_object(oid),
            "kept after a packed branch is deleted: {oid}"
        );
        // `repack -A -d`, not `-a -d`, is what makes the grace period real: it
        // writes the unreachable packed object as a fresh loose file. The next
        // run measures that file's age.
        assert!(
            loose_exists(&fx.repo, oid),
            "-A must loosen the unreachable packed object: {oid}"
        );
    }

    // Once they are loose and old, the next run removes them.
    fx.age_objects(HOUR * 2);
    let second = expect_ok(&fx.repo, &plan(HOUR));
    assert_eq!(second.expired, branch_objects.len());
    for oid in &branch_objects {
        assert!(!fx.has_object(oid), "removed after the grace period: {oid}");
    }
    assert!(fx.has_object(&main));
    fx.fsck();
    assert!(fx.clone_and_list().status.success());
}

/// Documents the hazard the design must never reintroduce: `-a -d` drops an
/// unreachable packed object immediately, with no grace period. The production
/// snapshot uses `-A -d`, which loosens it instead.
#[test]
fn repack_a_deletes_unreachable_packed_objects_immediately() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "main");
    let tip = fx.commit("refs/heads/packed", Some(&main), "packed-content");
    let output = fx.git(&["repack", "-a", "-d", "-q"]);
    assert!(output.status.success());
    fx.git_ok(&["update-ref", "-d", "refs/heads/packed"]);
    let output = fx.git(&["repack", "-a", "-d", "-q"]);
    assert!(output.status.success());
    assert!(
        !fx.has_object(&tip),
        "`-a -d` deletes the unreachable commit before the grace period"
    );
    assert!(fx.has_object(&main));
}

fn loose_exists(repo: &Path, oid: &str) -> bool {
    repo.join("objects")
        .join(&oid[..2])
        .join(&oid[2..])
        .is_file()
}

/// Every object `rev` reaches, including its trees and blobs.
fn git_objects_reachable(repo: &Path, rev: &str) -> Vec<String> {
    let output = git_in(repo, &["rev-list", "--objects", rev], None);
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.split(' ').next().map(str::to_owned))
        .filter(|oid| oid.len() == 40 || oid.len() == 64)
        .collect()
}

#[test]
fn zero_grace_removes_everything_unreachable_at_once() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "keep");
    for i in 0..20 {
        fx.dangling_commit(&format!("gone-{i}"));
    }
    let report = expect_ok(&fx.repo, &plan(immediate()));
    assert!(report.repacked && report.pruned);
    assert_eq!(
        report.expired, 60,
        "commit, tree and blob per dangling commit"
    );
    // A second zero-grace run has nothing left to do.
    let again = expect_ok(&fx.repo, &plan(immediate()));
    assert_eq!(again.expired, 0);
    assert!(fx.has_object(&main));
    fx.fsck();
}

#[test]
fn repeated_runs_are_harmless() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let feature = fx.commit("refs/heads/feature", Some(&main), "two");
    let detached = fx.dangling_commit("three");
    fx.age_objects(HOUR * 2);

    let first = expect_ok(&fx.repo, &plan(HOUR));
    assert!(!fx.has_object(&detached));
    let size = fx.objects_size();
    for _ in 0..3 {
        let again = expect_ok(&fx.repo, &plan(HOUR));
        assert_eq!(again.bytes_after, fx.objects_size());
        assert_eq!(again.expired, 0);
        assert_eq!(fx.objects_size(), size, "a repeated run changes nothing");
    }
    assert!(fx.has_object(&main) && fx.has_object(&feature));
    fx.fsck();
    assert!(first.repacked && first.pruned);
    // The first run packs loose history, which adds the pack index and `.rev`
    // files; `freed_bytes` is therefore only meaningful once a snapshot exists.
    assert!(fx.clone_and_list().status.success());
}

#[test]
fn keep_points_protect_an_unreachable_merge_candidate() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    // A merge candidate: a commit built for an intent whose ref update has not
    // happened, so no ref reaches it.
    let candidate = fx.dangling_commit("candidate");
    let candidate_objects = git_objects_reachable(&fx.repo, &candidate);
    let doomed = fx.dangling_commit("doomed");
    fx.age_objects(HOUR * 2);

    let report = expect_ok(
        &fx.repo,
        &plan(HOUR).with_keep(KeepPoints {
            commits: vec![candidate.clone()],
        }),
    );
    assert_eq!(report.kept_commits, 1);
    for oid in &candidate_objects {
        assert!(fx.has_object(oid), "a pinned candidate survives: {oid}");
    }
    assert!(
        !fx.has_object(&doomed),
        "an unpinned unreachable object goes"
    );
    assert!(fx.has_object(&main));

    // Once the intent is gone the keep point is dropped and the candidate can
    // expire like any other unreachable object.
    expect_ok(&fx.repo, &plan(HOUR));
    for oid in &candidate_objects {
        assert!(!fx.has_object(oid), "an unpinned candidate expires: {oid}");
    }
    fx.fsck();
}

#[test]
fn invalid_keep_points_are_refused_before_anything_changes() {
    let fx = Fixture::new();
    fx.commit("refs/heads/main", None, "one");
    for keep in [
        KeepPoints {
            commits: vec!["not-an-oid".into()],
        },
        KeepPoints {
            commits: vec!["ABC".into()],
        },
        KeepPoints {
            commits: vec!["g".repeat(40)],
        },
    ] {
        let error = run(&fx.repo, &plan(immediate()).with_keep(keep)).unwrap_err();
        assert!(matches!(error, StorageError::Invalid(_)), "{error}");
    }
    assert!(fx.clone_and_list().status.success());
}

#[test]
fn a_repository_borrowing_objects_is_refused() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let alternates = fx.repo.join("objects").join("info").join("alternates");
    fs::create_dir_all(alternates.parent().unwrap()).unwrap();
    fs::write(&alternates, "/somewhere/else/objects\n").unwrap();
    let error = run(&fx.repo, &plan(HOUR)).unwrap_err();
    match &error {
        StorageError::Deferred(Deferred::Alternates { path }) => assert_eq!(path, &alternates),
        other => panic!("expected an alternates refusal, got {other}"),
    }
    assert!(error.to_string().contains("borrows objects"));
    assert!(fx.has_object(&main));

    // An empty alternates file names nothing and does not block a run.
    fs::write(&alternates, "\n").unwrap();
    expect_ok(&fx.repo, &plan(HOUR));
}

#[test]
fn a_real_quarantine_is_refused_and_a_stale_one_is_removed() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    // The exact name Git's tmp-objdir.c creates for a push. There is no
    // `push.lock` inside: the old guard looked for one and therefore never
    // matched a real quarantine.
    let quarantine = fx.repo.join("objects").join("tmp_objdir-incoming-a1B2c3");
    fs::create_dir_all(quarantine.join("pack")).unwrap();
    fs::write(quarantine.join("pack").join("pack-test.pack"), b"in flight").unwrap();
    assert!(
        githearth_git::maintenance::push_quarantines(&fx.repo)
            .unwrap()
            .contains(&quarantine)
    );

    let error = run(&fx.repo, &plan(immediate())).unwrap_err();
    match &error {
        StorageError::Deferred(Deferred::Quarantine { directory }) => {
            assert_eq!(directory.as_path(), quarantine.as_path());
        }
        other => panic!("expected a quarantine refusal, got {other}"),
    }
    assert!(error.to_string().contains("tmp_objdir-incoming"));
    assert!(quarantine.join("pack").join("pack-test.pack").exists());
    assert!(fx.has_object(&main));

    // A quarantine left by a killed process is older than any push deadline:
    // the run removes it rather than deferring forever.
    let stale = plan(HOUR).with_quarantine_max_age(HOUR);
    set_mtime_tree(&quarantine, SystemTime::now() - HOUR * 2);
    let report = expect_ok(&fx.repo, &stale);
    assert!(report.repacked && report.pruned);
    assert!(!quarantine.exists(), "the stale quarantine is removed");
    assert!(fx.has_object(&main));
    fx.fsck();
}

#[test]
fn sha256_repositories_are_maintained_too() {
    let dir = std::env::temp_dir().join(format!(
        "githearth-maintenance-sha256-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    let storage = Storage::open_with(
        dir.join("storage"),
        StorageOptions {
            hooks_path: PathBuf::new(),
            max_input_size: 1 << 20,
        },
    )
    .unwrap();
    let repo = storage
        .create(Uuid::new_v4(), Uuid::new_v4(), ObjectFormat::Sha256)
        .unwrap();
    let fx = Fixture {
        dir: dir.clone(),
        repo: repo.clone(),
    };
    let main = fx.commit("refs/heads/main", None, "one");
    let dangling = fx.dangling_commit("dangling");
    fx.age_objects(HOUR * 2);
    let report = expect_ok(&repo, &plan(HOUR));
    assert!(report.repacked);
    assert!(!fx.has_object(&dangling));
    assert!(fx.has_object(&main));
    fx.fsck();
    drop(fx);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn every_fault_point_leaves_a_valid_repository_that_a_restart_finishes() {
    for point in [
        FaultPoint::BeforeRepack,
        FaultPoint::AfterRepack,
        FaultPoint::DuringPrune,
        FaultPoint::BeforeVerification,
    ] {
        let fx = Fixture::new();
        let main = fx.commit("refs/heads/main", None, "one");
        let feature = fx.commit("refs/heads/feature", Some(&main), "two");
        let mut doomed = Vec::new();
        for i in 0..6 {
            doomed.push(fx.dangling_commit(&format!("doomed-{i}")));
        }
        fx.age_objects(HOUR * 2);

        let interrupted = plan(HOUR).with_faults(Faults {
            crash_at: Some(point),
            ..Faults::default()
        });
        let error = run(&fx.repo, &interrupted).unwrap_err();
        match &error {
            StorageError::Injected(name) => assert_eq!(*name, point.as_str()),
            other => panic!("{point:?}: expected an injected fault, got {other}"),
        }
        assert!(error.to_string().contains(point.as_str()));
        // Whatever the fault point, the reachable objects are intact and Git
        // accepts the repository.
        assert!(fx.has_object(&main) && fx.has_object(&feature), "{point:?}");
        fx.fsck();
        assert!(fx.clone_and_list().status.success(), "{point:?}");

        // A restarted run finishes the work.
        let report = expect_ok(&fx.repo, &plan(HOUR));
        assert!(report.repacked && report.pruned, "{point:?}");
        assert!(fx.has_object(&main) && fx.has_object(&feature), "{point:?}");
        for oid in &doomed {
            assert!(!fx.has_object(oid), "{point:?}: {oid} should be pruned");
        }
        fx.fsck();
        assert!(fx.clone_and_list().status.success(), "{point:?}");
    }
}

#[test]
fn a_ref_reaching_an_old_object_keeps_it_past_the_grace_period() {
    // The writers GitHearth runs all hold the repository lock (ADR 0013), so
    // this exercises the rule underneath it: a ref that reaches an old
    // object keeps it, because the deletion set is computed from
    // reachability, not age alone.
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let late = fx.dangling_commit("late");
    fx.age_objects(HOUR * 2);
    // The late object is old, but a ref now reaches it: reachability wins
    // over age.
    fx.git_ok(&["update-ref", "refs/heads/late", &late]);
    expect_ok(&fx.repo, &plan(HOUR));
    assert!(fx.has_object(&late));
    assert!(fx.has_object(&main));
    fx.fsck();
}

#[test]
fn a_missing_keep_point_keeps_nothing_and_a_missing_repository_errors() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    // A candidate that was already collected (or never existed) is not an
    // error: it simply pins nothing.
    let report = expect_ok(
        &fx.repo,
        &plan(HOUR).with_keep(KeepPoints {
            commits: vec!["a".repeat(40)],
        }),
    );
    assert_eq!(report.kept_commits, 1);
    assert!(fx.has_object(&main));

    let missing = fx.dir.join("no-such-repository.git");
    let error = run(&missing, &plan(HOUR)).unwrap_err();
    assert!(!error.to_string().is_empty(), "{error}");
}

#[test]
fn a_repository_with_missing_objects_fails_instead_of_reporting_success() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "main-content");
    let blob = fx.git_ok(&["rev-parse", &format!("{main}:file-main-content")]);
    let path = fx.repo.join("objects").join(&blob[..2]).join(&blob[2..]);
    fs::remove_file(&path).unwrap();

    let error = run(&fx.repo, &plan(HOUR)).unwrap_err();
    assert!(
        error.to_string().contains("rev-list")
            || error.to_string().contains("repack")
            || error.to_string().contains("verification"),
        "the run must fail instead of recording a successful prune: {error}"
    );
}

#[test]
fn a_truncated_live_set_fails_before_anything_is_deleted() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "main");
    // A pinned candidate and an ordinary dangling commit, both old enough to
    // be deleted if the live set were believed.
    let pinned = fx.dangling_commit("pinned");
    let pinned_objects = git_objects_reachable(&fx.repo, &pinned);
    let doomed = fx.dangling_commit("doomed");
    fx.age_objects(HOUR * 2);

    let error = run(
        &fx.repo,
        &plan(HOUR)
            .with_keep(KeepPoints {
                commits: vec![pinned.clone()],
            })
            // Far smaller than this repository's `rev-list` output.
            .with_max_live_output(64),
    )
    .unwrap_err();
    assert!(error.to_string().contains("live-set cap"), "{error}");
    assert!(error.to_string().contains("Nothing was deleted"), "{error}");
    for oid in &pinned_objects {
        assert!(
            fx.has_object(oid),
            "a truncated live set must not delete the pin: {oid}"
        );
    }
    assert!(
        fx.has_object(&doomed),
        "a truncated live set must not drive any deletion"
    );
    assert!(fx.has_object(&main));
    fx.fsck();
    assert!(fx.clone_and_list().status.success());

    // With the real cap the same repository completes and prunes normally.
    let report = expect_ok(
        &fx.repo,
        &plan(HOUR).with_keep(KeepPoints {
            commits: vec![pinned.clone()],
        }),
    );
    assert!(report.repacked && report.pruned);
    assert!(!fx.has_object(&doomed), "the prune really ran");
    for oid in &pinned_objects {
        assert!(fx.has_object(oid), "the pin survives the prune: {oid}");
    }
    fx.fsck();
}

#[test]
fn a_repack_that_uses_the_whole_step_timeout_does_not_starve_the_scan() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let doomed = fx.dangling_commit("doomed");
    fx.age_objects(HOUR * 2);

    // The repack takes longer than the step timeout. The scan deadline is
    // created after the snapshot, so it must still have a full timeout.
    let report = expect_ok(
        &fx.repo,
        &Plan::new(HOUR)
            .with_timeout(Duration::from_secs(1))
            .with_faults(Faults {
                slow_repack: Some(Duration::from_millis(1200)),
                ..Faults::default()
            }),
    );
    assert!(report.repacked);
    assert!(
        !report.truncated && report.pruned,
        "a slow repack must not starve the scan: {report:?}"
    );
    assert!(!fx.has_object(&doomed));
    assert!(fx.has_object(&main));
    fx.fsck();
}

#[test]
fn a_scan_that_exhausts_its_deadline_reports_truncated_and_deletes_nothing() {
    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let doomed = fx.dangling_commit("doomed");
    fx.age_objects(HOUR * 2);

    // A scan with no time left stops cleanly and must say so: it may not
    // claim a prune it did not do.
    let report = expect_ok(
        &fx.repo,
        &plan(HOUR).with_faults(Faults {
            scan_budget: Some(Duration::ZERO),
            ..Faults::default()
        }),
    );
    assert!(report.repacked);
    assert!(report.truncated, "{report:?}");
    assert!(!report.pruned, "a starved scan must not claim a prune");
    assert_eq!(report.expired, 0);
    assert!(
        fx.has_object(&doomed),
        "a starved scan must not delete anything"
    );
    assert!(fx.has_object(&main));

    // The next run has time and finishes the work.
    let report = expect_ok(&fx.repo, &plan(HOUR));
    assert!(report.pruned && !report.truncated, "{report:?}");
    assert!(!fx.has_object(&doomed));
    fx.fsck();
}

#[cfg(unix)]
#[test]
fn a_failed_unlink_fails_the_run_and_keeps_the_object() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let main = fx.commit("refs/heads/main", None, "one");
    let doomed = fx.dangling_commit("doomed");
    fx.age_objects(HOUR * 2);

    // A read-only fan-out directory: the scan still reads it, the unlink
    // cannot remove the object inside it.
    let parent = fx.repo.join("objects").join(&doomed[..2]);
    let original = fs::metadata(&parent).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_mode(0o555);
    fs::set_permissions(&parent, readonly).unwrap();

    let error = run(&fx.repo, &plan(HOUR)).unwrap_err();
    fs::set_permissions(&parent, original).unwrap();
    assert!(
        error.to_string().contains("remove loose object"),
        "the failure is reported, not discarded: {error}"
    );
    assert!(
        fx.has_object(&doomed),
        "the object whose unlink failed is still there"
    );
    assert!(fx.has_object(&main));

    // With the directory writable again the next run removes it.
    let report = expect_ok(&fx.repo, &plan(HOUR));
    assert!(report.pruned, "{report:?}");
    assert!(!fx.has_object(&doomed));
    fx.fsck();
}

#[test]
fn plan_builders_and_display_are_stable() {
    let plan = Plan::new(HOUR)
        .with_timeout(Duration::from_secs(5))
        .with_keep(KeepPoints::default())
        .with_faults(Faults::default());
    assert_eq!(plan.grace, HOUR);
    assert!(plan.keep.is_empty());
    assert!(KeepPoints::default().is_empty());
    for point in [
        FaultPoint::BeforeRepack,
        FaultPoint::AfterRepack,
        FaultPoint::DuringPrune,
        FaultPoint::BeforeVerification,
    ] {
        assert!(!point.as_str().is_empty());
    }
    for deferred in [
        Deferred::Quarantine {
            directory: PathBuf::from("/tmp/x"),
        },
        Deferred::JournalNotReplayed {
            reason: "torn".into(),
        },
    ] {
        assert!(!deferred.to_string().is_empty());
    }
    let error = StorageError::Deferred(Deferred::JournalNotReplayed {
        reason: "torn".into(),
    });
    assert!(error.to_string().contains("not reconciled"));
}
