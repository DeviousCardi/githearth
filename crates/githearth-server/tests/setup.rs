//! `githearth-server setup` integration tests (M4.1 acceptance case U09).
//!
//! Each test owns a temporary installation directory and its own database so
//! the suite passes in parallel and repeatedly. The database cases are
//! `--ignored` like the other real-PostgreSQL tests in this crate.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

/// The test database; never printed, because it carries the server's password.
fn base_url() -> String {
    std::env::var("GITHEARTH_TEST_DATABASE_URL").expect(
        "set GITHEARTH_TEST_DATABASE_URL to a disposable PostgreSQL database; see docs/foundation.md",
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// One database per test, removed on drop like `tests/binary.rs` does with schemas.
struct Database {
    admin_url: String,
    name: String,
    url: String,
    created: bool,
}

impl Database {
    fn new() -> Self {
        let mut database = Self::reserve();
        database.create();
        database
    }

    /// A name and URL for a database that does not exist yet, so a test can
    /// interrupt a run on an unreachable database and then create it.
    fn reserve() -> Self {
        let admin_url = base_url();
        let name = format!("githearth_setup_{}", Uuid::new_v4().simple());
        // Connect to the new database explicitly through the options so a
        // search_path or another parameter in the base URL cannot redirect it.
        let url = with_database(&admin_url, &name);
        Self {
            admin_url,
            name,
            url,
            created: false,
        }
    }

    fn create(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        runtime().block_on(async {
            let pool = sqlx::PgPool::connect(&admin_url).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&pool)
                .await
                .expect("cannot create the test database");
            pool.close().await;
        });
        self.created = true;
    }

    /// Counts rows matching `condition`, or 0 when `table` does not exist yet:
    /// a setup that stopped before `migrate` is inspected without a panic.
    fn count(&self, table: &str, condition: &str) -> i64 {
        let url = self.url.clone();
        runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            if !table_exists(&pool, table).await {
                pool.close().await;
                return 0;
            }
            let value: i64 =
                sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE {condition}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            pool.close().await;
            value
        })
    }
}

async fn table_exists(pool: &sqlx::PgPool, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_name = $1)",
    )
    .bind(table)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

impl Drop for Database {
    fn drop(&mut self) {
        if !self.created {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&admin_url).await?;
            // Terminate leftover sessions so the drop cannot wait forever.
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid()",
            )
            .bind(&name)
            .execute(&pool)
            .await;
            sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
                .execute(&pool)
                .await?;
            pool.close().await;
            Ok::<(), sqlx::Error>(())
        });
    }
}

/// Replaces the database name in a PostgreSQL URL, keeping any query string.
fn with_database(base: &str, database: &str) -> String {
    let (without_query, query) = match base.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (base, None),
    };
    let head = without_query.trim_end_matches('/');
    let slash = head.rfind('/').expect("database URL has a path");
    let mut url = format!("{}/{database}", &head[..slash]);
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    url
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn nonce() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// `setup` and `migrate` take database-wide locks and the server bounds lock
/// waits; run one database command at a time as `tests/binary.rs` does.
static RUN: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    RUN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A private temporary installation directory.
struct Installation {
    path: PathBuf,
    /// Fixed for the installation's lifetime: a repeated `setup` must pass the
    /// same settings, and the configuration records the port it listened on.
    http_port: u16,
    ssh_port: u16,
}

impl Installation {
    fn new() -> Self {
        // Create it private from the start: the parent is the shared temp dir.
        // The name stays short because storage lives below it and the hook
        // socket path is bounded by the Unix socket limit.
        let path = std::env::temp_dir().join(format!("ghm41-{}-{}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let http_port = free_port();
        let mut ssh_port = free_port();
        while ssh_port == http_port {
            ssh_port = free_port();
        }
        Self {
            path,
            http_port,
            ssh_port,
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn config(&self) -> PathBuf {
        self.join("githearth/config.toml")
    }

    fn host_key(&self) -> PathBuf {
        self.join("githearth/ssh_host_key")
    }

    fn storage(&self) -> PathBuf {
        self.join("data/repositories")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
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

fn run(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.output().unwrap()
}

fn run_with_stdin(mut command: Command, input: &[u8]) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn both(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// The password passed in tests; every output is checked for it.
const PASSWORD: &str = "correct horse battery staple";

/// A `setup` invocation with explicit paths and a free HTTP port.
fn setup(installation: &Installation, url: &str) -> Command {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", url);
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "5");
    command.args([
        "setup",
        "--config",
        installation.config().to_str().unwrap(),
        "--storage-root",
        installation.storage().to_str().unwrap(),
        "--http-port",
        &installation.http_port.to_string(),
        "--ssh-port",
        &installation.ssh_port.to_string(),
    ]);
    command
}

fn setup_with_password(installation: &Installation, url: &str) -> Output {
    let _one = exclusive();
    run_with_stdin(setup(installation, url), PASSWORD.as_bytes())
}

fn setup_again(installation: &Installation, url: &str) -> Output {
    let _one = exclusive();
    run(setup(installation, url))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The HTTP status for `path`, or `None` when the listener is not answering yet.
fn http_status(port: u16, path: &str) -> Option<u16> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
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

/// Waits for `GET /health/ready` to return 200, giving up early if `serve`
/// exits. Returns false on timeout or an exited child.
fn wait_for_ready(child: &mut std::process::Child, port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if http_status(port, "/health/ready") == Some(200) {
            return true;
        }
        if child.try_wait().unwrap().is_some() {
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Reads the server's SSH identification line from a fresh connection.
fn ssh_banner(address: SocketAddr) -> Option<String> {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut banner = String::new();
    BufReader::new(stream).read_line(&mut banner).ok()?;
    Some(banner)
}

fn snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>, u32, i64, i64, u64)> {
    let mut entries = Vec::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(directory) = todo.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            let contents = if metadata.is_file() {
                fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            entries.push((
                entry.path(),
                contents,
                metadata.permissions().mode(),
                metadata.mtime_nsec(),
                metadata.mtime(),
                metadata.ino(),
            ));
            if metadata.is_dir() {
                todo.push(entry.path());
            }
        }
    }
    entries.sort();
    entries
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn admin_count(database: &Database) -> i64 {
    database.count("accounts", "is_admin")
}

fn audit_count(database: &Database) -> i64 {
    database.count("audit_events", "kind = 'administrator.bootstrapped'")
}

fn assert_no_secrets(output: &Output, url: &str) {
    let text = both(output);
    assert!(
        !text.contains(PASSWORD),
        "output contains the password: {text}"
    );
    assert!(
        !text.contains(url),
        "output contains the database URL: {text}"
    );
    // The database name is unique to this run; no fragment of the URL leaks either.
    let name = url.rsplit('/').next().unwrap().split('?').next().unwrap();
    assert!(
        !text.contains(name),
        "output contains the database name: {text}"
    );
    for line in text.lines() {
        assert!(
            !line.contains("postgres://") && !line.contains("postgresql://"),
            "output contains a connection URL: {line}"
        );
    }
}

#[test]
fn setup_help_documents_the_inputs() {
    let mut command = server();
    command.args(["setup", "--help"]);
    let output = run(command);
    assert!(output.status.success(), "{}", stderr(&output));
    let help = stdout(&output);
    assert!(help.contains("GITHEARTH_DATABASE_URL"), "{help}");
    assert!(help.contains("--no-ssh"), "{help}");
    assert!(help.contains("--storage-root"), "{help}");
    assert!(
        !help.contains("--database-url"),
        "the database URL must not be an argument: {help}"
    );
}

#[test]
fn setup_without_a_database_url_fails_before_writing_anything() {
    let installation = Installation::new();
    let mut command = server();
    command.args(["setup", "--config", installation.config().to_str().unwrap()]);
    let output = run_with_stdin(command, b"");
    assert!(!output.status.success(), "{}", both(&output));
    let err = stderr(&output);
    assert!(
        err.contains("set GITHEARTH_DATABASE_URL or write the connection URL to stdin"),
        "{err}"
    );
    assert!(
        !installation.config().exists(),
        "a failed setup must not leave a configuration behind"
    );
}

#[test]
fn setup_rejects_invalid_arguments_without_touching_the_disk() {
    let installation = Installation::new();
    let _one = exclusive();

    // Two configuration paths are ambiguous; clap refuses before any work.
    let mut command = setup(&installation, "postgres://localhost/db");
    command.args([
        "--config",
        installation.join("other.toml").to_str().unwrap(),
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("cannot be used multiple times"),
        "{}",
        stderr(&output)
    );

    let mut command = setup(&installation, "postgres://localhost/db");
    command.args(["--http-port", "0"]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // The database URL is deliberately not an argument.
    let mut command = setup(&installation, "postgres://localhost/db");
    command.args(["--database-url", "postgres://localhost/other"]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unexpected argument"),
        "{}",
        stderr(&output)
    );

    assert!(!installation.join("githearth").exists());
    assert!(!installation.join("data").exists());
}

#[test]
fn setup_refuses_the_global_config_together_with_config_path() {
    let installation = Installation::new();
    let _one = exclusive();
    // clap refuses the ambiguity: the tool cannot silently prefer one path.
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", "postgres://localhost/db");
    command.args([
        "setup",
        "--config",
        installation.join("global.toml").to_str().unwrap(),
        "--config-path",
        installation.join("sub.toml").to_str().unwrap(),
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("cannot be used with"), "{err}");
    assert!(
        !installation.join("global.toml").exists() && !installation.join("sub.toml").exists(),
        "an ambiguous run writes neither file"
    );

    // The global flag written *before* the subcommand is outside clap's
    // conflict scope, so `run_setup` refuses it instead of silently preferring
    // the global path and discarding `--config-path`. An existing global file
    // proves the refusal happens before it is inspected or consumed.
    let global = installation.join("global.toml");
    fs::write(
        &global,
        "bind = \"127.0.0.1:8080\"\nssh_bind = \"127.0.0.1:2222\"\n",
    )
    .unwrap();
    fs::set_permissions(&global, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&global).unwrap();
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", "postgres://localhost/db");
    command.args([
        "--config",
        global.to_str().unwrap(),
        "setup",
        "--config-path",
        installation.join("sub.toml").to_str().unwrap(),
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    // The same usage error and code as the clap-refused ordering above.
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("--config and --config-path"), "{err}");
    assert!(err.contains("only one of them"), "{err}");
    assert!(
        !err.contains("is a symbolic link") && !err.contains("has mode"),
        "{err}"
    );
    assert_eq!(
        fs::read(&global).unwrap(),
        before,
        "the refused run must not rewrite the global file"
    );
    assert!(
        !installation.join("sub.toml").exists(),
        "a refused pre-subcommand run writes neither file"
    );
}

#[test]
fn setup_refuses_the_same_port_for_http_and_ssh() {
    let installation = Installation::new();
    let _one = exclusive();
    let port = installation.http_port.to_string();
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", "postgres://localhost/db");
    command.args([
        "setup",
        "--config",
        installation.config().to_str().unwrap(),
        "--storage-root",
        installation.storage().to_str().unwrap(),
        "--http-port",
        &port,
        "--ssh-port",
        &port,
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let text = both(&output);
    assert!(text.contains("--http-port"), "{text}");
    assert!(text.contains("--ssh-port"), "{text}");
    assert!(text.contains("must differ"), "{text}");
    assert!(
        !installation.config().exists(),
        "the refused configuration is not written"
    );
}

#[test]
fn setup_with_a_non_postgres_url_is_rejected_without_echoing_it() {
    let installation = Installation::new();
    let _one = exclusive();
    let secret_url = "mysql://user:supersecret@127.0.0.1:1/db";
    let output = run_with_stdin(setup(&installation, secret_url), PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1));
    let text = both(&output);
    assert!(
        text.contains("database_url must be a PostgreSQL URL"),
        "{text}"
    );
    assert!(!text.contains("supersecret"), "{text}");
    assert!(!installation.config().exists());
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn clean_host_reaches_a_doctor_clean_running_installation() {
    let installation = Installation::new();
    let database = Database::new();
    let output = setup_with_password(&installation, &database.url);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("created      "), "{out}");
    assert!(out.contains("12 checks, 0 failed"), "{out}");
    assert!(out.contains("setup complete"), "{out}");
    assert_no_secrets(&output, &database.url);

    // The configuration is private, complete, and loads as the server does.
    let config_path = installation.config();
    assert_eq!(mode(&config_path), 0o600);
    let text = fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("ssh_enabled = true"), "{text}");
    assert!(
        text.contains(&format!(
            "ssh_bind = \"127.0.0.1:{}\"",
            installation.ssh_port
        )),
        "the requested --ssh-port is recorded: {text}"
    );
    assert!(
        text.contains(&format!(
            "ssh_host_key_path = {:?}",
            installation.host_key().to_str().unwrap()
        )),
        "{text}"
    );
    let mut command = server();
    command.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    let doctor = run(command);
    assert!(doctor.status.success(), "{}", stdout(&doctor));

    // Directories and the host key are private.
    for directory in [installation.join("githearth"), installation.storage()] {
        assert!(directory.is_dir(), "{} missing", directory.display());
        assert_eq!(mode(&directory), 0o700, "{}", directory.display());
    }
    assert_eq!(mode(&installation.host_key()), 0o600);
    let key = fs::read_to_string(installation.host_key()).unwrap();
    assert!(key.contains("OPENSSH PRIVATE KEY"), "not an SSH key");
    assert!(!key.contains(PASSWORD));

    // Exactly one administrator, with its audit record in the same database.
    assert_eq!(admin_count(&database), 1);
    assert_eq!(audit_count(&database), 1);

    // Running-ready: the written configuration serves and listens on both the
    // HTTP port and the SSH port recorded by `setup`.
    let port: u16 = fs::read_to_string(&config_path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("bind = ").map(str::to_owned))
        .and_then(|value| value.trim_matches('"').parse::<SocketAddr>().ok())
        .map(|address| address.port())
        .expect("bind in the written configuration");
    let mut serve = server();
    serve.args(["--config", config_path.to_str().unwrap(), "serve"]);
    serve
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = serve.spawn().unwrap();
    let ready = wait_for_ready(&mut child, port);
    let ssh_address: SocketAddr = format!("127.0.0.1:{}", installation.ssh_port)
        .parse()
        .unwrap();
    let banner = ssh_banner(ssh_address);
    let running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "GET /health/ready never returned 200 on {port}");
    assert!(
        banner
            .as_deref()
            .is_some_and(|banner| banner.starts_with("SSH-2.0-")),
        "the SSH listener never sent a banner on {}: {banner:?}",
        installation.ssh_port
    );
    assert!(
        running,
        "the serve child exited after both listeners accepted"
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn relative_paths_are_recorded_absolute_and_work_from_another_directory() {
    let installation = Installation::new();
    let database = Database::new();

    // Run from inside the installation with the relative values the sample
    // configuration suggests. Both `conf/` and `data/` are missing, so setup
    // must create them rather than fail on the empty ancestor.
    let _one = exclusive();
    let mut command = server();
    command.current_dir(&installation.path);
    command.env("GITHEARTH_DATABASE_URL", &database.url);
    command.args([
        "setup",
        "--config-path",
        "conf/config.toml",
        "--storage-root",
        "data/repositories",
        "--http-port",
        &installation.http_port.to_string(),
        "--ssh-port",
        &installation.ssh_port.to_string(),
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_no_secrets(&output, &database.url);

    let config_path = installation.join("conf/config.toml");
    let storage = installation.join("data/repositories");
    let key = installation.join("conf/ssh_host_key");
    assert_eq!(mode(&config_path), 0o600);
    assert_eq!(mode(&storage), 0o700);
    assert_eq!(mode(&key), 0o600);
    // getcwd resolves the temp-directory symlink, so compare with the resolved
    // base; the product deliberately does not resolve symlinks itself.
    let base = fs::canonicalize(&installation.path).unwrap();
    let text = fs::read_to_string(&config_path).unwrap();
    assert!(
        text.contains(&format!(
            "storage_root = {:?}",
            base.join("data/repositories").to_str().unwrap()
        )),
        "the storage root is recorded as an absolute path: {text}"
    );
    assert!(
        text.contains(&format!(
            "ssh_host_key_path = {:?}",
            base.join("conf/ssh_host_key").to_str().unwrap()
        )),
        "the host key path is recorded as an absolute path: {text}"
    );

    // A different working directory: doctor and serve use only the absolute
    // path in the file, so both must still work.
    let elsewhere = installation.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = server();
    command.current_dir(&elsewhere);
    command.args(["--config", config_path.to_str().unwrap(), "doctor"]);
    let doctor = run(command);
    assert!(
        doctor.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&doctor),
        stderr(&doctor)
    );

    let mut serve = server();
    serve.current_dir(&elsewhere);
    serve.args(["--config", config_path.to_str().unwrap(), "serve"]);
    serve
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = serve.spawn().unwrap();
    let ready = wait_for_ready(&mut child, installation.http_port);
    let ssh_address: SocketAddr = format!("127.0.0.1:{}", installation.ssh_port)
        .parse()
        .unwrap();
    let banner = ssh_banner(ssh_address);
    let running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        ready,
        "GET /health/ready never returned 200 on {}",
        installation.http_port
    );
    assert!(
        banner
            .as_deref()
            .is_some_and(|banner| banner.starts_with("SSH-2.0-")),
        "the SSH listener never sent a banner on {}: {banner:?}",
        installation.ssh_port
    );
    assert!(
        running,
        "the serve child exited after both listeners accepted"
    );
    assert_eq!(admin_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn a_resumed_run_uses_the_recorded_url_and_reads_no_stdin() {
    let installation = Installation::new();
    let database = Database::new();

    // First install: the URL arrives on stdin, so no environment variable is
    // left for the rerun. This is the documented pipe-only install path.
    let _one = exclusive();
    let mut first = setup(&installation, "");
    first.env_remove("GITHEARTH_DATABASE_URL");
    let input = format!("{}\n{PASSWORD}\n", database.url);
    let first = run_with_stdin(first, input.as_bytes());
    assert!(
        first.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&first),
        stderr(&first)
    );
    let before = snapshot(&installation.path);

    // Second run: no GITHEARTH_DATABASE_URL and empty stdin. The recorded URL
    // is used, nothing is created, and nothing waits for input.
    let mut second = setup(&installation, "");
    second.env_remove("GITHEARTH_DATABASE_URL");
    let second = run_with_stdin(second, b"");
    assert!(
        second.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&second),
        stderr(&second)
    );
    let out = stdout(&second);
    assert!(
        !out.contains("created      "),
        "nothing should be created: {out}"
    );
    assert!(out.contains("existing settings used"), "{out}");
    assert!(out.contains("administrator exists"), "{out}");
    assert!(out.contains("12 checks, 0 failed"), "{out}");
    assert_eq!(
        before,
        snapshot(&installation.path),
        "a resumed setup must leave every file unchanged"
    );
    assert_eq!(admin_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn a_group_or_other_readable_existing_configuration_is_refused() {
    let installation = Installation::new();
    let database = Database::new();
    let first = setup_with_password(&installation, &database.url);
    assert!(first.status.success(), "{}", both(&first));

    // A manual or copied file left at 0644 exposes the database URL to every
    // local user; setup refuses it rather than claiming a mode it never read.
    fs::set_permissions(installation.config(), fs::Permissions::from_mode(0o644)).unwrap();
    let before = fs::read(installation.config()).unwrap();

    let _one = exclusive();
    let output = run(setup(&installation, &database.url));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let err = stderr(&output);
    assert!(err.contains("has mode 0644"), "{err}");
    assert!(err.contains("chmod 0600"), "{err}");
    assert!(
        !err.contains(&database.url),
        "the URL stays unprinted: {err}"
    );
    assert_eq!(
        mode(&installation.config()),
        0o644,
        "the mode is not changed"
    );
    assert_eq!(fs::read(installation.config()).unwrap(), before);
    assert_eq!(admin_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn a_symlinked_existing_configuration_is_refused() {
    let installation = Installation::new();
    let database = Database::new();
    let first = setup_with_password(&installation, &database.url);
    assert!(first.status.success(), "{}", both(&first));

    // An attacker with write access to the config directory could swap in a
    // symlink; setup must refuse it exactly as `write_config` always has.
    let real = installation.join("real-config.toml");
    fs::rename(installation.config(), &real).unwrap();
    std::os::unix::fs::symlink(&real, installation.config()).unwrap();

    let _one = exclusive();
    let output = run(setup(&installation, &database.url));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let err = stderr(&output);
    assert!(err.contains("is a symbolic link"), "{err}");
    assert!(
        fs::symlink_metadata(installation.config())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink is left in place"
    );
    assert_eq!(mode(&real), 0o600);
    assert_eq!(admin_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn second_run_changes_nothing_and_asks_for_nothing() {
    let installation = Installation::new();
    let database = Database::new();
    let first = setup_with_password(&installation, &database.url);
    assert!(first.status.success(), "{}", both(&first));
    let before = snapshot(&installation.path);

    // No stdin at all: an installation that is complete never waits for input.
    let second = setup_again(&installation, &database.url);
    assert!(
        second.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&second),
        stderr(&second)
    );
    let out = stdout(&second);
    assert!(
        !out.contains("created      "),
        "nothing should be created: {out}"
    );
    assert!(out.contains("administrator exists"), "{out}");
    assert!(out.contains("up to date"), "{out}");
    assert!(out.contains("existing key kept"), "{out}");
    assert!(out.contains("12 checks, 0 failed"), "{out}");
    assert_no_secrets(&second, &database.url);

    let after = snapshot(&installation.path);
    assert_eq!(
        before, after,
        "a repeated setup must leave every file unchanged"
    );
    assert_eq!(mode(&installation.config()), 0o600);
    assert_eq!(admin_count(&database), 1);
    assert_eq!(audit_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn interrupted_run_resumes_and_finishes_only_the_missing_steps() {
    let installation = Installation::new();
    let mut database = Database::reserve();

    // First run stops at the database: everything before it is done, and the
    // configuration already records the URL the operator will use.
    let mut command = setup(&installation, &database.url);
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "1");
    let failed = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(failed.status.code(), Some(1));
    let err = stderr(&failed);
    assert!(err.contains("stopped during the schema step"), "{err}");
    assert!(
        err.contains("completed: configuration, installation key, storage directory, SSH host key"),
        "{err}"
    );
    assert!(err.contains("run it again to resume"), "{err}");
    assert!(err.contains("cannot connect to PostgreSQL"), "{err}");
    // The URL came through the environment; it is never echoed.
    assert!(!both(&failed).contains(&database.url), "{}", both(&failed));
    assert!(installation.config().exists());
    assert!(installation.host_key().exists());
    assert!(
        installation
            .path
            .join("githearth/installation.key")
            .exists(),
        "the interrupted run creates the installation key before the schema"
    );
    assert_eq!(
        fs::read_to_string(installation.config())
            .unwrap()
            .matches(&database.url)
            .count(),
        1
    );

    // The same failure again: the configuration, storage and host key are
    // already present, so the resumed failure must name them instead of
    // claiming that no step completed.
    let mut again = setup(&installation, &database.url);
    again.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "1");
    let failed_again = run_with_stdin(again, PASSWORD.as_bytes());
    assert_eq!(failed_again.status.code(), Some(1));
    let err = stderr(&failed_again);
    assert!(err.contains("stopped during the schema step"), "{err}");
    assert!(
        err.contains("completed: configuration, installation key, storage directory, SSH host key"),
        "{err}"
    );
    assert!(!err.contains("no step completed"), "{err}");

    // The operator fixes the database and runs the same command again.
    database.create();
    let config_before = fs::read(installation.config()).unwrap();
    let key_before = fs::read(installation.host_key()).unwrap();
    let resumed = setup_with_password(&installation, &database.url);
    assert!(
        resumed.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&resumed),
        stderr(&resumed)
    );
    let out = stdout(&resumed);
    assert!(out.contains("unchanged    "), "{out}");
    assert!(out.contains("created      migrated to v"), "{out}");
    assert!(out.contains("first administrator"), "{out}");
    assert_eq!(fs::read(installation.config()).unwrap(), config_before);
    assert_eq!(fs::read(installation.host_key()).unwrap(), key_before);
    assert_eq!(admin_count(&database), 1);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn run_that_stopped_before_the_administrator_resumes_it() {
    let installation = Installation::new();
    let database = Database::new();

    // No password: schema and host key are prepared, the administrator is not.
    let _one = exclusive();
    let failed = run(setup(&installation, &database.url));
    assert_eq!(failed.status.code(), Some(1));
    let err = stderr(&failed);
    assert!(
        err.contains("stopped during the administrator step"),
        "{err}"
    );
    assert!(
        err.contains("no administrator exists and no password was read"),
        "{err}"
    );
    assert_eq!(admin_count(&database), 0);

    // The operator notices and pipes a password: setup completes.
    let resumed = run_with_stdin(setup(&installation, &database.url), PASSWORD.as_bytes());
    assert!(resumed.status.success(), "{}", both(&resumed));
    assert_eq!(admin_count(&database), 1);
    assert_no_secrets(&resumed, &database.url);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn group_or_world_writable_directories_are_refused() {
    let installation = Installation::new();
    let database = Database::new();

    for (name, permissions) in [("data", 0o775), ("data/repositories", 0o777)] {
        let root = installation.join(name);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(permissions)).unwrap();
    }
    // The host key directory is checked as well, with a path given explicitly.
    let key_dir = installation.join("keys");
    fs::create_dir_all(&key_dir).unwrap();
    fs::set_permissions(&key_dir, fs::Permissions::from_mode(0o770)).unwrap();

    let _one = exclusive();
    let mut command = setup(&installation, &database.url);
    command.args([
        "--ssh-host-key-path",
        key_dir.join("ssh_host_key").to_str().unwrap(),
    ]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let err = stderr(&output);
    assert!(
        err.contains(&installation.join("data").display().to_string()),
        "{err}"
    );
    assert!(err.contains("group or others could write to it"), "{err}");
    assert!(err.contains("chmod 0700"), "{err}");
    assert!(
        err.contains("stopped during the storage directory step"),
        "{err}"
    );
    assert!(!installation.host_key().exists());
    assert_eq!(admin_count(&database), 0);

    // Fix the permissions and the same command completes.
    fs::set_permissions(installation.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(
        installation.join("data/repositories"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let host_key = key_dir.join("ssh_host_key");
    fs::set_permissions(&key_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = setup(&installation, &database.url);
    command.args(["--ssh-host-key-path", host_key.to_str().unwrap()]);
    let fixed = run_with_stdin(command, PASSWORD.as_bytes());
    assert!(fixed.status.success(), "{}", both(&fixed));
    assert_eq!(mode(&host_key), 0o600);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn a_missing_database_reports_a_sanitized_error_and_the_url_can_be_corrected() {
    let installation = Installation::new();
    let database = Database::new();
    let missing = with_database(&database.url, "githearth_setup_missing_database");
    let _one = exclusive();
    let mut command = setup(&installation, &missing);
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "1");
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains("cannot connect to PostgreSQL; check database availability and credentials"),
        "{err}"
    );
    assert!(err.contains("stopped during the schema step"), "{err}");
    assert_no_secrets(&output, &missing);
    assert!(!err.contains("githearth_setup_missing_database"), "{err}");
    assert!(installation.config().exists());
    assert!(installation.host_key().exists());
    assert_eq!(
        fs::read_to_string(installation.config())
            .unwrap()
            .matches(&missing)
            .count(),
        1
    );

    // A corrected URL in the environment alone cannot repair the run: the
    // recorded value is authoritative, and the error says to edit the file.
    let refused = run(setup(&installation, &database.url));
    assert_eq!(refused.status.code(), Some(1));
    let err = stderr(&refused);
    assert!(err.contains("records a different database URL"), "{err}");
    assert!(err.contains("correct `database_url` in the file"), "{err}");

    // The documented recovery: correct `database_url` in the configuration
    // file, then run the same command again.
    let text = fs::read_to_string(installation.config()).unwrap();
    let corrected = text.replace(&missing, &database.url);
    assert_ne!(corrected, text, "the recorded URL was replaced");
    fs::write(installation.config(), corrected).unwrap();
    fs::set_permissions(installation.config(), fs::Permissions::from_mode(0o600)).unwrap();
    let resumed = run_with_stdin(setup(&installation, &database.url), PASSWORD.as_bytes());
    assert!(
        resumed.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&resumed),
        stderr(&resumed)
    );
    assert_eq!(admin_count(&database), 1);
    assert_no_secrets(&resumed, &database.url);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn an_existing_configuration_is_never_overwritten() {
    let installation = Installation::new();
    let database = Database::new();
    let first = setup_with_password(&installation, &database.url);
    assert!(first.status.success(), "{}", both(&first));
    let original = fs::read(installation.config()).unwrap();

    // A second run naming a different database must fail, not replace the file
    // or migrate the database the file does not describe.
    let other = Database::new();
    let _one = exclusive();
    let output = run(setup(&installation, &other.url));
    assert_eq!(output.status.code(), Some(1), "{}", both(&output));
    let err = stderr(&output);
    assert!(err.contains("records a different database URL"), "{err}");
    assert!(
        err.contains("stopped during the configuration step"),
        "{err}"
    );
    assert!(
        !err.contains("database_url ="),
        "the URL is never printed: {err}"
    );
    assert_no_secrets(&output, &other.url);
    assert_eq!(fs::read(installation.config()).unwrap(), original);
    assert_eq!(admin_count(&other), 0, "the other database is untouched");
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn an_operators_edit_is_kept_and_the_rerun_uses_it() {
    let installation = Installation::new();
    let database = Database::new();
    let first = setup_with_password(&installation, &database.url);
    assert!(first.status.success(), "{}", both(&first));

    // The operator moves storage and turns off secure cookies over loopback
    // HTTP: the next `setup` must use those values, not overwrite them.
    let moved = installation.join("moved-repositories");
    let edited = fs::read_to_string(installation.config())
        .unwrap()
        .replace(
            &format!(
                "storage_root = {:?}",
                installation.storage().to_str().unwrap()
            ),
            &format!("storage_root = {:?}", moved.to_str().unwrap()),
        )
        .replace("secure_cookies = true", "secure_cookies = false");
    fs::write(installation.config(), &edited).unwrap();
    fs::set_permissions(installation.config(), fs::Permissions::from_mode(0o600)).unwrap();

    let second = setup_again(&installation, &database.url);
    assert!(
        second.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&second),
        stderr(&second)
    );
    let out = stdout(&second);
    assert!(out.contains("unchanged"), "{out}");
    assert_eq!(fs::read_to_string(installation.config()).unwrap(), edited);
    assert!(moved.is_dir(), "the edited storage root is created");
    assert_eq!(mode(&moved), 0o700);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn an_unparseable_existing_configuration_is_left_alone() {
    let installation = Installation::new();
    let database = Database::new();
    fs::create_dir_all(installation.join("githearth")).unwrap();
    fs::set_permissions(
        installation.join("githearth"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(installation.config(), "this is not TOML").unwrap();
    // The mode is checked before the contents, so a private file is what
    // reaches the parser this test is about.
    fs::set_permissions(installation.config(), fs::Permissions::from_mode(0o600)).unwrap();
    let _one = exclusive();
    let output = run(setup(&installation, &database.url));
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("is not valid TOML"), "{err}");
    assert!(err.contains("was left unchanged"), "{err}");
    assert_eq!(
        fs::read_to_string(installation.config()).unwrap(),
        "this is not TOML"
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn a_broken_or_symlinked_host_key_is_refused() {
    let installation = Installation::new();
    let database = Database::new();
    fs::create_dir_all(installation.join("githearth")).unwrap();
    fs::set_permissions(
        installation.join("githearth"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(installation.host_key(), "not a key").unwrap();
    fs::set_permissions(installation.host_key(), fs::Permissions::from_mode(0o600)).unwrap();

    let _one = exclusive();
    let output = run_with_stdin(setup(&installation, &database.url), PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("is not a private SSH host key"), "{err}");
    assert_eq!(
        fs::read_to_string(installation.host_key()).unwrap(),
        "not a key"
    );
    assert_eq!(admin_count(&database), 0);

    // A symlink is refused even when its target is a valid key.
    fs::remove_file(installation.host_key()).unwrap();
    let elsewhere = installation.join("elsewhere-key");
    let mut keygen = server();
    keygen
        .env("GITHEARTH_DATABASE_URL", &database.url)
        .env("GITHEARTH_SSH_ENABLED", "true")
        .env("GITHEARTH_SSH_HOST_KEY_PATH", &elsewhere);
    keygen.arg("ssh-keygen");
    let keygen = run(keygen);
    assert!(keygen.status.success(), "{}", stderr(&keygen));
    std::os::unix::fs::symlink(&elsewhere, installation.host_key()).unwrap();
    let output = run_with_stdin(setup(&installation, &database.url), PASSWORD.as_bytes());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is a symbolic link"),
        "{}",
        stderr(&output)
    );
    assert_eq!(admin_count(&database), 0);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn concurrent_setups_create_one_administrator_and_one_configuration() {
    let installation = Installation::new();
    let database = Database::new();
    let mut commands = Vec::new();
    for _ in 0..3 {
        let mut command = setup(&installation, &database.url);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        commands.push(command);
    }
    // Started together on purpose: setup must serialise through the database
    // (migration and bootstrap locks), not by refusing the second caller.
    let children: Vec<_> = commands
        .into_iter()
        .map(|mut command| {
            let mut child = command.spawn().unwrap();
            {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(PASSWORD.as_bytes())
                    .unwrap();
            }
            child
        })
        .collect();
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let failures: Vec<String> = outputs
        .iter()
        .filter(|output| !output.status.success())
        .map(both)
        .collect();
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(admin_count(&database), 1);
    assert_eq!(audit_count(&database), 1);
    assert_eq!(mode(&installation.config()), 0o600);
    assert_eq!(
        outputs
            .iter()
            .filter(|output| stdout(output).contains("created      first administrator"))
            .count(),
        1
    );
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn ssh_can_be_left_out_for_a_loopback_http_install() {
    let installation = Installation::new();
    let database = Database::new();
    let _one = exclusive();
    let mut command = setup(&installation, &database.url);
    command.args(["--no-ssh"]);
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert!(output.status.success(), "{}", both(&output));
    let out = stdout(&output);
    assert!(out.contains("skipped      SSH is disabled"), "{out}");
    assert!(
        !installation.host_key().exists(),
        "no host key when SSH is off"
    );
    let text = fs::read_to_string(installation.config()).unwrap();
    assert!(text.contains("ssh_enabled = false"), "{text}");
    assert!(text.contains("ssh_host_key_path = \"\""), "{text}");
    assert_no_secrets(&output, &database.url);
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn the_database_url_can_be_read_from_stdin_without_the_environment() {
    let installation = Installation::new();
    let database = Database::new();
    let _one = exclusive();
    let mut command = setup(&installation, "");
    command.env_remove("GITHEARTH_DATABASE_URL");
    // Line one is the URL, line two the password: neither is an argument.
    let input = format!("{}\n{PASSWORD}\n", database.url);
    let output = run_with_stdin(command, input.as_bytes());
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_no_secrets(&output, &database.url);
    assert_eq!(admin_count(&database), 1);
    assert_eq!(
        fs::read_to_string(installation.config())
            .unwrap()
            .matches(&database.url)
            .count(),
        1
    );
}

#[test]
fn an_empty_stdin_without_the_environment_reports_what_is_missing() {
    let installation = Installation::new();
    let _one = exclusive();
    let mut command = setup(&installation, "");
    command.env_remove("GITHEARTH_DATABASE_URL");
    let output = run_with_stdin(command, b"");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("set GITHEARTH_DATABASE_URL"),
        "{}",
        stderr(&output)
    );
    assert!(!installation.config().exists());
}

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn the_environment_url_is_used_before_stdin() {
    let installation = Installation::new();
    let database = Database::new();
    let missing = with_database(&database.url, "githearth_setup_env_wins");
    let _one = exclusive();
    // With GITHEARTH_DATABASE_URL set, stdin carries only the password.
    let mut command = setup(&installation, &missing);
    command.env("GITHEARTH_DATABASE_URL", &database.url);
    command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "5");
    let output = run_with_stdin(command, PASSWORD.as_bytes());
    assert!(output.status.success(), "{}", both(&output));
    // The installed configuration records the URL from the environment; the
    // URL passed to this test's helper was never used.
    let text = fs::read_to_string(installation.config()).unwrap();
    assert!(
        text.contains(&format!("database_url = {:?}", database.url)),
        "{text}"
    );
    assert!(!text.contains(&missing) && !text.contains("githearth_setup_env_wins"));
    assert_eq!(admin_count(&database), 1);
    assert_no_secrets(&output, &database.url);
}
