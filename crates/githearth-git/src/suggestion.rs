//! Applying a ```suggestion block as a commit on a pull request's head branch
//! (M3 inline review).
//!
//! The suggestion replaces one line of a text blob at the head commit with the
//! replacement text, writing objects into the repository's object store and
//! returning the new commit id. Nothing here moves a ref: the caller moves the
//! head branch through [`crate::merge::update_ref_expected`] so the update is
//! journaled and authorized exactly like a merge or push.

use crate::StorageError;
use crate::browse::{
    MAX_BLOB_BYTES, MAX_OUTPUT_BYTES, cmd, nul_fields, run_bounded, run_bounded_input,
    stderr_error, utf8,
};
use crate::merge::Identity;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

/// The replacement text of a ```suggestion block: each suggested line
/// newline-terminated, or `""` for an empty block (which deletes the line).
/// Returns `None` when the body carries no suggestion block.
pub fn suggestion_replacement(body: &str) -> Option<String> {
    let mut in_block = false;
    let mut lines: Vec<&str> = Vec::new();
    for raw in body.split('\n') {
        let line = raw.trim_end_matches('\r');
        if !in_block {
            if line.trim() == "```suggestion" {
                in_block = true;
            }
        } else if line.trim() == "```" {
            let mut out = String::new();
            for line in &lines {
                out.push_str(line);
                out.push('\n');
            }
            return Some(out);
        } else {
            lines.push(line);
        }
    }
    None
}

/// Split a blob into lines, each keeping its trailing newline except a final
/// line that has none. An empty blob has no lines.
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            lines.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// Replaces line `line` (1-based) of the blob at `head_commit:path` with
/// `replacement`, then builds and returns a new commit on top of `head_commit`
/// with `message`. The ref is not moved.
#[allow(clippy::too_many_arguments)]
pub fn apply_suggestion(
    repo: &Path,
    head_commit: &str,
    path: &str,
    line: usize,
    replacement: &[u8],
    message: &[u8],
    author: &Identity,
    committer: &Identity,
    timeout: Duration,
) -> Result<String, StorageError> {
    if line == 0 {
        return Err(StorageError::Invalid("line must be at least 1".into()));
    }
    if replacement.len() > MAX_BLOB_BYTES {
        return Err(StorageError::TooLarge {
            size: replacement.len() as u64,
            limit: MAX_BLOB_BYTES,
        });
    }
    for value in [
        &author.name,
        &author.email,
        &committer.name,
        &committer.email,
    ] {
        if !crate::merge::valid_identity(value) {
            return Err(StorageError::Invalid(
                "identity contains a forbidden character".into(),
            ));
        }
    }
    for offset in [&author.offset, &committer.offset] {
        if !crate::merge::valid_offset(offset) {
            return Err(StorageError::Invalid(
                "identity offset must be [+-]HHMM".into(),
            ));
        }
    }

    let blob = crate::browse::blob(repo, head_commit, path, MAX_BLOB_BYTES, timeout)?
        .ok_or_else(|| StorageError::Invalid("path not found at commit".into()))?;
    if blob.binary {
        return Err(StorageError::Invalid("path is not a text file".into()));
    }
    if blob.truncated {
        return Err(StorageError::TooLarge {
            size: blob.size,
            limit: MAX_BLOB_BYTES,
        });
    }
    let lines = split_lines(&blob.content);
    if line > lines.len() {
        return Err(StorageError::Invalid("line out of range".into()));
    }

    let mut content = Vec::with_capacity(blob.content.len() + replacement.len());
    for line in &lines[..line - 1] {
        content.extend_from_slice(line);
    }
    content.extend_from_slice(replacement);
    for line in &lines[line..] {
        content.extend_from_slice(line);
    }

    let blob_oid = write_blob(repo, &content, timeout)?;
    let mode = read_mode(repo, head_commit, path, timeout)?;
    let tree_oid = build_tree(repo, head_commit, path, &mode, &blob_oid, timeout)?;
    crate::merge::commit_tree(
        repo,
        None,
        &tree_oid,
        &[head_commit],
        message,
        author,
        committer,
        timeout,
    )
}

fn write_blob(repo: &Path, content: &[u8], timeout: Duration) -> Result<String, StorageError> {
    let mut command = cmd(repo);
    command.args(["hash-object", "-w", "--stdin"]);
    let result = run_bounded_input(command, content, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn read_mode(
    repo: &Path,
    commit: &str,
    path: &str,
    timeout: Duration,
) -> Result<String, StorageError> {
    let mut command = cmd(repo);
    command.args(["ls-tree", "-z", commit, "--", path]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    let record = nul_fields(&result.stdout)
        .into_iter()
        .next()
        .ok_or_else(|| StorageError::Invalid("path not found at commit".into()))?;
    let text = utf8(record)?;
    text.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| StorageError::Invalid("unexpected ls-tree record".into()))
}

/// Builds the head tree with the blob at `path` replaced, using a temporary
/// index file so the repository's real index is never touched.
fn build_tree(
    repo: &Path,
    head_commit: &str,
    path: &str,
    mode: &str,
    blob_oid: &str,
    timeout: Duration,
) -> Result<String, StorageError> {
    let index = std::env::temp_dir().join(format!("githearth-index-{}", Uuid::new_v4()));
    let result = build_tree_at(&index, repo, head_commit, path, mode, blob_oid, timeout);
    let _ = std::fs::remove_file(&index);
    result
}

fn build_tree_at(
    index: &Path,
    repo: &Path,
    head_commit: &str,
    path: &str,
    mode: &str,
    blob_oid: &str,
    timeout: Duration,
) -> Result<String, StorageError> {
    let mut command = cmd(repo);
    command
        .env("GIT_INDEX_FILE", index)
        .arg("read-tree")
        .arg(head_commit);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }

    // `--index-info -z`: `<mode> SP <object> TAB <path> NUL`. The path is a
    // single NUL-terminated field, so it can carry any byte valid_path allows.
    let mut input = Vec::with_capacity(mode.len() + 1 + blob_oid.len() + 1 + path.len() + 1);
    input.extend_from_slice(mode.as_bytes());
    input.push(b' ');
    input.extend_from_slice(blob_oid.as_bytes());
    input.push(b'\t');
    input.extend_from_slice(path.as_bytes());
    input.push(0);
    let mut command = cmd(repo);
    command
        .env("GIT_INDEX_FILE", index)
        .args(["update-index", "-z", "--index-info"]);
    let result = run_bounded_input(command, &input, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }

    let mut command = cmd(repo);
    command.env("GIT_INDEX_FILE", index).arg("write-tree");
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
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_replacement_parses_first_block_and_preserves_lines() {
        assert_eq!(suggestion_replacement("no block"), None);
        assert_eq!(
            suggestion_replacement("```suggestion\nfoo\nbar\n```\n"),
            Some("foo\nbar\n".into())
        );
        // An empty block deletes the line.
        assert_eq!(
            suggestion_replacement("```suggestion\n```\n"),
            Some(String::new())
        );
        // Text before the block is ignored.
        assert_eq!(
            suggestion_replacement("note\n```suggestion\nx\n```\n"),
            Some("x\n".into())
        );
        // An unclosed block yields no suggestion.
        assert_eq!(suggestion_replacement("```suggestion\nx\n"), None);
        // CRLF line endings are tolerated on the fence.
        assert_eq!(
            suggestion_replacement("```suggestion\r\nx\r\n```\r\n"),
            Some("x\n".into())
        );
        // Only the first block is returned.
        assert_eq!(
            suggestion_replacement("```suggestion\na\n```\n```suggestion\nb\n```\n"),
            Some("a\n".into())
        );
    }

    #[test]
    fn split_lines_keeps_terminators() {
        assert_eq!(split_lines(b""), Vec::<&[u8]>::new());
        assert_eq!(split_lines(b"a\n"), vec![b"a\n".as_slice()]);
        assert_eq!(
            split_lines(b"a\nb"),
            vec![b"a\n".as_slice(), b"b".as_slice()]
        );
        assert_eq!(
            split_lines(b"a\nb\n"),
            vec![b"a\n".as_slice(), b"b\n".as_slice()]
        );
    }
}
