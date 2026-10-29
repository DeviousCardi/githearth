//! The Git hooks GitHearth installs (`pre-receive`, `reference-transaction`).
//!
//! Git runs each hook as `githearth-server git-hook <name> [args]` with a
//! sanitized environment: the receive-pack command was launched with
//! `env_clear`, so everything the hook needs arrives via environment
//! variables.  All logic lives in [`run`], which takes the hook name, args,
//! environment, stdin and stdout/stderr as values, so it is unit-testable in
//! process; the binary only forwards process stdio to it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

use githearth_git::journal::{Journal, Kind, Update};

const HOOK_SOCKET: &str = "GITHEARTH_HOOK_SOCKET";
const HOOK_TOKEN: &str = "GITHEARTH_HOOK_TOKEN";
const HOOK_OPERATION: &str = "GITHEARTH_OPERATION";
const HOOK_ACTOR: &str = "GITHEARTH_ACTOR";
const REFUSE: &str = "ref updates must go through GitHearth";
const RESPONSE_LIMIT: u64 = 4096;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Entry point for the `git-hook` subcommand: forwards process stdio.
pub fn run_process(name: &str, args: &[String]) -> i32 {
    let env: HashMap<String, String> = std::env::vars().collect();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run(
        name,
        args,
        &env,
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

/// Runs one hook invocation and returns the process exit code.
pub fn run(
    name: &str,
    args: &[String],
    env: &HashMap<String, String>,
    stdin: &mut dyn Read,
    _stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match name {
        "pre-receive" => pre_receive(env, stdin, stderr),
        "reference-transaction" => reference_transaction(args, env, stdin, stderr),
        _ => {
            let _ = writeln!(stderr, "unknown hook: {name}");
            1
        }
    }
}

fn has_hook_env(env: &HashMap<String, String>) -> bool {
    env.contains_key(HOOK_SOCKET)
        && env.contains_key(HOOK_TOKEN)
        && env.contains_key(HOOK_OPERATION)
        && env.contains_key(HOOK_ACTOR)
}

fn is_zero(oid: &str) -> bool {
    oid.bytes().all(|b| b == b'0')
}

/// `old` is an ancestor of `new` in the repository Git gave this hook
/// (including its quarantine).  False when either side is missing.
fn fast_forward(old: &str, new: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .status()
        .is_ok_and(|s| s.success())
}

/// Stage 3: asks the server, over the local socket, to admit every ref update.
fn pre_receive(env: &HashMap<String, String>, stdin: &mut dyn Read, stderr: &mut dyn Write) -> i32 {
    if !has_hook_env(env) {
        let _ = writeln!(stderr, "{REFUSE}");
        return 1;
    }
    let socket = env[HOOK_SOCKET].as_str();
    let token = env[HOOK_TOKEN].as_str();
    let mut input = String::new();
    if stdin.read_to_string(&mut input).is_err() {
        let _ = writeln!(stderr, "cannot read ref updates from stdin");
        return 1;
    }
    let mut updates = Vec::new();
    for line in input.lines() {
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(reference)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        // Fast-forward is only meaningful when both sides exist.
        let ff = !is_zero(old) && !is_zero(new) && fast_forward(old, new);
        updates.push((old.to_owned(), new.to_owned(), reference.to_owned(), ff));
    }
    match query_policy(socket, token, &updates) {
        Ok(()) => 0,
        Err(reason) => {
            let _ = writeln!(stderr, "{reason}");
            1
        }
    }
}

fn query_policy(
    socket: &str,
    token: &str,
    updates: &[(String, String, String, bool)],
) -> Result<(), String> {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;

    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("cannot reach the push service: {e}"))?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| format!("cannot reach the push service: {e}"))?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| format!("cannot reach the push service: {e}"))?;

    let mut request = String::with_capacity(64 + 64 * updates.len());
    request.push_str(token);
    request.push('\n');
    for (old, new, reference, ff) in updates {
        request.push_str(old);
        request.push(' ');
        request.push_str(new);
        request.push(' ');
        request.push_str(reference);
        request.push(' ');
        request.push_str(if *ff { "1" } else { "0" });
        request.push('\n');
    }
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot reach the push service: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("cannot reach the push service: {e}"))?;

    let mut response = String::new();
    match stream.take(RESPONSE_LIMIT).read_to_string(&mut response) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(format!("cannot reach the push service: {e}")),
    }
    let response = response.trim();
    if response == "OK" {
        Ok(())
    } else if let Some(reason) = response.strip_prefix("DENIED ") {
        Err(reason.to_owned())
    } else {
        Err(response.to_owned())
    }
}

fn repo_path(env: &HashMap<String, String>) -> Option<PathBuf> {
    let dir = env.get("GIT_DIR")?;
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

/// The journaled updates: `refs/heads/*` and `refs/tags/*` only.  Git reports
/// the packed-refs half of a deletion as a second, zero-to-zero transaction
/// (and `HEAD` on the first push), so those lines are dropped to journal each
/// deletion exactly once.
fn parse_updates(stdin: &mut dyn Read) -> Result<Vec<Update>, String> {
    let mut input = String::new();
    if stdin.read_to_string(&mut input).is_err() {
        return Err("cannot read ref updates from stdin".into());
    }
    let mut updates = Vec::new();
    for line in input.lines() {
        let mut parts = line.split(' ');
        let (Some(old), Some(new), Some(reference)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if parts.next().is_some() {
            continue;
        }
        if !crate::pushes::valid_ref_name(reference) {
            continue;
        }
        if is_zero(old) && is_zero(new) {
            continue;
        }
        updates.push(Update {
            old: old.to_owned(),
            new: new.to_owned(),
            reference: reference.to_owned(),
        });
    }
    Ok(updates)
}

/// Stages 4-5: journal the transaction.  `prepared` is durable; markers are
/// best-effort.
fn reference_transaction(
    args: &[String],
    env: &HashMap<String, String>,
    stdin: &mut dyn Read,
    stderr: &mut dyn Write,
) -> i32 {
    let state = args.first().map(String::as_str).unwrap_or("");
    // `preparing` on newer Git and anything unknown: nothing to journal.
    if !matches!(state, "prepared" | "committed" | "aborted") {
        return 0;
    }
    if !has_hook_env(env) {
        let _ = writeln!(stderr, "{REFUSE}");
        return 1;
    }
    let Some(repo) = repo_path(env) else {
        let _ = writeln!(stderr, "cannot determine repository directory");
        return 1;
    };
    let updates = match parse_updates(stdin) {
        Ok(u) => u,
        Err(msg) => {
            let _ = writeln!(stderr, "{msg}");
            return 1;
        }
    };
    match state {
        "prepared" => {
            if updates.is_empty() {
                return 0;
            }
            match append_prepared(&repo, env, updates) {
                Ok(()) => 0,
                Err(msg) => {
                    let _ = writeln!(stderr, "cannot record ref transaction: {msg}");
                    1
                }
            }
        }
        "committed" | "aborted" => {
            if updates.is_empty() {
                return 0;
            }
            let kind = if state == "committed" {
                Kind::Committed
            } else {
                Kind::Aborted
            };
            match append_marker(&repo, env, kind) {
                Ok(()) => 0,
                Err(msg) => {
                    let _ = writeln!(stderr, "cannot record ref transaction marker: {msg}");
                    0
                }
            }
        }
        _ => unreachable!("unknown states returned above"),
    }
}

fn append_prepared(
    repo: &Path,
    env: &HashMap<String, String>,
    updates: Vec<Update>,
) -> Result<(), String> {
    let operation =
        Uuid::parse_str(&env[HOOK_OPERATION]).map_err(|e| format!("invalid operation: {e}"))?;
    let actor = Uuid::parse_str(&env[HOOK_ACTOR]).map_err(|e| format!("invalid actor: {e}"))?;
    let mut journal = Journal::open(repo).map_err(|e| e.to_string())?;
    journal
        .append_prepared(operation, Some(actor), updates)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn append_marker(repo: &Path, env: &HashMap<String, String>, kind: Kind) -> Result<(), String> {
    let operation =
        Uuid::parse_str(&env[HOOK_OPERATION]).map_err(|e| format!("invalid operation: {e}"))?;
    let mut journal = Journal::open(repo).map_err(|e| e.to_string())?;
    journal
        .append_marker(operation, kind)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert(HOOK_SOCKET.into(), "/nonexistent/hook.sock".into());
        env.insert(HOOK_TOKEN.into(), "a".repeat(64));
        env.insert(HOOK_OPERATION.into(), Uuid::new_v4().to_string());
        env.insert(HOOK_ACTOR.into(), Uuid::new_v4().to_string());
        for (k, v) in extra {
            env.insert((*k).into(), (*v).into());
        }
        env
    }

    fn run_with(
        name: &str,
        args: &[&str],
        env: &HashMap<String, String>,
        stdin: &str,
    ) -> (i32, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            name,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            env,
            &mut stdin.as_bytes(),
            &mut out,
            &mut err,
        );
        (code, String::from_utf8(err).unwrap())
    }

    /// A `Read` that always fails, to exercise stdin error paths.
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
    }

    /// A temp directory with the `githearth/` subdirectory a repository would
    /// have, so `Journal::open` can create its journal file.
    fn temp_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("githearth-hooks-r-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("githearth")).unwrap();
        dir
    }

    const ZERO: &str = "0000000000000000000000000000000000000000";
    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const UPDATE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/main\n";

    /// A one-shot Unix listener that answers a fixed response.
    fn mock_server(response: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("githearth-hooks-s-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hook.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let path = sock.to_str().unwrap().to_owned();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);
            let _ = std::fs::remove_dir_all(&dir);
        });
        (path, handle)
    }

    #[test]
    fn missing_env_refuses() {
        for missing in [HOOK_SOCKET, HOOK_TOKEN, HOOK_OPERATION, HOOK_ACTOR] {
            let mut env = env_with(&[]);
            env.remove(missing);
            let (code, err) = run_with("pre-receive", &[], &env, "");
            assert_eq!(code, 1, "missing {missing}");
            assert!(err.contains(REFUSE), "missing {missing}: {err}");
        }
    }

    #[test]
    fn reference_transaction_missing_env_refuses() {
        let mut env = env_with(&[]);
        env.remove(HOOK_TOKEN);
        let (code, err) = run_with("reference-transaction", &["prepared"], &env, UPDATE);
        assert_eq!(code, 1);
        assert!(err.contains(REFUSE), "{err}");
    }

    #[test]
    fn unknown_hook_name_fails() {
        let (code, err) = run_with("update", &[], &env_with(&[]), "");
        assert_eq!(code, 1);
        assert!(err.contains("unknown hook"), "{err}");
    }

    #[test]
    fn socket_down_fails() {
        let (code, err) = run_with(
            "pre-receive",
            &[],
            &env_with(&[]),
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        assert_eq!(code, 1);
        assert!(err.contains("push service"), "{err}");
    }

    #[test]
    fn pre_receive_accepts_ok_response() {
        let (path, server) = mock_server("OK\n");
        let env = env_with(&[("GITHEARTH_HOOK_SOCKET", path.as_str())]);
        let (code, _err) = run_with(
            "pre-receive",
            &[],
            &env,
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        server.join().unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn pre_receive_denied_response_prints_reason() {
        let (path, server) = mock_server("DENIED permission denied: ref.push\n");
        let env = env_with(&[("GITHEARTH_HOOK_SOCKET", path.as_str())]);
        let (code, err) = run_with(
            "pre-receive",
            &[],
            &env,
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        server.join().unwrap();
        assert_eq!(code, 1);
        assert!(err.contains("permission denied: ref.push"), "{err}");
    }

    #[test]
    fn pre_receive_other_response_prints_it() {
        let (path, server) = mock_server("ERROR something went wrong\n");
        let env = env_with(&[("GITHEARTH_HOOK_SOCKET", path.as_str())]);
        let (code, err) = run_with(
            "pre-receive",
            &[],
            &env,
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        server.join().unwrap();
        assert_eq!(code, 1);
        assert!(err.contains("something went wrong"), "{err}");
    }

    #[test]
    fn pre_receive_stdin_error_fails() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            "pre-receive",
            &[],
            &env_with(&[]),
            &mut FailingReader,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("cannot read ref updates")
        );
    }

    #[test]
    fn pre_receive_malformed_lines_are_skipped() {
        let (path, server) = mock_server("OK\n");
        let env = env_with(&[("GITHEARTH_HOOK_SOCKET", path.as_str())]);
        let stdin = format!(
            "{ZERO} {SHA}\n{ZERO} {SHA} refs/heads/main extra\n{ZERO} {SHA} refs/heads/main\n"
        );
        let (code, _err) = run_with("pre-receive", &[], &env, &stdin);
        server.join().unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn bad_token_reports_server_reason() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("githearth-hooks-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("hook.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let sock_path = sock.to_str().unwrap().to_owned();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();
            assert!(buf.starts_with(&"a".repeat(64)));
            stream
                .write_all(b"DENIED permission denied: ref.push\n")
                .unwrap();
        });
        let env = env_with(&[("GITHEARTH_HOOK_SOCKET", &sock_path)]);
        let (code, err) = run_with(
            "pre-receive",
            &[],
            &env,
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        server.join().unwrap();
        assert_eq!(code, 1);
        assert!(err.contains("permission denied: ref.push"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_path_resolves_absolute_and_relative() {
        let abs = env_with(&[("GIT_DIR", "/tmp/repo")]);
        assert_eq!(repo_path(&abs), Some(PathBuf::from("/tmp/repo")));
        let rel = env_with(&[("GIT_DIR", "rel")]);
        assert_eq!(
            repo_path(&rel),
            Some(std::env::current_dir().unwrap().join("rel"))
        );
        let none = env_with(&[]);
        assert_eq!(repo_path(&none), None);
    }

    #[test]
    fn unknown_reference_transaction_state_does_nothing() {
        let (code, _err) = run_with("reference-transaction", &["preparing"], &env_with(&[]), "");
        assert_eq!(code, 0);
    }

    #[test]
    fn reference_transaction_missing_git_dir_fails() {
        let mut env = env_with(&[]);
        env.remove("GIT_DIR");
        let (code, err) = run_with("reference-transaction", &["prepared"], &env, UPDATE);
        assert_eq!(code, 1);
        assert!(
            err.contains("cannot determine repository directory"),
            "{err}"
        );
    }

    #[test]
    fn reference_transaction_stdin_error_fails() {
        let repo = temp_repo();
        let env = env_with(&[("GIT_DIR", repo.to_str().unwrap())]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            "reference-transaction",
            &["prepared".to_string()],
            &env,
            &mut FailingReader,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("cannot read ref updates")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn reference_transaction_empty_updates_do_nothing() {
        let repo = temp_repo();
        let env = env_with(&[("GIT_DIR", repo.to_str().unwrap())]);
        for state in ["prepared", "committed", "aborted"] {
            let (code, _err) = run_with("reference-transaction", &[state], &env, "");
            assert_eq!(code, 0, "{state}");
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn reference_transaction_prepared_journals_updates() {
        let repo = temp_repo();
        let env = env_with(&[("GIT_DIR", repo.to_str().unwrap())]);
        let (code, err) = run_with("reference-transaction", &["prepared"], &env, UPDATE);
        assert_eq!(code, 0, "{err}");
        let journal = Journal::open(&repo).unwrap();
        let records: Vec<_> = journal.records_after(0).cloned().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, Kind::Prepared);
        assert_eq!(records[0].updates.len(), 1);
        assert_eq!(records[0].updates[0].reference, "refs/heads/main");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn reference_transaction_marker_states_append_marker() {
        for state in ["committed", "aborted"] {
            let repo = temp_repo();
            let env = env_with(&[("GIT_DIR", repo.to_str().unwrap())]);
            let (code, err) = run_with("reference-transaction", &[state], &env, UPDATE);
            assert_eq!(code, 0, "{state}: {err}");
            let journal = Journal::open(&repo).unwrap();
            let records: Vec<_> = journal.records_after(0).cloned().collect();
            assert_eq!(records.len(), 1);
            assert_eq!(
                records[0].kind,
                if state == "committed" {
                    Kind::Committed
                } else {
                    Kind::Aborted
                }
            );
            let _ = std::fs::remove_dir_all(&repo);
        }
    }

    #[test]
    fn reference_transaction_bad_operation_or_actor_fails() {
        for (key, value, msg) in [
            (HOOK_OPERATION, "not-a-uuid", "invalid operation"),
            (HOOK_ACTOR, "not-a-uuid", "invalid actor"),
        ] {
            let repo = temp_repo();
            let env = env_with(&[(key, value), ("GIT_DIR", repo.to_str().unwrap())]);
            let (code, err) = run_with("reference-transaction", &["prepared"], &env, UPDATE);
            assert_eq!(code, 1, "{key}");
            assert!(err.contains(msg), "{key}: {err}");
            let _ = std::fs::remove_dir_all(&repo);
        }
    }

    #[test]
    fn reference_transaction_marker_failure_is_best_effort() {
        // GIT_DIR inside a regular file: the marker append fails but the hook
        // must still exit 0 (best-effort).
        let dir = std::env::temp_dir().join(format!("githearth-hooks-m-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let env = env_with(&[("GIT_DIR", blocker.join("sub").to_str().unwrap())]);
        let (code, err) = run_with("reference-transaction", &["committed"], &env, UPDATE);
        assert_eq!(code, 0);
        assert!(
            err.contains("cannot record ref transaction marker"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_append_failure_exits_nonzero() {
        // GIT_DIR points inside a regular file, so Journal::open cannot create
        // the journal and the prepared append must fail the hook.
        let dir = std::env::temp_dir().join(format!("githearth-hooks-j-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let env = env_with(&[("GIT_DIR", blocker.join("sub").to_str().unwrap())]);
        let (code, err) = run_with(
            "reference-transaction",
            &["prepared"],
            &env,
            &format!("{ZERO} {SHA} refs/heads/main\n"),
        );
        assert_eq!(code, 1);
        assert!(err.contains("cannot record ref transaction"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_to_zero_deletion_and_head_are_not_journaled() {
        let dir = std::env::temp_dir().join(format!("githearth-hooks-p-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let stdin = format!(
            "{zero} {zero} refs/heads/main\n{zero} {new} refs/heads/main\n{zero} {new} HEAD\n",
            zero = "0".repeat(40),
            new = "a".repeat(40),
        );
        let updates = parse_updates(&mut stdin.as_bytes()).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].reference, "refs/heads/main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_updates_skips_malformed_lines_and_stdin_error() {
        let stdin = format!(
            "{ZERO}\n{ZERO} {SHA} refs/heads/main extra\n{REFUSE} {SHA} refs/heads/main\n{SHA} {SHA} refs/heads/main\n",
        );
        let updates = parse_updates(&mut stdin.as_bytes()).unwrap();
        assert_eq!(updates.len(), 1);
        assert!(parse_updates(&mut FailingReader).is_err());
    }
}
