//! Merge candidate construction without a work tree (ADR 0004).
//!
//! Builds the result commit (or conflict/refusal outcome) for the four merge
//! methods using `git merge-tree --write-tree` and `git commit-tree`, writing
//! objects into the repository's object store but never moving a ref.

use crate::StorageError;
use crate::browse::{
    MAX_OUTPUT_BYTES, MergeTree, cmd, nul_fields, parse_merge_tree, run_bounded, run_bounded_input,
    stderr_error, utf8,
};
use crate::journal::Origin;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

/// Most commits a rebase will replay before giving up.
pub const MAX_REBASE_COMMITS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Merge,
    Squash,
    Rebase,
    FastForward,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub offset: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    pub target: &'a str,
    pub source: &'a str,
    pub message: &'a str,
    pub author: &'a Identity,
    pub committer: &'a Identity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Candidate {
    Ready { oid: String },
    Conflicts(Vec<String>),
    UpToDate,
    NotFastForward,
    Unrelated,
    RebaseMergeCommit,
}

fn valid_oid(oid: &str) -> bool {
    (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn valid_identity(value: &str) -> bool {
    !value.bytes().any(|b| matches!(b, b'<' | b'>' | b'\n' | 0))
}

pub(crate) fn valid_offset(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && matches!(bytes[0], b'+' | b'-')
        && bytes[1..].iter().all(|b| b.is_ascii_digit())
}

/// Compare-and-swap a branch through `git update-ref --stdin`, journaled by the
/// `reference-transaction` hook exactly as a push would be (ADR 0004). The
/// `origin` tags the journal record (`Merge` or `Suggestion`) so replay routes
/// it to the right metadata handler.
#[allow(clippy::too_many_arguments)]
pub fn update_ref_expected(
    repo: &Path,
    hooks_dir: &Path,
    reference: &str,
    new_oid: &str,
    expected_old_oid: &str,
    operation: Uuid,
    actor: Uuid,
    origin: Origin,
    timeout: Duration,
) -> Result<(), StorageError> {
    // The name goes into `update-ref --stdin`, so whitespace or a newline
    // would inject a second command; git checks the remaining rules.
    if !reference
        .strip_prefix("refs/heads/")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(StorageError::Invalid(
            "merge target must be a branch".into(),
        ));
    }
    if !valid_oid(new_oid) || !valid_oid(expected_old_oid) {
        return Err(StorageError::Invalid(
            "merge object ids must be full commit ids".into(),
        ));
    }
    let mut command = cmd(repo);
    if !hooks_dir.as_os_str().is_empty() {
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", hooks_dir.display()));
    }
    command
        .arg("update-ref")
        .arg("--stdin")
        .env("GITHEARTH_OPERATION", operation.to_string())
        .env("GITHEARTH_ACTOR", actor.to_string());
    match origin {
        Origin::Merge => {
            command.env("GITHEARTH_MERGE", "1");
        }
        Origin::Suggestion => {
            command.env("GITHEARTH_SUGGESTION", "1");
        }
    }
    let input = format!("update {reference} {new_oid} {expected_old_oid}\n");
    let result = run_bounded_input(command, input.as_bytes(), MAX_OUTPUT_BYTES, timeout)?;
    if result.success {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.stderr);
    // "is at X but expected Y", or the branch was deleted meanwhile.
    if stderr.contains("cannot lock ref")
        && (stderr.contains("but expected") || stderr.contains("unable to resolve reference"))
    {
        return Err(StorageError::RefMoved(stderr.trim().to_owned()));
    }
    Err(stderr_error(&result.stderr))
}

pub fn build_candidate(
    repo: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Candidate, StorageError> {
    if !valid_oid(request.target) || !valid_oid(request.source) {
        return Err(StorageError::Invalid(
            "object ids must be full commit ids".into(),
        ));
    }
    for value in [
        &request.author.name,
        &request.author.email,
        &request.committer.name,
        &request.committer.email,
    ] {
        if !valid_identity(value) {
            return Err(StorageError::Invalid(
                "identity contains a forbidden character".into(),
            ));
        }
    }
    for offset in [&request.author.offset, &request.committer.offset] {
        if !valid_offset(offset) {
            return Err(StorageError::Invalid(
                "identity offset must be [+-]HHMM".into(),
            ));
        }
    }
    verify_commit(repo, request.target, timeout)?;
    verify_commit(repo, request.source, timeout)?;

    if is_ancestor(repo, request.source, request.target, timeout)? {
        return Ok(Candidate::UpToDate);
    }

    match request.method {
        Method::FastForward => {
            if is_ancestor(repo, request.target, request.source, timeout)? {
                Ok(Candidate::Ready {
                    oid: request.source.to_owned(),
                })
            } else {
                Ok(Candidate::NotFastForward)
            }
        }
        Method::Merge | Method::Squash => {
            if merge_base(repo, request.target, request.source, timeout)?.is_none() {
                return Ok(Candidate::Unrelated);
            }
            let tree = match run_merge_tree(repo, None, request.target, request.source, timeout)? {
                MergeTree::Clean(tree) => tree,
                MergeTree::Conflicts(paths) => return Ok(Candidate::Conflicts(paths)),
            };
            let parents: &[&str] = match request.method {
                Method::Merge => &[request.target, request.source],
                _ => &[request.target],
            };
            let oid = commit_tree(
                repo,
                &tree,
                parents,
                request.message.as_bytes(),
                request.author,
                request.committer,
                timeout,
            )?;
            Ok(Candidate::Ready { oid })
        }
        Method::Rebase => {
            if merge_base(repo, request.target, request.source, timeout)?.is_none() {
                return Ok(Candidate::Unrelated);
            }
            rebase(repo, request, timeout)
        }
    }
}

fn verify_commit(repo: &Path, oid: &str, timeout: Duration) -> Result<(), StorageError> {
    let mut command = cmd(repo);
    command.args(["cat-file", "-e", &format!("{oid}^{{commit}}")]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if result.success {
        Ok(())
    } else {
        Err(StorageError::Invalid(format!("not a commit: {oid}")))
    }
}

fn is_ancestor(
    repo: &Path,
    ancestor: &str,
    descendant: &str,
    timeout: Duration,
) -> Result<bool, StorageError> {
    let mut command = cmd(repo);
    command.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    match result.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(stderr_error(&result.stderr)),
    }
}

fn merge_base(
    repo: &Path,
    a: &str,
    b: &str,
    timeout: Duration,
) -> Result<Option<String>, StorageError> {
    let mut command = cmd(repo);
    command.args(["merge-base", a, b]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    match result.code {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&result.stdout).trim().to_owned(),
        )),
        // Exit 1 with no output: the histories share no commit.
        Some(1) => Ok(None),
        _ => Err(stderr_error(&result.stderr)),
    }
}

fn run_merge_tree(
    repo: &Path,
    merge_base: Option<&str>,
    a: &str,
    b: &str,
    timeout: Duration,
) -> Result<MergeTree, StorageError> {
    let mut command = cmd(repo);
    command.args([
        "merge-tree",
        "--write-tree",
        "-z",
        "--name-only",
        "--no-messages",
    ]);
    if let Some(base) = merge_base {
        command.arg(format!("--merge-base={base}"));
    }
    command.arg(a).arg(b);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    parse_merge_tree(
        result.code,
        &result.stdout,
        &result.stderr,
        result.truncated,
    )
}

pub(crate) fn commit_tree(
    repo: &Path,
    tree: &str,
    parents: &[&str],
    message: &[u8],
    author: &Identity,
    committer: &Identity,
    timeout: Duration,
) -> Result<String, StorageError> {
    let mut command = cmd(repo);
    command.arg("commit-tree").arg(tree);
    for parent in parents {
        command.arg("-p").arg(parent);
    }
    command.arg("-F").arg("-");
    with_identities(&mut command, author, committer);
    let result = run_bounded_input(command, message, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    if result.truncated {
        return Err(StorageError::TooLarge {
            size: result.stdout.len() as u64,
            limit: MAX_OUTPUT_BYTES,
        });
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn with_identities(command: &mut Command, author: &Identity, committer: &Identity) {
    command
        .env("GIT_AUTHOR_NAME", &author.name)
        .env("GIT_AUTHOR_EMAIL", &author.email)
        .env(
            "GIT_AUTHOR_DATE",
            format!("@{} {}", author.time, author.offset),
        )
        .env("GIT_COMMITTER_NAME", &committer.name)
        .env("GIT_COMMITTER_EMAIL", &committer.email)
        .env(
            "GIT_COMMITTER_DATE",
            format!("@{} {}", committer.time, committer.offset),
        );
}

struct RebaseCommit {
    oid: String,
    parents: Vec<String>,
    author: Identity,
    message: Vec<u8>,
}

const REBASE_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%ad%x00%B";

fn parse_rebase_commits(bytes: &[u8]) -> Result<Vec<RebaseCommit>, StorageError> {
    let fields = nul_fields(bytes);
    if !fields.len().is_multiple_of(6) {
        return Err(StorageError::Invalid("unexpected git log output".into()));
    }
    let mut commits = Vec::with_capacity(fields.len() / 6);
    for chunk in fields.as_chunks::<6>().0 {
        let parents = utf8(chunk[1])?;
        let raw_date = utf8(chunk[4])?;
        let (time, offset) = raw_date
            .split_once(' ')
            .ok_or_else(|| StorageError::Invalid("invalid author date".into()))?;
        commits.push(RebaseCommit {
            oid: utf8(chunk[0])?,
            parents: if parents.is_empty() {
                Vec::new()
            } else {
                parents.split(' ').map(str::to_owned).collect()
            },
            author: Identity {
                name: utf8(chunk[2])?,
                email: utf8(chunk[3])?,
                time: time
                    .parse()
                    .map_err(|_| StorageError::Invalid("invalid author time".into()))?,
                offset: offset.to_owned(),
            },
            message: chunk[5].to_vec(),
        });
    }
    Ok(commits)
}

fn check_commit_count(count: usize) -> Result<(), StorageError> {
    if count > MAX_REBASE_COMMITS {
        Err(StorageError::TooLarge {
            size: count as u64,
            limit: MAX_REBASE_COMMITS,
        })
    } else {
        Ok(())
    }
}

/// Replay each source commit onto the target, keeping its author and message.
/// A commit whose result tree equals its new parent's is dropped, like
/// `git rebase` does for already-applied changes; this also drops commits that
/// were intentionally empty.
fn rebase(repo: &Path, request: &Request, timeout: Duration) -> Result<Candidate, StorageError> {
    let commits = list_rebase_commits(repo, request, timeout)?;
    check_commit_count(commits.len())?;

    let mut previous = request.target.to_owned();
    let mut produced = None;
    for commit in commits {
        if commit.parents.len() > 1 {
            return Ok(Candidate::RebaseMergeCommit);
        }
        let base = format!("{}^", commit.oid);
        let tree = match run_merge_tree(repo, Some(&base), &previous, &commit.oid, timeout)? {
            MergeTree::Clean(tree) => tree,
            MergeTree::Conflicts(paths) => return Ok(Candidate::Conflicts(paths)),
        };
        if tree == tree_of(repo, &previous, timeout)? {
            continue;
        }
        let oid = commit_tree(
            repo,
            &tree,
            &[&previous],
            &commit.message,
            &commit.author,
            request.committer,
            timeout,
        )?;
        previous = oid;
        produced = Some(previous.clone());
    }
    match produced {
        Some(oid) => Ok(Candidate::Ready { oid }),
        None => Ok(Candidate::UpToDate),
    }
}

fn list_rebase_commits(
    repo: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Vec<RebaseCommit>, StorageError> {
    let mut command = cmd(repo);
    let format = format!("--format={REBASE_FORMAT}");
    command.args([
        "log",
        "--reverse",
        "--topo-order",
        "-z",
        "--date=raw",
        &format,
    ]);
    command.arg(format!("{}..{}", request.target, request.source));
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    if result.truncated {
        return Err(StorageError::TooLarge {
            size: result.stdout.len() as u64,
            limit: MAX_OUTPUT_BYTES,
        });
    }
    parse_rebase_commits(&result.stdout)
}

fn tree_of(repo: &Path, oid: &str, timeout: Duration) -> Result<String, StorageError> {
    let mut command = cmd(repo);
    command.args(["rev-parse", "--verify", &format!("{oid}^{{tree}}")]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rebase_commits_rejects_partial_records_and_bad_dates() {
        assert!(matches!(
            parse_rebase_commits(b"a\0b\0c\0d\0e"),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            parse_rebase_commits(b"a\0\0n\0e\0not-a-date\0m\0"),
            Err(StorageError::Invalid(_))
        ));
    }

    #[test]
    fn git_errors_are_not_read_as_answers() {
        // Exit 128 (not a repository) must surface, not become "unrelated" or
        // "not an ancestor".
        let missing = std::env::temp_dir().join("githearth-merge-no-such-repo");
        let oid = "0".repeat(40);
        let timeout = Duration::from_secs(10);
        assert!(matches!(
            merge_base(&missing, &oid, &oid, timeout),
            Err(StorageError::Git(_))
        ));
        assert!(matches!(
            is_ancestor(&missing, &oid, &oid, timeout),
            Err(StorageError::Git(_))
        ));
        assert!(matches!(
            run_merge_tree(&missing, None, &oid, &oid, timeout),
            Err(StorageError::Git(_))
        ));
    }

    #[test]
    fn commit_count_bound_is_exclusive() {
        assert!(check_commit_count(MAX_REBASE_COMMITS).is_ok());
        assert!(matches!(
            check_commit_count(MAX_REBASE_COMMITS + 1),
            Err(StorageError::TooLarge { limit: 1000, .. })
        ));
    }
}
