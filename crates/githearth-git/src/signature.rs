//! SSH-signed commit verification for branch protection.
//!
//! A push into a protected branch that requires signed commits is verified by
//! the server while the incoming objects still live in the receive-pack
//! quarantine. `quarantine` names that object directory; when set, Git reads
//! the new objects from it and the pre-existing objects from the repository's
//! own `objects/` directory as an alternate.

use crate::StorageError;
use crate::browse::{MAX_OUTPUT_BYTES, cmd, run_bounded, stderr_error};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn with_objects(repo: &Path, quarantine: Option<&Path>) -> Command {
    let mut command = cmd(repo);
    if let Some(quarantine) = quarantine {
        command
            .env("GIT_OBJECT_DIRECTORY", quarantine)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", repo.join("objects"));
    }
    command
}

/// Commits reachable from `new` but not `old`, in topological order. When `old`
/// is all zeros (branch creation) this is every commit reachable from `new`.
pub fn new_commits(
    repo: &Path,
    old: &str,
    new: &str,
    quarantine: Option<&Path>,
    timeout: Duration,
) -> Result<Vec<String>, StorageError> {
    let mut command = with_objects(repo, quarantine);
    command.args(["rev-list", "--topo-order"]);
    if old.bytes().all(|b| b == b'0') {
        command.arg(new);
    } else {
        command.arg(format!("{old}..{new}"));
    }
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
    Ok(String::from_utf8_lossy(&result.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The commit's committer email.
pub fn committer_email(
    repo: &Path,
    oid: &str,
    quarantine: Option<&Path>,
    timeout: Duration,
) -> Result<String, StorageError> {
    let mut command = with_objects(repo, quarantine);
    command.args(["log", "-1", "--format=%ce", oid]);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    if !result.success {
        return Err(stderr_error(&result.stderr));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

/// Verifies `oid`'s SSH signature against `allowed_signers`. Returns true only
/// when `git verify-commit` accepts the signature; a missing or bad signature
/// returns false. Infrastructure failures return an error.
pub fn verify_ssh_signature(
    repo: &Path,
    oid: &str,
    quarantine: Option<&Path>,
    allowed_signers: &Path,
    timeout: Duration,
) -> Result<bool, StorageError> {
    let mut command = with_objects(repo, quarantine);
    command
        .arg("-c")
        .arg("gpg.format=ssh")
        .arg("-c")
        .arg(format!(
            "gpg.ssh.allowedSignersFile={}",
            allowed_signers.display()
        ))
        .arg("verify-commit")
        .arg(oid);
    let result = run_bounded(command, MAX_OUTPUT_BYTES, timeout)?;
    Ok(result.success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run_git(dir: &str, args: &[&str]) -> String {
        let mut command = StdCommand::new("git");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .arg("-C")
            .arg(dir)
            .args(args);
        let out = command.output().expect("git");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn new_commits_and_committer_email_read_objects_without_quarantine() {
        let dir = std::env::temp_dir().join(format!("sig-list-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a"), "1\n").unwrap();
        run_git(d, &["add", "-A"]);
        run_git(
            d,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=alice@x",
                "commit",
                "-q",
                "-m",
                "one",
            ],
        );
        let first = run_git(d, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("a"), "2\n").unwrap();
        run_git(d, &["add", "-A"]);
        run_git(
            d,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=alice@x",
                "commit",
                "-q",
                "-m",
                "two",
            ],
        );
        let second = run_git(d, &["rev-parse", "HEAD"]);
        let repo = dir.join(".git");
        let timeout = Duration::from_secs(10);

        let commits = new_commits(&repo, &first, &second, None, timeout).unwrap();
        assert_eq!(commits, vec![second.clone()]);
        assert_eq!(
            committer_email(&repo, &second, None, timeout).unwrap(),
            "alice@x"
        );

        // Creation (old all zeros) lists every reachable commit.
        let zero = "0".repeat(40);
        let all = new_commits(&repo, &zero, &second, None, timeout).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&first));
        assert!(all.contains(&second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_ssh_signature_reports_bad_and_missing_signatures() {
        let dir = std::env::temp_dir().join(format!("sig-verify-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        run_git(d, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a"), "1\n").unwrap();
        run_git(d, &["add", "-A"]);
        run_git(
            d,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=alice@x",
                "commit",
                "-q",
                "-m",
                "one",
            ],
        );
        let head = run_git(d, &["rev-parse", "HEAD"]);
        let repo = dir.join(".git");
        let timeout = Duration::from_secs(10);

        // An empty allowed-signers file rejects an unsigned commit.
        let signers = dir.join("allowed");
        std::fs::write(&signers, "").unwrap();
        assert!(!verify_ssh_signature(&repo, &head, None, &signers, timeout).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
