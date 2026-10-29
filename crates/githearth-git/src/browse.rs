//! Read-only repository browsing: refs, trees, blobs, commits and diffs.
//!
//! Every Git process runs through the sanitized [`crate::git`] command with
//! its stdout read capped and the caller-supplied deadline enforced by
//! killing the child on overrun. Output is parsed from NUL-separated formats;
//! user-controlled paths and revisions are validated before Git sees them.

use crate::{StorageError, git};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Largest blob we will ever read into memory.
pub const MAX_BLOB_BYTES: usize = 1 << 20;
/// Largest diff patch we will emit.
pub const MAX_PATCH_BYTES: usize = 1 << 20;
/// Largest output of a metadata/listing command.
pub const MAX_OUTPUT_BYTES: usize = 1 << 20;
/// Most files reported per commit diff.
pub const MAX_DIFF_FILES: usize = 300;
/// Largest user-facing log page.
pub const MAX_LOG_LIMIT: usize = 100;

pub fn valid_rev(rev: &str) -> bool {
    !rev.is_empty()
        && !rev.starts_with('-')
        && !rev.contains("..")
        && !rev.contains(':')
        && !rev.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn cmd(repo: &Path) -> Command {
    let mut c = git();
    c.arg("--git-dir")
        .arg(repo)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    c
}

/// Read a child pipe fully, storing at most `cap + 1` bytes so the caller can
/// detect truncation while the child never blocks on a full pipe.
fn read_capped(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut buf = [0u8; 8192];
    let mut out = Vec::new();
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        let remaining = (cap + 1).saturating_sub(out.len());
        if remaining > 0 {
            out.extend_from_slice(&buf[..n.min(remaining)]);
        }
    }
    out
}

struct RunResult {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

fn run_bounded(
    mut command: Command,
    max_output: usize,
    timeout: Duration,
) -> Result<RunResult, StorageError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StorageError::Invalid("git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| StorageError::Invalid("git stderr unavailable".into()))?;
    let stdout_thread = std::thread::spawn(move || read_capped(stdout, max_output));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr, 64 * 1024));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(StorageError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| StorageError::Invalid("git reader panicked".into()))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| StorageError::Invalid("git reader panicked".into()))?;
    Ok(RunResult {
        success: status.success(),
        truncated: stdout.len() > max_output,
        stdout,
        stderr,
    })
}

/// Run a command that must succeed and fit within `max_output`.
fn run(command: Command, timeout: Duration, max_output: usize) -> Result<Vec<u8>, StorageError> {
    let result = run_bounded(command, max_output, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    if result.truncated {
        return Err(StorageError::TooLarge {
            size: result.stdout.len() as u64,
            limit: max_output,
        });
    }
    Ok(result.stdout)
}

/// NUL-separated fields, dropping only the final empty trailing field.
fn nul_fields(bytes: &[u8]) -> Vec<&[u8]> {
    let mut fields: Vec<&[u8]> = bytes.split(|&b| b == 0).collect();
    if fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    fields
}

fn utf8(bytes: &[u8]) -> Result<String, StorageError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| StorageError::Invalid("git output is not UTF-8".into()))
}

pub fn resolve(repo: &Path, rev: &str, timeout: Duration) -> Result<Option<String>, StorageError> {
    if !valid_rev(rev) {
        return Err(StorageError::Invalid("invalid revision".into()));
    }
    let mut command = cmd(repo);
    command.args(["rev-parse", "--verify", "--quiet", "--end-of-options"]);
    command.arg(format!("{rev}^{{commit}}"));
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&result.stdout);
    Ok(Some(id.trim().to_owned()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefInfo {
    pub name: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refs {
    pub default_branch: Option<String>,
    pub branches: Vec<RefInfo>,
    pub tags: Vec<RefInfo>,
}

pub fn refs(repo: &Path, timeout: Duration) -> Result<Refs, StorageError> {
    let mut command = cmd(repo);
    command.args([
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/heads",
        "refs/tags",
    ]);
    let out = run(command, timeout, MAX_OUTPUT_BYTES)?;

    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let (target, name) = line
            .trim()
            .split_once(' ')
            .ok_or_else(|| StorageError::Invalid("unexpected ref record".into()))?;
        let info = RefInfo {
            name: name
                .strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/tags/"))
                .unwrap_or(name)
                .to_owned(),
            target: target.to_owned(),
        };
        if name.starts_with("refs/heads/") {
            branches.push(info);
        } else if name.starts_with("refs/tags/") {
            tags.push(info);
        }
    }

    let mut command = cmd(repo);
    command.args(["symbolic-ref", "--quiet", "HEAD"]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    let default_branch = if result.success {
        let name = String::from_utf8_lossy(&result.stdout);
        name.trim().strip_prefix("refs/heads/").map(str::to_owned)
    } else {
        None
    };

    Ok(Refs {
        default_branch,
        branches,
        tags,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Blob,
    Tree,
    Commit,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub mode: String,
    pub id: String,
    pub size: Option<u64>,
}

pub fn tree(
    repo: &Path,
    commit: &str,
    path: Option<&str>,
    timeout: Duration,
) -> Result<Option<Vec<TreeEntry>>, StorageError> {
    tree_impl(repo, commit, path, timeout, MAX_OUTPUT_BYTES)
}

fn tree_impl(
    repo: &Path,
    commit: &str,
    path: Option<&str>,
    timeout: Duration,
    max_output: usize,
) -> Result<Option<Vec<TreeEntry>>, StorageError> {
    if let Some(path) = path
        && !valid_path(path)
    {
        return Err(StorageError::Invalid("invalid path".into()));
    }
    let mut command = cmd(repo);
    match path {
        None => {
            command.args(["ls-tree", "-z", "-l", commit]);
        }
        // List the tree object at `<commit>:<path>`, not the entry itself:
        // `ls-tree <commit> -- <path>` would return the directory entry.
        Some(path) => {
            command.args(["ls-tree", "-z", "-l"]);
            command.arg(format!("{commit}:{path}"));
        }
    }
    let result = run_bounded(command, max_output, timeout)?;
    // A path that does not exist or is not a tree makes ls-tree fail.
    if !result.success {
        return Ok(None);
    }
    if result.truncated {
        return Err(StorageError::TooLarge {
            size: result.stdout.len() as u64,
            limit: max_output,
        });
    }

    let mut entries = Vec::new();
    for record in nul_fields(&result.stdout) {
        let (meta, name) = record
            .iter()
            .position(|&b| b == b'\t')
            .map(|i| (&record[..i], &record[i + 1..]))
            .ok_or_else(|| StorageError::Invalid("unexpected ls-tree record".into()))?;
        let mut parts = std::str::from_utf8(meta)
            .map_err(|_| StorageError::Invalid("ls-tree record is not UTF-8".into()))?
            .split_whitespace();
        let mode = parts
            .next()
            .ok_or_else(|| StorageError::Invalid("ls-tree record has no mode".into()))?;
        let kind_bytes = parts
            .next()
            .ok_or_else(|| StorageError::Invalid("ls-tree record has no type".into()))?;
        let id = parts
            .next()
            .ok_or_else(|| StorageError::Invalid("ls-tree record has no id".into()))?;
        let size = parts
            .next()
            .ok_or_else(|| StorageError::Invalid("ls-tree record has no size".into()))?;
        let kind = match kind_bytes {
            "tree" => EntryKind::Tree,
            "commit" => EntryKind::Commit,
            _ if mode.starts_with("12") => EntryKind::Symlink,
            _ => EntryKind::Blob,
        };
        entries.push(TreeEntry {
            name: utf8(name)?,
            kind,
            mode: mode.to_owned(),
            id: id.to_owned(),
            size: if size == "-" {
                None
            } else {
                Some(
                    size.parse()
                        .map_err(|_| StorageError::Invalid("invalid ls-tree blob size".into()))?,
                )
            },
        });
    }
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.kind, EntryKind::Tree | EntryKind::Commit);
        let b_dir = matches!(b.kind, EntryKind::Tree | EntryKind::Commit);
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
    Ok(Some(entries))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub id: String,
    pub size: u64,
    pub binary: bool,
    pub truncated: bool,
    pub content: Vec<u8>,
}

pub fn blob(
    repo: &Path,
    commit: &str,
    path: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Option<Blob>, StorageError> {
    if !valid_path(path) {
        return Err(StorageError::Invalid("invalid path".into()));
    }
    let max_bytes = max_bytes.min(MAX_BLOB_BYTES);

    let mut command = cmd(repo);
    command.args(["rev-parse", "--verify", "--quiet", "--end-of-options"]);
    command.arg(format!("{commit}:{path}"));
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Ok(None);
    }
    let id = String::from_utf8_lossy(&result.stdout);
    let id = id.trim().to_owned();

    let mut command = cmd(repo);
    command.args(["cat-file", "-t", &id]);
    let kind = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    // A gitlink points at a commit object that need not exist in this
    // repository; a missing or non-blob object is "not found", not an error.
    if !kind.success || String::from_utf8_lossy(&kind.stdout).trim() != "blob" {
        return Ok(None);
    }

    let mut command = cmd(repo);
    command.args(["cat-file", "-s", &id]);
    let size_out = run(command, timeout, MAX_OUTPUT_BYTES)?;
    let size: u64 = String::from_utf8_lossy(&size_out)
        .trim()
        .parse()
        .map_err(|_| StorageError::Invalid("invalid blob size".into()))?;

    let mut command = cmd(repo);
    command.args(["cat-file", "blob", &id]);
    // The type and size were verified above, so this read cannot fail.
    let result = run_bounded(command, max_bytes, timeout)?;
    let mut content = result.stdout;
    let truncated = content.len() > max_bytes;
    if truncated {
        content.truncate(max_bytes);
    }
    let binary = content.contains(&0);
    Ok(Some(Blob {
        id,
        size,
        binary,
        truncated,
        content,
    }))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSummary {
    pub id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: i64,
    pub summary: String,
}

const LOG_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%s";

pub fn log(
    repo: &Path,
    commit: &str,
    path: Option<&str>,
    after: Option<u64>,
    count: usize,
    timeout: Duration,
) -> Result<Vec<CommitSummary>, StorageError> {
    if let Some(path) = path
        && !valid_path(path)
    {
        return Err(StorageError::Invalid("invalid path".into()));
    }
    let count = count.min(MAX_LOG_LIMIT + 1);
    let mut command = cmd(repo);
    command.args(["log", "-z", &format!("--format={LOG_FORMAT}")]);
    if let Some(after) = after {
        command.arg(format!("--skip={after}"));
    }
    command.arg(format!("--max-count={count}"));
    command.arg(commit);
    if let Some(path) = path {
        command.arg("--").arg(path);
    }
    let out = run(command, timeout, MAX_OUTPUT_BYTES)?;
    parse_commits(&out, true)
}

const DETAIL_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%B";

fn parse_commits(bytes: &[u8], summary: bool) -> Result<Vec<CommitSummary>, StorageError> {
    let fields = nul_fields(bytes);
    if !fields.len().is_multiple_of(9) {
        return Err(StorageError::Invalid("unexpected git log output".into()));
    }
    let mut commits = Vec::with_capacity(fields.len() / 9);
    for chunk in fields.as_chunks::<9>().0 {
        let parents = utf8(chunk[1])?;
        commits.push(CommitSummary {
            id: utf8(chunk[0])?,
            parents: if parents.is_empty() {
                Vec::new()
            } else {
                parents.split(' ').map(str::to_owned).collect()
            },
            author_name: utf8(chunk[2])?,
            author_email: utf8(chunk[3])?,
            author_time: utf8(chunk[4])?
                .parse()
                .map_err(|_| StorageError::Invalid("invalid author time".into()))?,
            committer_name: utf8(chunk[5])?,
            committer_email: utf8(chunk[6])?,
            committer_time: utf8(chunk[7])?
                .parse()
                .map_err(|_| StorageError::Invalid("invalid committer time".into()))?,
            summary: if summary {
                utf8(chunk[8])?
            } else {
                utf8(chunk[8])?.trim_end_matches('\n').to_owned()
            },
        });
    }
    Ok(commits)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub patch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: i64,
    pub message: String,
    pub truncated: bool,
    pub files: Vec<FileDiff>,
}

pub fn commit(
    repo: &Path,
    id: &str,
    timeout: Duration,
) -> Result<Option<CommitDetail>, StorageError> {
    commit_impl(repo, id, timeout, MAX_OUTPUT_BYTES)
}

fn commit_impl(
    repo: &Path,
    id: &str,
    timeout: Duration,
    max_output: usize,
) -> Result<Option<CommitDetail>, StorageError> {
    let mut command = cmd(repo);
    command.args(["log", "-1", "-z", &format!("--format={DETAIL_FORMAT}"), id]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Ok(None);
    }
    let summaries = parse_commits(&result.stdout, false)?;
    let summary = summaries
        .into_iter()
        .next()
        .expect("a successful `log -1` always yields one commit");

    let root = summary.parents.is_empty();
    let first_parent = summary.parents.first();

    // Status + paths for every changed file (rename-aware). A listing larger
    // than `max_output` is truncated: only complete records are kept and the
    // result is flagged, not failed.
    let mut command = cmd(repo);
    command.arg("-c").arg("core.quotePath=false");
    command.args([
        "diff-tree",
        "-r",
        "-M",
        "--name-status",
        "-z",
        "--no-commit-id",
    ]);
    if root {
        command.arg("--root").arg(id);
    } else {
        command
            .arg(first_parent.expect("non-root has a parent"))
            .arg(id);
    }
    let name_status = run_bounded(command, max_output, timeout)?;
    let mut truncated = name_status.truncated;
    let statuses = parse_name_status(&name_status.stdout, name_status.truncated)?;

    // Per-file addition/deletion counts (rename-aware).
    let mut command = cmd(repo);
    command.arg("-c").arg("core.quotePath=false");
    command.args(["diff-tree", "-r", "-M", "--numstat", "-z", "--no-commit-id"]);
    if root {
        command.arg("--root").arg(id);
    } else {
        command
            .arg(first_parent.expect("non-root has a parent"))
            .arg(id);
    }
    let numstat = run_bounded(command, max_output, timeout)?;
    truncated |= numstat.truncated;
    let counts = parse_numstat(&numstat.stdout, numstat.truncated)?;

    // Bounded unified patch text.
    let mut command = cmd(repo);
    command.arg("-c").arg("core.quotePath=false");
    command.args([
        "diff-tree",
        "-r",
        "-M",
        "-p",
        "--no-ext-diff",
        "--no-textconv",
        "--no-commit-id",
    ]);
    if root {
        command.arg("--root").arg(id);
    } else {
        command
            .arg(first_parent.expect("non-root has a parent"))
            .arg(id);
    }
    let patch = run_bounded(command, MAX_PATCH_BYTES, timeout)?;
    let patch_truncated = patch.truncated;
    let patch_text = String::from_utf8_lossy(&patch.stdout).into_owned();

    let mut stats: HashMap<(String, String), (u64, u64, bool)> = HashMap::new();
    for (old, new, additions, deletions, binary) in counts {
        stats.insert((old, new), (additions, deletions, binary));
    }

    let mut files: Vec<FileDiff> = Vec::new();
    for (status, old, new) in statuses {
        let (additions, deletions, binary) = stats
            .get(&(old.clone(), new.clone()))
            .copied()
            .unwrap_or((0, 0, false));
        files.push(FileDiff {
            old_path: old,
            new_path: new,
            status,
            additions,
            deletions,
            binary,
            patch: String::new(),
        });
    }

    truncated |= patch_truncated;
    if files.len() > MAX_DIFF_FILES {
        truncated = true;
        files.truncate(MAX_DIFF_FILES);
    }

    let chunks = split_patch(&patch_text);
    // A type change is one `T` name-status record but two `diff --git`
    // chunks (delete + add); every other record has exactly one. Assign by
    // that count rather than position so later files keep their own patch.
    let mut chunks = chunks.into_iter();
    for file in &mut files {
        let mut patch = String::new();
        let pieces = if file.status == "T" { 2 } else { 1 };
        for _ in 0..pieces {
            if let Some(chunk) = chunks.next() {
                patch.push_str(&chunk);
            }
        }
        file.patch = patch;
    }
    if files.iter().any(|f| f.patch.is_empty() && !f.binary) {
        truncated = true;
    }

    Ok(Some(CommitDetail {
        id: summary.id,
        parents: summary.parents,
        author_name: summary.author_name,
        author_email: summary.author_email,
        author_time: summary.author_time,
        committer_name: summary.committer_name,
        committer_email: summary.committer_email,
        committer_time: summary.committer_time,
        message: summary.summary,
        truncated,
        files,
    }))
}

fn stderr_error(stderr: &[u8]) -> StorageError {
    let message = String::from_utf8_lossy(stderr);
    let message = message.trim();
    StorageError::Git(if message.is_empty() {
        "git exited non-zero".into()
    } else {
        message.to_owned()
    })
}

type NameStatus = (String, String, String);
type Numstat = (String, String, u64, u64, bool);

fn parse_name_status(bytes: &[u8], truncated: bool) -> Result<Vec<NameStatus>, StorageError> {
    let tokens = nul_fields(bytes);
    let mut files = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = utf8(tokens[i])?;
        if status.starts_with('R') || status.starts_with('C') {
            let (Some(old), Some(new)) = (tokens.get(i + 1), tokens.get(i + 2)) else {
                if truncated {
                    break;
                }
                return Err(StorageError::Invalid("truncated rename record".into()));
            };
            files.push((status, utf8(old)?, utf8(new)?));
            i += 3;
        } else {
            let Some(path) = tokens.get(i + 1) else {
                if truncated {
                    break;
                }
                return Err(StorageError::Invalid("truncated status record".into()));
            };
            let path = utf8(path)?;
            files.push((status, path.clone(), path));
            i += 2;
        }
    }
    Ok(files)
}

fn parse_numstat(bytes: &[u8], truncated: bool) -> Result<Vec<Numstat>, StorageError> {
    let tokens = nul_fields(bytes);
    let mut files = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut parts = tokens[i].splitn(3, |&b| b == b'\t');
        // splitn always yields at least one field.
        let add = parts.next().expect("splitn yields a field");
        let del = parts.next();
        let rest = parts.next();
        let binary = add == b"-" && del == Some(b"-");
        let additions = if binary {
            0
        } else {
            utf8(add)?
                .parse()
                .map_err(|_| StorageError::Invalid("invalid numstat count".into()))?
        };
        let deletions = match del {
            Some(del) if del != b"-" => utf8(del)?
                .parse()
                .map_err(|_| StorageError::Invalid("invalid numstat count".into()))?,
            _ => 0,
        };
        match rest {
            Some(rest) if !rest.is_empty() => {
                let path = utf8(rest)?;
                files.push((path.clone(), path, additions, deletions, binary));
                i += 1;
            }
            _ => {
                let (Some(old), Some(new)) = (tokens.get(i + 1), tokens.get(i + 2)) else {
                    if truncated {
                        break;
                    }
                    return Err(StorageError::Invalid("truncated numstat rename".into()));
                };
                files.push((utf8(old)?, utf8(new)?, additions, deletions, binary));
                i += 3;
            }
        }
    }
    Ok(files)
}

fn split_patch(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run_git(args: &[&str]) -> String {
        let mut command = StdCommand::new("git");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .args(args);
        let out = command.output().expect("git");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn rev_validator_rejects_injection_and_ranges() {
        for bad in [
            "", "-", "--help", "a..b", "a:b", "a b", "a\nb", "a\tb", "\x01",
        ] {
            assert!(!valid_rev(bad), "{bad:?} should be rejected");
        }
        for ok in [
            "HEAD",
            "main",
            "refs/heads/main",
            "v1.0",
            "deadbeef",
            "HEAD~1",
            "main^{commit}",
        ] {
            assert!(valid_rev(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn path_validator_rejects_escape_and_empty_segments() {
        for bad in [
            "",
            "/",
            "/abs",
            "a/",
            "/a",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "a\0b",
            &"x".repeat(4097),
        ] {
            assert!(!valid_path(bad), "{bad:?} should be rejected");
        }
        for ok in ["a", "a/b", "a b", "ünïcode/文件", "a.b", "dir/file.txt"] {
            assert!(valid_path(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn split_patch_groups_files() {
        let text = "diff --git a/x b/x\n+x\ndiff --git a/y b/y\n+y\n";
        let chunks = split_patch(text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].starts_with("diff --git a/x b/x\n"));
        assert!(chunks[1].starts_with("diff --git a/y b/y\n"));
    }

    #[test]
    fn commit_truncates_oversized_diff_listing() {
        let dir = std::env::temp_dir().join(format!("browse-commit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(&["init", "-q", "-b", "main", d]);
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), "x\n").unwrap();
        }
        run_git(&["-C", d, "add", "-A"]);
        run_git(&[
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
        ]);
        let head = run_git(&["-C", d, "rev-parse", "HEAD"]);
        let repo = dir.join(".git");
        // A listing cap too small for three records keeps only the complete
        // ones and flags the result, instead of failing.
        let detail = commit_impl(&repo, &head, Duration::from_secs(10), 15)
            .unwrap()
            .unwrap();
        assert!(detail.truncated);
        assert!(!detail.files.is_empty());
        assert!(detail.files.len() < 3);
        assert!(detail.files.iter().all(|f| f.status == "A"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn commit_assigns_type_change_chunks_to_the_right_files() {
        let dir = std::env::temp_dir().join(format!("browse-type-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(&["init", "-q", "-b", "main", d]);
        std::fs::write(dir.join("f"), "a\n").unwrap();
        std::fs::write(dir.join("z"), "b\n").unwrap();
        run_git(&["-C", d, "add", "-A"]);
        run_git(&[
            "-C",
            d,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "1",
        ]);
        std::fs::remove_file(dir.join("f")).unwrap();
        std::os::unix::fs::symlink("z", dir.join("f")).unwrap();
        std::fs::write(dir.join("z"), "b\nc\n").unwrap();
        run_git(&["-C", d, "add", "-A"]);
        run_git(&[
            "-C",
            d,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "2",
        ]);
        let head = run_git(&["-C", d, "rev-parse", "HEAD"]);
        let repo = dir.join(".git");

        let detail = commit(&repo, &head, Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let files = &detail.files;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, "T");
        assert_eq!(files[0].new_path, "f");
        assert!(
            files[0].patch.contains("120000"),
            "type change keeps its add chunk: {}",
            files[0].patch
        );
        assert!(
            !files[0].patch.contains("+c"),
            "type change patch must not leak z: {}",
            files[0].patch
        );
        assert_eq!(files[1].status, "M");
        assert_eq!(files[1].new_path, "z");
        assert!(
            files[1].patch.contains("+c"),
            "z patch keeps its own content: {}",
            files[1].patch
        );
        assert!(
            !files[1].patch.contains("120000"),
            "z patch must not leak f: {}",
            files[1].patch
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A small repository on disk: returns (work tree, git dir, HEAD commit).
    fn make_repo() -> (std::path::PathBuf, std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("browse-unit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(&["init", "-q", "-b", "main", d]);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/guide.md"), "guide\n").unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run_git(&["-C", d, "add", "-A"]);
        run_git(&[
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
        ]);
        let head = run_git(&["-C", d, "rev-parse", "HEAD"]);
        (dir.clone(), dir.join(".git"), head)
    }

    #[test]
    fn resolve_rejects_invalid_rev() {
        let (dir, repo, _head) = make_repo();
        assert!(matches!(
            resolve(&repo, "a..b", Duration::from_secs(5)),
            Err(StorageError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_rejects_invalid_path() {
        let (dir, repo, head) = make_repo();
        assert!(matches!(
            tree(&repo, &head, Some("../bad"), Duration::from_secs(5)),
            Err(StorageError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_flags_oversized_listing() {
        let (dir, repo, head) = make_repo();
        assert!(matches!(
            tree_impl(&repo, &head, None, Duration::from_secs(5), 8),
            Err(StorageError::TooLarge { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_rejects_invalid_path() {
        let (dir, repo, head) = make_repo();
        assert!(matches!(
            blob(&repo, &head, "../bad", 1024, Duration::from_secs(5)),
            Err(StorageError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_rejects_invalid_path() {
        let (dir, repo, head) = make_repo();
        assert!(matches!(
            log(
                &repo,
                &head,
                Some(".."),
                Some(0),
                10,
                Duration::from_secs(5)
            ),
            Err(StorageError::Invalid(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refs_detached_head_has_no_default_branch() {
        let (dir, repo, head) = make_repo();
        run_git(&[
            "--git-dir",
            repo.to_str().unwrap(),
            "update-ref",
            "--no-deref",
            "HEAD",
            &head,
        ]);
        let found = refs(&repo, Duration::from_secs(5)).unwrap();
        assert_eq!(found.default_branch, None);
        assert!(!found.branches.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_commits_rejects_partial_records() {
        assert!(matches!(
            parse_commits(b"abc\0def\0", true),
            Err(StorageError::Invalid(_))
        ));
    }

    #[test]
    fn diff_parsers_reject_truncation_when_not_flagged() {
        assert!(matches!(
            parse_name_status(b"A\0", false),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            parse_name_status(b"R100\0old\0", false),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            parse_numstat(b"1", false),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            parse_numstat(b"1\t0", false),
            Err(StorageError::Invalid(_))
        ));
        assert!(matches!(
            parse_numstat(b"1\t0\t", false),
            Err(StorageError::Invalid(_))
        ));
    }

    #[test]
    fn diff_parsers_drop_partial_records_when_flagged() {
        // A trailing status with no path is dropped.
        assert!(parse_name_status(b"A\0", true).unwrap().is_empty());
        // A trailing rename with only the old path is dropped.
        assert!(parse_name_status(b"R100\0old\0", true).unwrap().is_empty());
        // A trailing numstat record missing its del/path is dropped.
        assert!(parse_numstat(b"1", true).unwrap().is_empty());
        // A trailing rename-style numstat with no paths is dropped.
        assert!(parse_numstat(b"1\t0\t", true).unwrap().is_empty());
    }

    #[test]
    fn stderr_error_prefers_git_message() {
        assert!(matches!(stderr_error(b"boom\n"), StorageError::Git(m) if m == "boom"));
        assert!(matches!(
            stderr_error(b"   "),
            StorageError::Git(m) if m == "git exited non-zero"
        ));
    }

    #[test]
    fn run_reports_failure_and_oversize() {
        fn sh(script: &str) -> Command {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
        let timeout = Duration::from_secs(5);
        assert!(matches!(
            run(sh("echo boom >&2; exit 3"), timeout, 1024),
            Err(StorageError::Git(m)) if m == "boom"
        ));
        assert!(matches!(
            run(sh("exit 3"), timeout, 1024),
            Err(StorageError::Git(m)) if m == "git exited non-zero"
        ));
        assert!(matches!(
            run(sh("printf '0123456789'"), timeout, 4),
            Err(StorageError::TooLarge { .. })
        ));
    }

    #[test]
    fn run_bounded_times_out() {
        let mut command = Command::new("sleep");
        command.arg("5");
        assert!(matches!(
            run_bounded(command, 1024, Duration::from_millis(20)),
            Err(StorageError::Timeout)
        ));
    }
}
