//! Scheduled maintenance of one repository: repack, prune unreachable objects
//! after a grace period, and verify the result (ADR 0013).
//!
//! The functions here work on a single Git directory and never take the
//! per-repository lock themselves: the caller (the housekeeping job) holds it
//! for the whole run, so pushes, merges and fork creation are excluded. What
//! is live, and how concurrent writers are protected, is specified in ADR
//! 0013 "What is live"; [`KeepPoints`] is that definition expressed as data.
//!
//! # What a run does, and why each step is safe to interrupt
//!
//! 1. **Resolve the keep points.** Pinned commits and kept branches become
//!    object ids once, before anything moves; everything after works on ids,
//!    so a branch deleted between two steps cannot change the decision.
//! 2. **Compute the live set.** `git rev-list --objects --all` plus the keep
//!    points. Anything a ref reaches is live; anything a keep point reaches is
//!    live. Nothing else is.
//! 3. **Snapshot** (`git repack -A -d`, not `-a -d`). `-A` *loosens* an
//!    unreachable object that lives only in a pack: it moves it out of the old
//!    pack into a fresh loose file, which restarts its grace clock, and then
//!    packs the reachable history. `-a -d` would instead drop every unreachable
//!    packed object immediately, bypassing the grace period entirely, so the
//!    code must keep `-A`. Unreachable loose objects are never touched by
//!    `repack`; their age is their own. A crash leaves either the old packs,
//!    the new pack, or both: all valid.
//! 4. **Delete** the loose objects that are neither live nor older than the
//!    grace period. Each deletion is one `unlink`, the same operation
//!    `git prune` performs; a crash mid-way leaves some deleted and the rest
//!    for the next run. The scan and the deletion share one deadline created
//!    **after** the snapshot ([`Plan::timeout`]), so a slow repack cannot
//!    starve them; if it expires the run stops cleanly, leaves the rest for
//!    the next run and records a partial result. A failed `unlink` fails the
//!    run instead of being counted as a prune.
//! 5. **Verify** (`git fsck --full` and a `rev-list` walk of every ref). A run
//!    that stops before this point records nothing and is simply run again.
//!
//! The live set is re-read after the snapshot, so a ref moved by a writer that
//! does not take the lock (an outside `git`) narrows nothing that now exists.
//! A live-set walk larger than [`Plan::max_live_output`] fails the run before
//! anything is deleted: a partial set is never used for the deletion pass.

use crate::{StorageError, browse, git};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Where a run was interrupted. The value is never stored outside a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    /// Before the snapshot (`repack`) starts.
    BeforeRepack,
    /// After the snapshot, before the deletion pass.
    AfterRepack,
    /// During the deletion pass: half the doomed objects removed, the rest
    /// not.
    DuringPrune,
    /// After the deletion pass, before the verification.
    BeforeVerification,
}

impl FaultPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforeRepack => "before_repack",
            Self::AfterRepack => "after_repack",
            Self::DuringPrune => "during_prune",
            Self::BeforeVerification => "before_verification",
        }
    }
}

/// Largest `rev-list --objects --all` output a run will accept. A larger walk
/// is refused rather than truncated (see [`reachable_set`]): deleting against
/// a partial live set could remove a merge candidate or a pinned
/// pull-request head. The value matches the default of
/// `housekeeping_max_live_set_bytes`.
pub const MAX_LIVE_OUTPUT_BYTES: usize = 1 << 26;
/// Largest `cat-file --batch-check` output used to resolve keep points. This
/// bounds the number of live pins a run can resolve in one call.
pub const MAX_PRESENT_OUTPUT_BYTES: usize = 1 << 22;

/// Interruption points for a run. `crash_at` stops the run there by returning
/// [`StorageError::Injected`]; nothing after the point happens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    pub crash_at: Option<FaultPoint>,
    /// Test-only delay applied after the before-repack fault check. It makes a
    /// run take longer than a job's generic timeout without being a Git step.
    pub slow: Option<Duration>,
    /// Test-only delay applied after the size walk and before the snapshot.
    /// It lets a test prove that a repack using the whole step timeout no
    /// longer starves the scan (the scan has its own deadline).
    pub slow_repack: Option<Duration>,
    /// Test-only replacement for the scan/deletion deadline. A zero value
    /// makes the scan stop immediately, so a test can assert the run reports
    /// `truncated = true, pruned = false` instead of claiming a prune.
    pub scan_budget: Option<Duration>,
}

fn crash_at(faults: Faults, point: FaultPoint) -> Result<(), StorageError> {
    if faults.crash_at == Some(point) {
        Err(StorageError::Injected(point.as_str()))
    } else {
        Ok(())
    }
}

/// The objects a run may not delete even though no ref reaches them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeepPoints {
    /// Full commit ids; everything they reach is kept too. Merge candidates
    /// whose intent has not reached its ref update yet: the candidate commit
    /// is the result commit, whose tree and parents pin the represented
    /// source and target history.
    pub commits: Vec<String>,
}

impl KeepPoints {
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }
}

/// Why a run did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deferred {
    /// A live `receive-pack` quarantine directory exists. Refused, not
    /// cleaned: the pack in it is `receive-pack`'s to accept or discard. A
    /// quarantine older than [`Plan::quarantine_max_age`] is debris from a
    /// killed process and is removed instead.
    Quarantine { directory: PathBuf },
    /// The journal could not be replayed, and its records may still need
    /// objects this run would delete.
    JournalNotReplayed { reason: String },
    /// The repository borrows objects from an alternate object store, so its
    /// live set is not computable here. GitHearth never creates one (ADR 0008
    /// copies forks fully); an operator who adds one must maintain it by hand.
    Alternates { path: PathBuf },
}

impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quarantine { directory } => write!(
                f,
                "a push quarantine is present ({}); deferred",
                directory.display()
            ),
            Self::JournalNotReplayed { reason } => {
                write!(f, "the push journal is not reconciled ({reason}); deferred")
            }
            Self::Alternates { path } => write!(
                f,
                "the repository borrows objects through {}; deferred",
                path.display()
            ),
        }
    }
}

/// What one run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Size of `objects/` before the deletion pass.
    pub bytes_before: u64,
    /// Size of `objects/` after the run.
    pub bytes_after: u64,
    /// Loose objects past the threshold that were found before the deletion
    /// pass.
    pub expired: usize,
    /// Keep points pinned for the run.
    pub kept_commits: usize,
    /// Whether the snapshot ran.
    pub repacked: bool,
    /// Whether the deletion pass ran to completion. False when the scan or the
    /// deletion pass stopped at its deadline, leaving work for the next run.
    pub pruned: bool,
    /// Whether the scan or deletion pass hit its deadline.
    pub truncated: bool,
}

impl Report {
    /// Bytes the deletion pass freed (never negative).
    pub fn freed_bytes(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// One run's inputs.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Minimum age of an unreachable object before it may be deleted. The
    /// age is the object file's modification time, so it is per object and
    /// never needs a clock the caller controls.
    pub grace: Duration,
    /// Objects no ref reaches that are still live.
    pub keep: KeepPoints,
    pub faults: Faults,
    /// Bounds every Git step of the run, and its scan and deletion pass.
    pub timeout: Duration,
    /// A `tmp_objdir-incoming-*` directory older than this is treated as the
    /// debris of a killed `receive-pack` and removed. A live push cannot
    /// outlast the push deadline, so callers set this above it.
    pub quarantine_max_age: Duration,
    /// Largest live-set walk output accepted. Exceeding it fails the run
    /// before anything is deleted; it is never parsed as a partial set.
    pub max_live_output: usize,
}

impl Plan {
    /// A plan with `grace` of protection, no keep points, no faults, a
    /// ten-minute step timeout and a one-hour stale-quarantine threshold.
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            keep: KeepPoints::default(),
            faults: Faults::default(),
            timeout: Duration::from_secs(600),
            quarantine_max_age: Duration::from_secs(3600),
            max_live_output: MAX_LIVE_OUTPUT_BYTES,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_keep(mut self, keep: KeepPoints) -> Self {
        self.keep = keep;
        self
    }

    pub fn with_faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    pub fn with_quarantine_max_age(mut self, age: Duration) -> Self {
        self.quarantine_max_age = age;
        self
    }

    /// Override the live-set walk cap. Production uses
    /// `housekeeping_max_live_set_bytes`; tests use a small value to prove a
    /// truncated walk fails closed.
    pub fn with_max_live_output(mut self, bytes: usize) -> Self {
        self.max_live_output = bytes;
        self
    }
}

/// Object directories `receive-pack` is still using. Git creates them as
/// `objects/tmp_objdir-incoming-XXXXXX` (`tmp-objdir.c`) and removes them on a
/// normal exit; there is no lock file inside. GitHearth's own pushes hold the
/// repository lock for the whole request, so this only sees an external
/// `git receive-pack`, or the debris of one that was killed.
pub fn push_quarantines(repo: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let objects = repo.join("objects");
    let entries = match fs::read_dir(&objects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("tmp_objdir-incoming-")
            && fs::symlink_metadata(entry.path())?.file_type().is_dir()
        {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// The newest modification time anywhere under `path`, used to tell a push
/// still writing from the debris of a killed one. A missing path reads as
/// "now", so it is never treated as stale.
fn newest_modified(path: &Path) -> SystemTime {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return SystemTime::now();
    };
    let own = metadata.modified().unwrap_or(SystemTime::now());
    if !metadata.is_dir() {
        return own;
    }
    // A directory's own mtime only changes when an entry is created, not when
    // an existing object file is written, so the files decide staleness. Only
    // an empty directory falls back to its own time.
    let Ok(entries) = fs::read_dir(path) else {
        return own;
    };
    let mut newest: Option<SystemTime> = None;
    for entry in entries.flatten() {
        let child = newest_modified(&entry.path());
        newest = Some(newest.map_or(child, |current| current.max(child)));
    }
    newest.unwrap_or(own)
}

fn quarantine_is_stale(path: &Path, max_age: Duration) -> bool {
    SystemTime::now()
        .duration_since(newest_modified(path))
        .is_ok_and(|age| age >= max_age)
}

/// The repository's `objects/info/alternates`, when it names another object
/// store. A non-empty file means the live set cannot be computed from this
/// directory alone, so the run refuses instead of guessing.
fn alternates(repo: &Path) -> Result<Option<PathBuf>, StorageError> {
    let path = repo.join("objects").join("info").join("alternates");
    match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => Ok(Some(path)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Size of `<repo>/objects`: the number the push quota hook compares. The
/// walk is bounded by `timeout` so a huge object directory cannot hold the
/// repository lock past the documented step bound; exceeding it fails the run
/// rather than recording a partial size as if it were the whole.
pub fn objects_size(repo: &Path, timeout: Duration) -> Result<u64, StorageError> {
    let deadline = Instant::now() + timeout;
    dir_size_within(&repo.join("objects"), deadline)
}

fn dir_size_within(dir: &Path, deadline: Instant) -> Result<u64, StorageError> {
    if Instant::now() >= deadline {
        return Err(StorageError::Timeout);
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut total = 0u64;
    for entry in entries {
        if Instant::now() >= deadline {
            return Err(StorageError::Timeout);
        }
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total = total.saturating_add(dir_size_within(&entry.path(), deadline)?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// Run one maintenance of the repository at `repo`. The caller holds the
/// per-repository lock, has replayed the journal, and has decided the keep
/// points. Every Git step is bounded by [`Plan::timeout`].
pub fn run(repo: &Path, plan: &Plan) -> Result<Report, StorageError> {
    for quarantine in push_quarantines(repo)? {
        if quarantine_is_stale(&quarantine, plan.quarantine_max_age) {
            // A killed `receive-pack` never migrated its pack into `objects/`,
            // so this directory holds nothing the repository owns. Removing it
            // is the recovery the deferred path needs; without it every later
            // run would defer forever.
            fs::remove_dir_all(&quarantine)?;
            continue;
        }
        return Err(StorageError::Deferred(Deferred::Quarantine {
            directory: quarantine,
        }));
    }
    if let Some(path) = alternates(repo)? {
        return Err(StorageError::Deferred(Deferred::Alternates { path }));
    }
    let threshold = plan.grace;
    let mut report = Report {
        kept_commits: plan.keep.commits.len(),
        ..Report::default()
    };

    // 1-2. Resolve the keep points and the live set once, before anything
    // moves. The walk has its own deadline; a truncated walk fails the run
    // instead of being parsed as a partial live set.
    let keep = resolve_keep_points(plan)?;
    let live_deadline = Instant::now() + plan.timeout;
    let mut live = reachable_set(repo, &keep, live_deadline, plan.max_live_output)?;

    crash_at(plan.faults, FaultPoint::BeforeRepack)?;
    if let Some(delay) = plan.faults.slow {
        std::thread::sleep(delay);
    }

    // 2. The size walk before the snapshot is its own bounded phase.
    let size_deadline = Instant::now() + plan.timeout;
    report.bytes_before = dir_size_within(&repo.join("objects"), size_deadline)?;

    // 3. Snapshot. `-A` (not `-a`) is required: it loosens an unreachable
    //    object that lives only in a pack into a loose file, then packs
    //    reachable history. `-a -d` would delete those objects immediately.
    //    Git copies the pack's modification time onto the loosened object, so
    //    a year-old pack would give a just-unreachable object a year-old age;
    //    touch the packs first so loosening starts the grace clock now (packs
    //    are not measured by age, only loose objects are). `repack` writes each
    //    pack under a temporary name and renames it into place, so a killed run
    //    leaves the old packs, the new pack, or both: all valid.
    freshen_packs(repo)?;
    if let Some(delay) = plan.faults.slow_repack {
        std::thread::sleep(delay);
    }
    repack(repo, plan)?;
    report.repacked = true;

    // A writer outside the lock may have moved a ref while `repack` ran; the
    // deletion pass must not act on a stale live set. This walk is its own
    // bounded phase.
    let live_deadline = Instant::now() + plan.timeout;
    live.extend(reachable_set(
        repo,
        &keep,
        live_deadline,
        plan.max_live_output,
    )?);

    crash_at(plan.faults, FaultPoint::AfterRepack)?;

    // 4. Deletion pass. Its deadline starts **after** the snapshot, so a
    //    repack that uses its whole step timeout cannot starve the scan. The
    //    scan and the unlinking share this deadline; the rest is left for the
    //    next run.
    let delete_deadline = Instant::now() + plan.faults.scan_budget.unwrap_or(plan.timeout);
    let (doomed, scan_complete) = doomed_objects(repo, &live, threshold, delete_deadline)?;
    report.expired = doomed.len();
    let mut delete_complete = true;
    let mut remove_failure: Option<StorageError> = None;
    if plan.faults.crash_at == Some(FaultPoint::DuringPrune) {
        for oid in &doomed[..doomed.len() / 2] {
            remove_loose(repo, oid)?;
        }
        crash_at(plan.faults, FaultPoint::DuringPrune)?;
    } else {
        for oid in &doomed {
            if Instant::now() >= delete_deadline {
                delete_complete = false;
                break;
            }
            if let Err(error) = remove_loose(repo, oid) {
                // A failed unlink leaves the object in place. Report the run
                // as failed so the next one retries instead of claiming a
                // prune that did not happen.
                remove_failure.get_or_insert_with(|| {
                    StorageError::Io(std::io::Error::other(format!(
                        "remove loose object {oid}: {error}"
                    )))
                });
            }
        }
    }
    if !scan_complete || !delete_complete {
        report.truncated = true;
        report.pruned = false;
    } else {
        report.pruned = true;
    }
    if let Some(error) = remove_failure {
        return Err(error);
    }

    crash_at(plan.faults, FaultPoint::BeforeVerification)?;

    // 5. Verification: `fsck` and the final size walk share one step timeout.
    let verify_deadline = Instant::now() + plan.timeout;
    verify(repo, verify_deadline)?;
    report.bytes_after = dir_size_within(&repo.join("objects"), verify_deadline)?;
    Ok(report)
}

/// Validate the keep points. They are already object ids, so nothing has to
/// be resolved against a moving ref: a branch deleted between the caller's
/// decision and this run cannot change the set.
fn resolve_keep_points(plan: &Plan) -> Result<Vec<String>, StorageError> {
    for oid in &plan.keep.commits {
        if !valid_oid(oid) {
            return Err(StorageError::Invalid(
                "keep point is not a full object id".into(),
            ));
        }
    }
    Ok(plan.keep.commits.clone())
}

/// Everything every ref and the `keep` commits reach. A keep commit that no
/// longer exists (already collected, or a candidate the writer discarded)
/// keeps nothing; every other failure is an error.
///
/// The walk is bounded by `deadline` **and** by `max_output`. A walk that
/// produced more than the cap is refused before its output is parsed: a
/// partial live set would make [`doomed_objects`] delete objects that a merge
/// candidate or a pinned pull-request head still needs.
fn reachable_set(
    repo: &Path,
    keep: &[String],
    deadline: Instant,
    max_output: usize,
) -> Result<HashSet<String>, StorageError> {
    let present = present_objects(repo, keep, deadline)?;
    let mut command = git();
    command
        .arg("--git-dir")
        .arg(repo)
        .args(["rev-list", "--objects", "--all"])
        .args(&present);
    let output = browse::run_bounded(command, max_output, remaining(deadline)?)?;
    if output.truncated {
        return Err(StorageError::Git(format!(
            "rev-list --objects produced more than the {max_output}-byte live-set cap; \
             refusing to prune against a partial live set. Nothing was deleted. \
             Raise housekeeping_max_live_set_bytes above {max_output} and retry."
        )));
    }
    if !output.success {
        return Err(StorageError::Git(format!(
            "rev-list: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(parse_oids(&output.stdout))
}

/// The subset of `oids` that exists in the repository, in one `cat-file`
/// round trip. Its output is bounded too: a truncated answer would silently
/// drop keep points from the live-set walk.
fn present_objects(
    repo: &Path,
    oids: &[String],
    deadline: Instant,
) -> Result<Vec<String>, StorageError> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = git();
    command
        .arg("--git-dir")
        .arg(repo)
        .args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]);
    let input = oids.join("\n") + "\n";
    let output = browse::run_bounded_input(
        command,
        input.as_bytes(),
        MAX_PRESENT_OUTPUT_BYTES,
        remaining(deadline)?,
    )?;
    if output.truncated {
        return Err(StorageError::Git(format!(
            "cat-file produced more than the {MAX_PRESENT_OUTPUT_BYTES}-byte keep-point cap; \
             refusing to prune without resolving every keep point. Nothing was deleted. \
             Reduce the number of live pins and retry."
        )));
    }
    if !output.success {
        return Err(StorageError::Git(format!(
            "cat-file: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut present = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split(' ');
        let (Some(oid), Some(kind)) = (fields.next(), fields.next()) else {
            continue;
        };
        if kind != "missing" && valid_oid(oid) {
            present.push(oid.to_owned());
        }
    }
    Ok(present)
}

fn parse_oids(stdout: &[u8]) -> HashSet<String> {
    stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| {
            let oid = line.split(|b| *b == b' ').next()?;
            std::str::from_utf8(oid).ok().filter(|oid| valid_oid(oid))
        })
        .map(str::to_owned)
        .collect()
}

/// Set every pack file's modification time to now. `git repack -A` copies the
/// pack's time onto the loose object it creates, so without this a
/// just-unreachable object from an old pack would look older than the grace
/// period and be deleted immediately. Only loose objects are measured by age,
/// so a pack's own time has no other meaning.
fn freshen_packs(repo: &Path) -> Result<(), StorageError> {
    let pack_dir = repo.join("objects").join("pack");
    let entries = match fs::read_dir(&pack_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let now = fs::FileTimes::new().set_modified(SystemTime::now());
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "pack") {
            continue;
        }
        // A pack that cannot be touched is not fatal: its objects keep their
        // old time, which only means the next run may collect them earlier.
        let _ = fs::File::open(&path).and_then(|file| file.set_times(now));
    }
    Ok(())
}

/// `git repack -A -d`: reachable history into one pack, indexed by a
/// multi-pack index. `-A` loosens an unreachable object that lives only in a
/// pack into a loose file; `-a -d` would delete those objects immediately.
/// Never deletes an object a ref or a keep point reaches.
fn repack(repo: &Path, plan: &Plan) -> Result<(), StorageError> {
    let mut command = git();
    command
        .arg("--git-dir")
        .arg(repo)
        .args(["repack", "-A", "-d", "-q", "--write-midx"]);
    expect_success(command, plan.timeout, "repack")
}

/// The loose objects the deletion pass removes: not live, and at least
/// `threshold` old. A broken object (unreadable header) is never a candidate.
/// The second value is false when the deadline stopped the scan early.
fn doomed_objects(
    repo: &Path,
    live: &HashSet<String>,
    threshold: Duration,
    deadline: Instant,
) -> Result<(Vec<String>, bool), StorageError> {
    let (loose, complete) = loose_objects(repo, deadline)?;
    let mut doomed = Vec::new();
    for oid in loose {
        if Instant::now() >= deadline {
            return Ok((sorted(doomed), false));
        }
        if live.contains(&oid) || !older_than(repo, &oid, threshold) {
            continue;
        }
        doomed.push(oid);
    }
    Ok((sorted(doomed), complete))
}

fn sorted(mut oids: Vec<String>) -> Vec<String> {
    oids.sort();
    oids
}

fn loose_objects(repo: &Path, deadline: Instant) -> Result<(Vec<String>, bool), StorageError> {
    let objects = repo.join("objects");
    let mut found = Vec::new();
    let entries = match fs::read_dir(&objects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((found, true)),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        if Instant::now() >= deadline {
            return Ok((found, false));
        }
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if !fs::symlink_metadata(entry.path())?.file_type().is_dir() {
            continue;
        }
        for object in fs::read_dir(entry.path())? {
            if Instant::now() >= deadline {
                return Ok((found, false));
            }
            let object = object?;
            let tail = object.file_name();
            let tail = tail.to_string_lossy();
            if !valid_oid_tail(&tail) {
                continue;
            }
            // A symlink is never an object file; never follow it either.
            if !fs::symlink_metadata(object.path())?.file_type().is_file() {
                continue;
            }
            found.push(format!("{name}{tail}"));
        }
    }
    Ok((found, true))
}

fn valid_oid_tail(tail: &str) -> bool {
    (tail.len() == 38 || tail.len() == 62) && tail.bytes().all(|b| b.is_ascii_hexdigit())
}

fn older_than(repo: &Path, oid: &str, threshold: Duration) -> bool {
    let Ok(metadata) = fs::symlink_metadata(loose_path(repo, oid)) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= threshold)
}

/// Delete one loose object file; an already absent file is not an error
/// (another run or a concurrent writer removed it). Any other failure is
/// returned so the caller can report it instead of claiming a prune.
fn remove_loose(repo: &Path, oid: &str) -> Result<bool, StorageError> {
    let path = loose_path(repo, oid);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                // An emptied fan-out directory is removed too, as prune does.
                let _ = fs::remove_dir(parent);
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn loose_path(repo: &Path, oid: &str) -> PathBuf {
    repo.join("objects").join(&oid[..2]).join(&oid[2..])
}

/// `git fsck` on the whole graph plus a walk of every ref: the walk fails if
/// any object a ref reaches is missing. Runs on every completed run, so a run
/// that never got here simply runs again.
pub fn verify(repo: &Path, deadline: Instant) -> Result<(), StorageError> {
    let mut fsck = git();
    fsck.arg("--git-dir")
        .arg(repo)
        .args(["fsck", "--full", "--no-progress", "--no-dangling"]);
    expect_success(fsck, remaining(deadline)?, "verification (fsck)")?;
    let mut walk = git();
    walk.arg("--git-dir")
        .arg(repo)
        .args(["rev-list", "--objects", "--all"])
        .arg("--quiet");
    expect_success(walk, remaining(deadline)?, "verification (rev-list)")
}

/// Time left in a phase, or [`StorageError::Timeout`] once it has expired.
fn remaining(deadline: Instant) -> Result<Duration, StorageError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(StorageError::Timeout)
}

fn expect_success(
    command: std::process::Command,
    timeout: Duration,
    step: &str,
) -> Result<(), StorageError> {
    let output = browse::run_bounded(command, 1 << 20, timeout)?;
    if output.success {
        Ok(())
    } else {
        Err(StorageError::Git(format!(
            "{step}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub(crate) fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
