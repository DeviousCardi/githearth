//! Real process contract tests for the server binary; database cases opt in with --ignored.
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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

/// One short-lived command at a time. `migrate` (sqlx's migration advisory lock)
/// and `bootstrap-admin` take database-wide locks under the server's 3 s
/// `lock_timeout`; run in parallel on a loaded machine, waiters time out spuriously.
static RUN: Mutex<()> = Mutex::new(());

fn run(mut command: Command) -> Output {
    let _one = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().unwrap()
}

fn run_with_stdin(mut command: Command, input: &[u8]) -> Output {
    let _one = RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn temp_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "githearth-server-{}-{}-{name}",
        std::process::id(),
        nonce()
    ))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn execute(url: &str, statement: &str) {
    runtime().block_on(async {
        let pool = sqlx::PgPool::connect(url).await.unwrap();
        sqlx::query(statement).execute(&pool).await.unwrap();
        pool.close().await;
    });
}

fn schema_url(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{schema}")
}

struct Schema {
    base: String,
    name: String,
    url: String,
}
impl Schema {
    fn new() -> Self {
        let base = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect(
            "set GITHEARTH_TEST_DATABASE_URL to a disposable PostgreSQL database; see docs/foundation.md",
        );
        let name = format!("gh_bin_{}_{}", std::process::id(), nonce());
        execute(&base, &format!("CREATE SCHEMA {name}"));
        let url = schema_url(&base, &name);
        Self { base, name, url }
    }
    fn url(&self) -> &str {
        &self.url
    }
    fn execute(&self, statement: &str) {
        execute(&self.url, statement);
    }
}
impl Drop for Schema {
    fn drop(&mut self) {
        let _ = runtime().block_on(async {
            let pool = sqlx::PgPool::connect(&self.base).await?;
            sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.name))
                .execute(&pool)
                .await?;
            pool.close().await;
            Ok::<(), sqlx::Error>(())
        });
    }
}

struct StorageGuard {
    path: PathBuf,
}
impl StorageGuard {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "githearth-bin-storage-{}-{}",
            std::process::id(),
            nonce()
        ));
        let _ = fs::create_dir_all(&path);
        Self { path }
    }
    fn env(&self) -> (&'static str, &str) {
        ("GITHEARTH_STORAGE_ROOT", self.path.to_str().unwrap())
    }
}
impl Drop for StorageGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard {
    child: Child,
}
impl ChildGuard {
    fn new(mut command: Command) -> Self {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self {
            child: command.spawn().unwrap(),
        }
    }
    fn pid(&self) -> u32 {
        self.child.id()
    }
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut handle) = self.child.stderr.take() {
            let _ = handle.read_to_string(&mut text);
        }
        text
    }
    fn stdout(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut handle) = self.child.stdout.take() {
            let _ = handle.read_to_string(&mut text);
        }
        text
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http_status(port: u16, path: &str) -> Option<u16> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn wait_for_health(server: &mut ChildGuard, port: u16, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if http_status(port, path) == Some(200) {
            return;
        }
        if let Some(status) = server.child.try_wait().unwrap() {
            panic!("server exited with {status}: {}", server.stderr());
        }
        assert!(Instant::now() < deadline, "timed out waiting for {path}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([format!("-{signal}"), pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn migrate(schema: &Schema) -> Output {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.arg("migrate");
    run(command)
}

fn bootstrap(schema: &Schema, username: &str, password: &[u8]) -> Output {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.args(["bootstrap-admin", "--username", username]);
    run_with_stdin(command, password)
}

fn graceful_signal(signal: &str) {
    let schema = Schema::new();
    let storage = StorageGuard::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let port = free_port();
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.env("GITHEARTH_BIND", format!("127.0.0.1:{port}"));
    command.env(storage.env().0, storage.env().1);
    command.arg("serve");
    let mut server = ChildGuard::new(command);

    wait_for_health(&mut server, port, "/health/live");
    wait_for_health(&mut server, port, "/health/ready");
    send_signal(server.pid(), signal);
    let status = server
        .wait_for_exit(Duration::from_secs(10))
        .expect("server did not exit within 10s of the signal");
    let logs = server.stdout();
    assert!(status.success(), "server exited with {status}: {logs}");
    assert!(logs.contains("HTTP server listening"), "{logs}");
    drop(storage);
}

#[test]
fn help_prints_usage() {
    let mut command = server();
    command.arg("--help");
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    assert!(help.contains("Experimental GitHearth foundation server"));
    assert!(help.contains("Usage: githearth-server"));
    assert!(help.contains("bootstrap-admin"));
}

#[test]
fn missing_or_unknown_commands_fail() {
    for args in [vec![], vec!["frobnicate"]] {
        let mut command = server();
        command.args(args);
        let output = run(command);
        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(2));
        let message = stderr(&output);
        assert!(message.contains("Usage: githearth-server"), "{message}");
    }
}

#[test]
fn missing_configuration_is_rejected() {
    let mut command = server();
    command.arg("migrate");
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "database_url must be a PostgreSQL URL\n");
    assert!(output.stdout.is_empty());
}

#[test]
fn unreadable_configuration_is_rejected() {
    let path = temp_file("missing.toml");
    let mut command = server();
    command.args(["--config", path.to_str().unwrap(), "migrate"]);
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("cannot read configuration file"),
        "{message}"
    );
}

#[test]
fn invalid_configuration_is_redacted() {
    let secret = "configuration-secret-must-not-appear";
    let path = temp_file("invalid.toml");
    fs::write(
        &path,
        format!(
            "database_url = \"postgres://user:{secret}@localhost/test\"\nunknown_setting = true\n"
        ),
    )
    .unwrap();
    let mut command = server();
    command.args(["--config", path.to_str().unwrap(), "migrate"]);
    let output = run(command);
    fs::remove_file(&path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "invalid TOML configuration: check field names and types\n"
    );
    assert!(!stderr(&output).contains(secret));
}

#[test]
fn invalid_environment_value_is_rejected() {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", "postgres://localhost/test");
    command.env("GITHEARTH_BIND", "not-an-address");
    command.arg("migrate");
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "invalid GITHEARTH_BIND\n");
}

#[test]
fn invalid_database_url_is_rejected_without_echoing_credentials() {
    let mut command = server();
    command.env(
        "GITHEARTH_DATABASE_URL",
        "postgres://user:supersecret@127.0.0.1:notaport/db",
    );
    command.arg("migrate");
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("invalid PostgreSQL connection URL"),
        "{message}"
    );
    assert!(!message.contains("supersecret"));
}

#[test]
fn unreachable_database_is_reported() {
    let mut command = server();
    command.env(
        "GITHEARTH_DATABASE_URL",
        "postgres://user:supersecret@127.0.0.1:1/db",
    );
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "1");
    command.arg("migrate");
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message
            .contains("cannot connect to PostgreSQL; check database availability and credentials"),
        "{message}"
    );
    assert!(!message.contains("supersecret"));
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn migrate_is_idempotent() {
    let schema = Schema::new();
    for _ in 0..2 {
        let output = migrate(&schema);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains("schema migrated"));
    }
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn migrate_reports_failure() {
    let schema = Schema::new();
    schema.execute("CREATE TABLE accounts (id integer)");
    let output = migrate(&schema);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "migration failed; inspect database availability and migration history\n"
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn bootstrap_admin_validates_and_refuses_duplicates() {
    let schema = Schema::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let output = bootstrap(&schema, "operator", b"correct horse battery staple\r\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("administrator created"));

    let output = bootstrap(&schema, "other-operator", b"another secure password");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("an administrator already exists"),
        "{}",
        stderr(&output)
    );

    let output = bootstrap(&schema, "Bad", b"another secure password");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("username must"));

    let output = bootstrap(&schema, "valid-name", b"short");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("password must"));

    let output = bootstrap(&schema, "valid-name", &[b'a'; 1026]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "password input exceeds limit\n");

    let output = bootstrap(&schema, "valid-name", &[0xff, 0xfe]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "cannot read password from stdin\n");
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn bootstrap_admin_reports_missing_schema() {
    let schema = Schema::new();
    let output = bootstrap(&schema, "operator", b"correct horse battery staple");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "bootstrap failed; check schema, database, and username availability\n"
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn serve_stops_gracefully_on_sigterm() {
    graceful_signal("TERM");
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn serve_stops_gracefully_on_sigint() {
    graceful_signal("INT");
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn serve_reports_bind_failure() {
    let schema = Schema::new();
    let storage = StorageGuard::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.env("GITHEARTH_BIND", format!("127.0.0.1:{port}"));
    command.env(storage.env().0, storage.env().1);
    command.arg("serve");
    let output = run(command);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("cannot bind HTTP listener"), "{message}");
    drop(listener);
    drop(storage);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn audit_prune_ips_rejects_invalid_days() {
    let schema = Schema::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.args(["audit", "prune-ips", "--older-than", "0"]);
    let output = run(command);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("invalid value '0'"),
        "{}",
        stderr(&output)
    );

    let mut command2 = server();
    command2.env("GITHEARTH_DATABASE_URL", schema.url());
    command2.args(["audit", "prune-ips", "--older-than", "-5"]);
    let output2 = run(command2);
    assert_eq!(output2.status.code(), Some(2), "{}", stderr(&output2));
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn audit_prune_ips_dry_run_counts_rows() {
    let schema = Schema::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.args(["audit", "prune-ips", "--older-than", "1", "--dry-run"]);
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("DRY RUN:"), "{out}");
    assert!(out.contains("clear"), "{out}");
    assert!(out.contains("legacy"), "{out}");
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn audit_rotate_ip_key_creates_key() {
    let schema = Schema::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.args(["audit", "rotate-ip-key"]);
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("created new key"),
        "{}",
        stdout(&output)
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn audit_drop_ip_keys_dry_run_counts_keys() {
    let schema = Schema::new();
    let migrated = migrate(&schema);
    assert!(migrated.status.success(), "{}", stderr(&migrated));

    // Create a key first so there's something to drop
    let mut cmd0 = server();
    cmd0.env("GITHEARTH_DATABASE_URL", schema.url());
    cmd0.args(["audit", "rotate-ip-key"]);
    assert!(run(cmd0).status.success());

    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", schema.url());
    command.args(["audit", "drop-ip-keys", "--older-than", "1", "--dry-run"]);
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("DRY RUN:"), "{}", stdout(&output));
}
