//! Doctor CLI integration tests; DB cases use --ignored.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn nonce() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_githearth-server"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GITHEARTH_") {
            command.env_remove(name);
        }
    }
    command
}

fn run(mut command: Command) -> std::process::Output {
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    command.output().unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "githearth-doctor-{}-{}-{name}",
        std::process::id(),
        nonce()
    ))
}

fn self_storage_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "githearth-doctor-storage-{}-{}",
        std::process::id(),
        nonce()
    ))
}

fn write_config(path: &std::path::Path, database_url: &str, storage_root: &str) {
    let content = format!("database_url = \"{database_url}\"\nstorage_root = \"{storage_root}\"\n");
    std::fs::write(path, content).unwrap();
}

#[test]
fn doctor_help() {
    let mut command = server();
    command.args(["doctor", "--help"]);
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Run read-only preflight checks"), "{out}");
}

#[test]
fn doctor_with_missing_storage_root_warns_but_exits_zero() {
    let mut command = server();
    let root = self_storage_root();
    let path = temp_file("ok.toml");
    write_config(
        &path,
        "postgres://unused:unused@127.0.0.1:1/db",
        root.to_str().unwrap(),
    );
    command.args(["--config", path.to_str().unwrap(), "doctor"]);
    let output = run(command);
    // Storage root does not exist -> warn, but database is unreachable -> fail
    let out = stdout(&output);
    assert!(
        out.contains("does not exist; will be created by serve"),
        "{out}"
    );
    // Database is unreachable, so exit code should be 1
    assert!(!output.status.success());
    // Summary must not leak password
    assert!(!out.contains("unused"));
    let _ = fs::remove_file(&path);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn doctor_with_fresh_database_reports_all_ok() {
    let base =
        std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");

    let root = self_storage_root();
    fs::create_dir_all(&root).unwrap();
    let config_path = temp_file("doctor-fresh.toml");
    let config_content = format!(
        "database_url = \"{base}\"\nstorage_root = \"{}\"\ndatabase_timeout_seconds = 5\n",
        root.to_str().unwrap()
    );
    std::fs::write(&config_path, config_content).unwrap();

    // Migrate first: the shared test database's state depends on test order.
    let mut command = server();
    command.args(["--config", config_path.to_str().unwrap(), "migrate"]);
    command.env_remove("GITHEARTH_DATABASE_URL");
    command.env_remove("GITHEARTH_STORAGE_ROOT");
    let output = run(command);
    assert!(output.status.success(), "migrate: {}", stderr(&output));
    let mut command = server();
    command.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    command.env_remove("GITHEARTH_DATABASE_URL");
    command.env_remove("GITHEARTH_STORAGE_ROOT");
    let output = run(command);
    let out = stdout(&output);
    assert!(
        output.status.success(),
        "stdout: {out}\nstderr: {}",
        stderr(&output)
    );
    assert!(out.contains("ok   config:"), "{out}");
    assert!(out.contains("ok   git:"), "{out}");
    assert!(out.contains("ok   storage:"), "{out}");
    assert!(out.contains("ok   database:"), "{out}");
    assert!(out.contains("ok   schema:"), "{out}");
    // Other tests may leave stuck repositories in the shared database: ok or warn.
    assert!(
        out.contains(" lifecycle:") && !out.contains("fail lifecycle"),
        "{out}"
    );
    assert!(out.contains("0 failed"), "{out}");
    let _ = fs::remove_file(&config_path);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn doctor_unreachable_database_sanitized() {
    let root = self_storage_root();
    let config_path = temp_file("doctor-bad-db.toml");
    write_config(
        &config_path,
        "postgres://user:supersecret@127.0.0.1:1/db",
        root.to_str().unwrap(),
    );
    let mut command = server();
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "1");
    command.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    let output = run(command);
    assert!(!output.status.success());
    let out = stdout(&output);
    assert!(
        out.contains("cannot connect to PostgreSQL; check database availability and credentials"),
        "{out}"
    );
    assert!(!out.contains("supersecret"), "{out}");
    assert!(!out.contains("127.0.0.1:1"), "{out}");
    let _ = fs::remove_file(&config_path);
}

#[test]
fn doctor_with_invalid_config_fails_with_exit_code_1() {
    let config_path = temp_file("doctor-invalid-config.toml");
    std::fs::write(
        &config_path,
        "database_url = \"postgres://localhost/test\"\nunknown_field = true\n",
    )
    .unwrap();
    let mut command = server();
    command.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains("invalid TOML configuration: check field names and types"),
        "{err}"
    );
    let _ = fs::remove_file(&config_path);
}

#[test]
fn doctor_missing_config_reports_error() {
    let mut command = server();
    command.arg("doctor");
    let output = run(command);
    assert!(!output.status.success());
    assert_eq!(stderr(&output), "database_url must be a PostgreSQL URL\n");
}
