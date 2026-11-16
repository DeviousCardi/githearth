//! `backup`/`restore` and the maintenance write fence (M4.1 U13).
//!
//! Every case runs the real binary: `setup`, `serve`, the maintenance control
//! endpoint, `maintenance start`, `backup` and `restore`. PostgreSQL is used to
//! create and drop each case's scratch database, and to ask whether a restore
//! left a destination database behind; assertions about repositories, metadata
//! and keys go through the API, Git or the restored files.
use githearth_git::object_store::{
    ByteRange, FsObjectStore, ObjectKey, ObjectStore, ObjectStoreError, TenantId,
};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple for backup";

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

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn nonce() -> usize {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Database commands take database-wide locks; do not run two at once.
static DB: Mutex<()> = Mutex::new(());

fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn with_database(base: &str, database: &str) -> String {
    let (head, query) = match base.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (base, None),
    };
    let head = head.trim_end_matches('/');
    let slash = head.rfind('/').expect("database URL has a path");
    let mut url = format!("{}/{database}", &head[..slash]);
    if let Some(query) = query {
        url.push('?');
        url.push_str(query);
    }
    url
}

struct Database {
    admin_url: String,
    name: String,
    url: String,
    created: bool,
}

impl Database {
    fn new(label: &str) -> Self {
        let mut database = Self::reserve(label);
        database.create();
        database
    }

    fn reserve(label: &str) -> Self {
        let admin_url = base_url();
        let name = format!("githearth_b6_{}_{}", label, Uuid::new_v4().simple());
        let url = with_database(&admin_url, &name);
        Self {
            admin_url,
            name,
            url,
            created: false,
        }
    }

    fn create(&mut self) {
        let _one = exclusive();
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        runtime().block_on(async {
            let pool = sqlx::PgPool::connect(&admin_url).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE {}", quote_ident(&name)))
                .execute(&pool)
                .await
                .expect("cannot create the test database");
            pool.close().await;
        });
        self.created = true;
    }

    fn exists(&self) -> bool {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        runtime().block_on(async {
            let pool = sqlx::PgPool::connect(&admin_url).await.unwrap();
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
                    .bind(&name)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            pool.close().await;
            exists
        })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if !self.created {
            return;
        }
        let _one = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&admin_url).await?;
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid()",
            )
            .bind(&name)
            .execute(&pool)
            .await;
            sqlx::query(&format!("DROP DATABASE IF EXISTS {}", quote_ident(&name)))
                .execute(&pool)
                .await?;
            pool.close().await;
            Ok::<(), sqlx::Error>(())
        });
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn both(output: &Output) -> String {
    format!("{}\n{}", stdout(output), stderr(output))
}

struct Installation {
    path: PathBuf,
    database: Database,
    http_port: u16,
    ssh_port: u16,
    child: Option<Child>,
    log: PathBuf,
}

impl Installation {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ghb6-{}-{}-{label}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let mut http_port = free_port();
        let mut ssh_port = free_port();
        while ssh_port == http_port {
            ssh_port = free_port();
        }
        http_port = http_port.max(1);
        Self {
            log: path.join("serve.log"),
            path,
            database: Database::new(label),
            http_port,
            ssh_port,
            child: None,
        }
    }

    fn reserve(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ghb6-{}-{}-{label}-restore",
            std::process::id(),
            nonce()
        ));
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            log: path.join("serve.log"),
            path,
            database: Database::reserve(label),
            http_port: free_port(),
            ssh_port: free_port(),
            child: None,
        }
    }

    fn config(&self) -> PathBuf {
        self.path.join("githearth/config.toml")
    }

    fn storage(&self) -> PathBuf {
        self.path.join("data/repositories")
    }

    fn host_key(&self) -> PathBuf {
        self.path.join("githearth/ssh_host_key")
    }

    fn setup(&self) {
        let _one = exclusive();
        let mut command = server();
        command.env("GITHEARTH_DATABASE_URL", &self.database.url);
        command.env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "5");
        command.args([
            "setup",
            "--config",
            self.config().to_str().unwrap(),
            "--storage-root",
            self.storage().to_str().unwrap(),
            "--http-port",
            &self.http_port.to_string(),
            "--ssh-port",
            &self.ssh_port.to_string(),
            "--ssh-host-key-path",
            self.host_key().to_str().unwrap(),
        ]);
        let output = run_with_stdin(command, PASSWORD.as_bytes());
        assert!(output.status.success(), "setup failed: {}", both(&output));
        assert!(
            stdout(&output).contains("setup complete"),
            "{}",
            stdout(&output)
        );
    }

    fn append_config(&self, lines: &str) {
        let mut text = fs::read_to_string(self.config()).unwrap();
        text.push('\n');
        text.push_str(lines);
        text.push('\n');
        fs::write(self.config(), text).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(self.config(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn start(&mut self) {
        self.start_with_env(&[]);
    }

    fn start_with_env(&mut self, env: &[(&str, &str)]) {
        let log = fs::File::create(&self.log).unwrap();
        let err = log.try_clone().unwrap();
        let mut command = server();
        for (name, value) in env {
            command.env(name, value);
        }
        command
            .args(["--config", self.config().to_str().unwrap(), "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err));
        let mut child = command.spawn().unwrap();
        assert!(
            wait_ready(&mut child, self.http_port),
            "serve never became ready: {}",
            fs::read_to_string(&self.log).unwrap_or_default()
        );
        self.child = Some(child);
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        wait_refused(self.http_port, self.ssh_port);
    }

    fn url(&self) -> String {
        let password = PASSWORD.replace(' ', "%20");
        format!(
            "http://admin:{password}@127.0.0.1:{}/acme/widget.git",
            self.http_port
        )
    }

    fn login(&self) -> String {
        let body = serde_json::json!({"username":"admin","password":PASSWORD}).to_string();
        let (status, body) = http(self.http_port, "POST", "/api/v1/login", None, Some(&body));
        assert_eq!(status, 200, "login: {body}");
        serde_json::from_str::<Value>(&body).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn create_org_and_repo(&self) -> Value {
        let token = self.login();
        let (status, body) = http(
            self.http_port,
            "POST",
            "/api/v1/organizations",
            Some(&token),
            Some(r#"{"slug":"acme","name":"Acme"}"#),
        );
        assert_eq!(status, 201, "create organization: {body}");
        let (status, body) = http(
            self.http_port,
            "POST",
            "/api/v1/organizations/acme/repositories",
            Some(&token),
            Some(r#"{"name":"widget","visibility":"private"}"#),
        );
        assert_eq!(status, 201, "create repository: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        http(self.http_port, method, path, token, body)
    }

    fn maintenance_token(&self) -> String {
        fs::read_to_string(self.path.join("data/repositories/run/maintenance.token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn maintenance(&self, method: &str, path: &str) -> githearth_server::maintenance::Status {
        let token = self.maintenance_token();
        let (status, body) = self.http(method, path, Some(&token), None);
        assert_eq!(status, 200, "maintenance {path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn backup(&self, output: &Path, key: &Path) -> Output {
        let mut command = server();
        command.args([
            "--config",
            self.config().to_str().unwrap(),
            "backup",
            "--output",
            output.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ]);
        run(command)
    }

    fn backup_held_at_dump(&self, output: &Path, key: &Path, marker: &Path) -> Command {
        let mut command = server();
        command.env("GITHEARTH_TEST_HOLD_BACKUP_AFTER_DUMP", marker);
        command.args([
            "--config",
            self.config().to_str().unwrap(),
            "backup",
            "--output",
            output.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ]);
        command
    }

    /// Runs one read-only query against this installation's database.
    fn database_scalar(&self, sql: &str) -> i64 {
        let url = self.database.url.clone();
        let sql = sql.to_owned();
        runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            let value = sqlx::query_scalar(&sql).fetch_one(&pool).await.unwrap();
            pool.close().await;
            value
        })
    }

    fn push_count(&self) -> i64 {
        self.database_scalar("SELECT count(*) FROM repository_pushes")
    }

    /// Enqueues a housekeeping job directly, as an unarchive or an operator
    /// follow-up would. The scheduler cannot be relied on in a short test.
    fn enqueue_housekeeping(&self, repository: Uuid) {
        let url = self.database.url.clone();
        runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&url).await.unwrap();
            sqlx::query(
                "INSERT INTO jobs (id, kind, payload, dedupe_key, not_before) \
                 VALUES ($1, 'repository.housekeeping', $2, $3, now())",
            )
            .bind(Uuid::new_v4())
            .bind(serde_json::json!({"repository_id": repository.to_string()}))
            .bind(repository.to_string())
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        });
    }

    fn ssh_url(&self) -> String {
        format!("ssh://git@127.0.0.1:{}/acme/widget.git", self.ssh_port)
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn wait_ready(child: &mut Child, port: u16) -> bool {
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
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_refused(http_port: u16, ssh_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let http = TcpStream::connect_timeout(
            &format!("127.0.0.1:{http_port}").parse().unwrap(),
            Duration::from_millis(200),
        );
        let ssh = TcpStream::connect_timeout(
            &format!("127.0.0.1:{ssh_port}").parse().unwrap(),
            Duration::from_millis(200),
        );
        if http.is_err() && ssh.is_err() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("listeners did not stop accepting connections");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn http_status(port: u16, path: &str) -> Option<u16> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500)).ok()?;
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

fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let body = body.unwrap_or("");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if body.starts_with('{') {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response has a body separator");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP response has a status");
    (status, body.to_owned())
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn git(args: &[&str], cwd: &Path) -> Output {
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSL_NO_VERIFY", "1")
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn git_ok(args: &[&str], cwd: &Path) {
    let output = git(args, cwd);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_push(work: &Path, message: &str, url: &str) -> String {
    fs::write(
        work.join(format!("{}.txt", message.replace(' ', "-"))),
        message,
    )
    .unwrap();
    git_ok(&["add", "."], work);
    git_ok(&["commit", "-m", message], work);
    let sha = String::from_utf8(git(&["rev-parse", "HEAD"], work).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git_ok(&["push", url, "main"], work);
    sha
}

fn prepare_work(work: &Path) {
    fs::create_dir_all(work).unwrap();
    git_ok(&["init", "-b", "main", "."], work);
    git_ok(&["config", "user.email", "test@example.com"], work);
    git_ok(&["config", "user.name", "Test"], work);
}

fn slow_hook_script(path: &Path, marker: &Path) {
    let real = env!("CARGO_BIN_EXE_githearth-server");
    let script = format!(
        "#!/bin/sh\nprintf 'hook\\n' >> '{}'\nsleep 2\nexec '{}' \"$@\"\n",
        marker.display(),
        real
    );
    fs::write(path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_marker(marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "push never reached the hook");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_drained(installation: &Installation) -> githearth_server::maintenance::Status {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
        if status.phase == githearth_server::maintenance::Phase::Drained {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "maintenance never drained: {status:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Waits until no admitted write is in flight, so a test starts from a quiet
/// fence rather than from the tail of its own push.
fn wait_no_in_flight(installation: &Installation) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
        if status.phase == githearth_server::maintenance::Phase::Active && status.in_flight == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "writes never settled: {status:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Waits for the journal rows a push produces, independent of the fence count:
/// a test that is checking the fence must not use `in_flight` to decide when
/// the replay finished, or it would accept the very bug it is looking for.
fn wait_push_count(installation: &Installation, at_least: i64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while installation.push_count() < at_least {
        assert!(Instant::now() < deadline, "push journal row never appeared");
        thread::sleep(Duration::from_millis(50));
    }
}

fn release(marker: &Path) {
    fs::write(marker.with_extension("release"), b"release").unwrap();
}

/// Registers a client key on the admin account and returns its path.
fn ssh_client_key(installation: &Installation, token: &str) -> PathBuf {
    let key = installation.path.join("client_key");
    let output = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let public_key = fs::read_to_string(key.with_extension("pub")).unwrap();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/user/ssh-keys",
        Some(token),
        Some(&serde_json::json!({"name":"backup-test","public_key":public_key}).to_string()),
    );
    assert_eq!(status, 201, "register ssh key: {body}");
    key
}

fn ssh_git(key: &Path, port: u16, args: &[&str], cwd: &Path) -> Output {
    let command = format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -o IdentitiesOnly=yes -o BatchMode=yes",
        key.display()
    );
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", command)
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn key_file(directory: &Path, name: &str, secret: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, secret).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// Decrypts an archive and extracts the manifest into a fresh directory.
fn open_archive(archive: &Path, secret: &str) -> (PathBuf, String) {
    let work = std::env::temp_dir().join(format!("ghb6-open-{}-{}", std::process::id(), nonce()));
    fs::create_dir_all(&work).unwrap();
    let payload = work.join("payload.tar");
    let header =
        githearth_server::backup::decrypt_file(archive, &payload, secret.as_bytes()).unwrap();
    let root = work.join("root");
    fs::create_dir_all(&root).unwrap();
    let output = Command::new("tar")
        .args([
            "-xf",
            payload.to_str().unwrap(),
            "-C",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (root, header.key_id)
}

fn restore_command(
    config: &Path,
    archive: &Path,
    database_url: &str,
    storage: &Path,
    host_key: &Path,
    key: Option<&Path>,
) -> Command {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", database_url);
    command.args([
        "--config",
        config.to_str().unwrap(),
        "restore",
        "--archive",
        archive.to_str().unwrap(),
        "--storage-root",
        storage.to_str().unwrap(),
        "--ssh-host-key-path",
        host_key.to_str().unwrap(),
    ]);
    if let Some(key) = key {
        command.args(["--key-file", key.to_str().unwrap()]);
    }
    command
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn backup_refuses_while_serve_accepts_writes_and_proceeds_under_both_fences() {
    let mut installation = Installation::new("fences");
    installation.setup();
    // This case is the M4.1 fence itself, so it keeps the fallback path; the
    // online path and its automatic short gate are proved in online_backup.rs.
    installation.append_config("backup_snapshot_mode = \"fence\"");
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "first", &installation.url());

    let key = key_file(&installation.path, "backup.key", "fence-test-passphrase");
    let refused = installation.path.join("refused.ghbackup");
    let output = installation.backup(&refused, &key);
    assert!(
        !output.status.success(),
        "backup ran while the server was accepting writes"
    );
    let text = both(&output);
    assert!(text.contains("accepting writes"), "{text}");
    assert!(
        text.contains("maintenance start") || text.contains("stop it"),
        "{text}"
    );
    assert!(!refused.exists(), "a refused backup left an archive behind");

    // Fence 2: maintenance mode. The push's response can be delivered before
    // its journal replay has finished, so the test waits for the same drained
    // state `backup` will require.
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);
    let under_maintenance = installation.path.join("maintenance.ghbackup");
    let output = installation.backup(&under_maintenance, &key);
    assert!(
        output.status.success(),
        "backup under maintenance: {}",
        both(&output)
    );
    assert!(
        stdout(&output).contains("fence used: maintenance"),
        "{}",
        stdout(&output)
    );
    let (root, _) = open_archive(&under_maintenance, "fence-test-passphrase");
    let manifest = githearth_server::backup::read_manifest(&root).unwrap();
    assert_eq!(manifest.fence.kind, "maintenance");

    // Fence 1: the server is not running.
    installation.stop();
    let stopped = installation.path.join("stopped.ghbackup");
    let output = installation.backup(&stopped, &key);
    assert!(
        output.status.success(),
        "backup with the server stopped: {}",
        both(&output)
    );
    assert!(
        stdout(&output).contains("fence used: not-running"),
        "{}",
        stdout(&output)
    );
    let (root, _) = open_archive(&stopped, "fence-test-passphrase");
    let manifest = githearth_server::backup::read_manifest(&root).unwrap();
    assert_eq!(manifest.fence.kind, "not-running");
    assert!(manifest.fence.detail.contains("refused connections"));
}

#[test]
fn maintenance_rejects_writes_with_503_drains_and_reports_drained_only_when_done() {
    let mut installation = Installation::new("drain");
    installation.setup();
    let marker = installation.path.join("hook.marker");
    let hook = installation.path.join("slow-hook.sh");
    slow_hook_script(&hook, &marker);
    installation.append_config(&format!("hook_program = {:?}", hook.to_str().unwrap()));
    installation.start();
    installation.create_org_and_repo();
    let session = installation.login();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "before", &installation.url());
    let _ = fs::remove_file(&marker);

    fs::write(work.join("second.txt"), "second").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "second"], &work);
    let url = installation.url();
    let push = thread::spawn(move || git(&["push", &url, "main"], &work));
    wait_marker(&marker);

    let started = installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    assert_eq!(
        started.phase,
        githearth_server::maintenance::Phase::Draining,
        "the push must be counted in flight"
    );
    assert!(started.in_flight >= 1, "{started:?}");

    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations",
        Some(&session),
        Some(r#"{"slug":"blocked","name":"Blocked"}"#),
    );
    assert_eq!(
        status, 503,
        "a write during maintenance must be refused: {body}"
    );

    let push = push.join().unwrap();
    assert!(
        push.status.success(),
        "the push that started before the fence must be allowed to finish: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let drained = wait_drained(&installation);
    assert_eq!(drained.in_flight, 0);

    let (status, _) = installation.http(
        "POST",
        "/api/v1/organizations",
        Some(&session),
        Some(r#"{"slug":"still-blocked","name":"Still Blocked"}"#),
    );
    assert_eq!(
        status, 503,
        "drained means no in-flight work, not an open door"
    );

    let stopped = installation.maintenance("POST", githearth_server::maintenance::STOP_PATH);
    assert_eq!(stopped.phase, githearth_server::maintenance::Phase::Active);
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations",
        Some(&session),
        Some(r#"{"slug":"after","name":"After"}"#),
    );
    assert_eq!(status, 201, "writes resume after maintenance stops: {body}");
}

#[test]
fn a_push_racing_the_snapshot_is_in_the_restore_or_was_refused() {
    let mut installation = Installation::new("race");
    installation.setup();
    let marker = installation.path.join("hook.marker");
    let hook = installation.path.join("slow-hook.sh");
    slow_hook_script(&hook, &marker);
    installation.append_config(&format!("hook_program = {:?}", hook.to_str().unwrap()));
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "baseline", &installation.url());
    let _ = fs::remove_file(&marker);

    fs::write(work.join("racing.txt"), "racing").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "racing"], &work);
    let racing_sha = String::from_utf8(git(&["rev-parse", "HEAD"], &work).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let url = installation.url();
    let push_work = work.clone();
    let push = thread::spawn(move || git(&["push", &url, "main"], &push_work));
    wait_marker(&marker);

    // The fence is raised while receive-pack is still running. Its permit keeps
    // the phase draining until the refs and journal are durable.
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    let push = push.join().unwrap();
    assert!(
        push.status.success(),
        "a push started before the fence is drained, not killed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    wait_drained(&installation);

    // A push that starts after the fence is refused outright.
    fs::write(work.join("refused.txt"), "refused").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "refused"], &work);
    let refused_sha = String::from_utf8(git(&["rev-parse", "HEAD"], &work).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let refused = git(&["push", &installation.url(), "main"], &work);
    assert!(
        !refused.status.success(),
        "a push after the fence must be refused"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("503")
            || String::from_utf8_lossy(&refused.stderr).contains("maintenance"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let key = key_file(&installation.path, "race.key", "race-passphrase");
    let archive = installation.path.join("race.ghbackup");
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));

    // Restore on a fresh database and storage, then prove what the snapshot
    // says about both commits.
    installation.stop();
    let mut destination = Installation::reserve("race-dest");
    let restored_storage = destination.path.join("restored-storage");
    let restored_config = destination.path.join("restored-config.toml");
    let key_for_restore = key_file(&destination.path, "race.key", "race-passphrase");
    // The restore creates this database; drop it even if the case panics.
    destination.database.created = true;
    let output = run(restore_command(
        &restored_config,
        &archive,
        &destination.database.url,
        &restored_storage,
        &destination.path.join("restored-ssh-host-key"),
        Some(&key_for_restore),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));

    let bare = repo_path(&restored_storage, repo_id);
    assert!(
        cat_file(&bare, &racing_sha),
        "the commit of the push that started before the snapshot is missing"
    );
    assert!(
        !cat_file(&bare, &refused_sha),
        "a push refused during maintenance must not appear in the restore"
    );
}

#[test]
fn restore_reproduces_repositories_metadata_keys_and_relationships() {
    let mut installation = Installation::new("reproduce");
    installation.setup();
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let token = installation.login();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/repositories/widget/issues",
        Some(&token),
        Some(r#"{"title":"Restore me","body":"relationship"}"#),
    );
    assert_eq!(status, 201, "create issue: {body}");

    let keygen = installation.path.join("account_key");
    let keygen_output = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&keygen)
        .output()
        .unwrap();
    assert!(keygen_output.status.success());
    let public_key = fs::read_to_string(keygen.with_extension("pub")).unwrap();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/user/ssh-keys",
        Some(&token),
        Some(&serde_json::json!({"name":"laptop","public_key":public_key}).to_string()),
    );
    assert_eq!(status, 201, "create ssh key: {body}");

    let work = installation.path.join("work");
    prepare_work(&work);
    let commit = commit_push(&work, "restore-me", &installation.url());

    let original_host_key = fs::read(installation.host_key()).unwrap();
    let key = key_file(&installation.path, "reproduce.key", "reproduce-passphrase");
    let archive = installation.path.join("reproduce.ghbackup");
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));
    installation.stop();

    let mut destination = Installation::reserve("reproduce-dest");
    let restored_storage = destination.path.join("restored-storage");
    let restored_config = destination.path.join("restored-config.toml");
    // The restore creates this database; drop it even if the case panics.
    destination.database.created = true;
    let output = run(restore_command(
        &restored_config,
        &archive,
        &destination.database.url,
        &restored_storage,
        &destination.path.join("restored-ssh-host-key"),
        Some(&key),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));
    assert_eq!(
        fs::read(destination.path.join("restored-ssh-host-key")).unwrap(),
        original_host_key,
        "the host key must survive the round trip"
    );

    // Start the restored installation and read the relationships back. The
    // restored configuration already names the restored database and storage;
    // it uses the archived bind address, which is free now.
    let log = fs::File::create(&destination.log).unwrap();
    let err = log.try_clone().unwrap();
    let mut command = server();
    command
        .args(["--config", restored_config.to_str().unwrap(), "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    let mut child = command.spawn().unwrap();
    assert!(
        wait_ready(&mut child, installation.http_port),
        "restored serve never became ready: {}",
        fs::read_to_string(&destination.log).unwrap_or_default()
    );
    destination.child = Some(child);
    destination.http_port = installation.http_port;

    let token = destination.login();
    let (status, body) = destination.http("GET", "/api/v1/organizations", Some(&token), None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("acme"), "{body}");
    let (status, body) = destination.http(
        "GET",
        "/api/v1/organizations/acme/repositories/widget",
        Some(&token),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = destination.http(
        "GET",
        "/api/v1/organizations/acme/repositories/widget/issues/1",
        Some(&token),
        None,
    );
    assert_eq!(status, 200, "restored issue: {body}");
    assert!(body.contains("Restore me"), "{body}");
    let (status, body) = destination.http("GET", "/api/v1/user/ssh-keys", Some(&token), None);
    assert_eq!(status, 200, "restored ssh keys: {body}");
    assert!(body.contains("laptop"), "{body}");

    let bare = repo_path(&restored_storage, repo_id);
    assert!(
        cat_file(&bare, &commit),
        "restored repository is missing its commit"
    );
    assert!(
        !both(&output).contains(PASSWORD) && !both(&output).contains("reproduce-passphrase"),
        "the restore output contains a secret"
    );
}

/// U24: an object written through the interface survives restart and
/// `backup` → `restore` into a fresh installation; a ranged read and an
/// appended log are correct after the restart; a corrupted object is refused.
#[test]
fn objects_survive_restart_and_backup_restore_and_corruption_is_refused() {
    let installation = Installation::new("objects");
    installation.setup();
    // `serve` normally creates the storage root's repository directories on
    // first start; this case uses the object store only, so open the same
    // layout the server would.
    githearth_git::layout::Storage::open(installation.storage()).unwrap();
    let tenant = TenantId::new(Uuid::new_v4());
    let artifact = ObjectKey::parse("ci/artifacts/release.bin").unwrap();
    let log = ObjectKey::parse("ci/logs/run-1.log").unwrap();
    let mut data = vec![7u8; githearth_git::object_store::CHUNK_BYTES];
    data.extend_from_slice(b"tail-after-the-boundary");
    let boundary = githearth_git::object_store::CHUNK_BYTES as u64;
    let range = ByteRange::new(boundary - 4, boundary + 4).unwrap();
    let expected_range = data[(boundary - 4) as usize..=(boundary + 4) as usize].to_vec();
    let object_root = installation.storage().join("objects");

    {
        let store = FsObjectStore::open(&object_root).unwrap();
        store.put(tenant, &artifact, &data).unwrap();
        store.create_appendable(tenant, &log).unwrap();
        store.append(tenant, &log, b"first line\n").unwrap();
        store.append(tenant, &log, b"second line\n").unwrap();
        assert_eq!(
            store.get(tenant, &artifact, Some(range)).unwrap(),
            expected_range
        );
    }

    // Restart: a fresh handle over the same files, then append again.
    {
        let store = FsObjectStore::open(&object_root).unwrap();
        assert_eq!(store.get(tenant, &artifact, None).unwrap(), data);
        assert_eq!(
            store.get(tenant, &artifact, Some(range)).unwrap(),
            expected_range
        );
        assert_eq!(
            store.get(tenant, &log, None).unwrap(),
            b"first line\nsecond line\n"
        );
        store.append(tenant, &log, b"third line\n").unwrap();
    }

    // A crash between the payload and sidecar writes leaves an orphan. It must
    // not fail the backup; the tar still carries its bytes (M3).
    let orphan = ObjectKey::parse("ci/artifacts/orphan.bin").unwrap();
    {
        let store = FsObjectStore::open(&object_root).unwrap();
        store
            .put(tenant, &orphan, b"uncommitted orphan bytes")
            .unwrap();
    }
    let orphan_meta = object_root
        .join("meta")
        .join(tenant.as_uuid().simple().to_string())
        .join("ci/artifacts/orphan.bin.json");
    fs::remove_file(&orphan_meta).unwrap();

    let key = key_file(&installation.path, "objects.key", "objects-passphrase");
    let archive = installation.path.join("objects.ghbackup");
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));
    // The manifest carries every object's key, checksum and size; the archive
    // is not only a blind tar of the storage root.
    let (manifest_root, _) = open_archive(&archive, "objects-passphrase");
    let manifest = githearth_server::backup::read_manifest(&manifest_root).unwrap();
    assert_eq!(manifest.objects.len(), 2);
    assert!(
        manifest
            .objects
            .iter()
            .any(|record| record.key == artifact.as_str())
    );
    assert!(
        manifest
            .objects
            .iter()
            .any(|record| record.key == log.as_str())
    );

    let mut destination = Installation::reserve("objects-dest");
    let restored_storage = destination.path.join("restored-storage");
    let restored_config = destination.path.join("restored-config.toml");
    destination.database.created = true;
    let output = run(restore_command(
        &restored_config,
        &archive,
        &destination.database.url,
        &restored_storage,
        &destination.path.join("restored-ssh-host-key"),
        Some(&key),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));

    let restored = FsObjectStore::open(restored_storage.join("objects")).unwrap();
    assert_eq!(
        restored.get(tenant, &artifact, None).unwrap(),
        data,
        "the object must survive backup and restore"
    );
    assert_eq!(
        restored.get(tenant, &artifact, Some(range)).unwrap(),
        expected_range
    );
    assert_eq!(
        restored.get(tenant, &log, None).unwrap(),
        b"first line\nsecond line\nthird line\n",
        "an appended log must survive backup and restore"
    );
    // The orphan's bytes travelled in the archive but it is not part of the
    // manifest or the inventory; verification reports it without failing.
    assert_eq!(restored.inventory().unwrap().len(), 2);
    let problems = restored.verify_all().unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(
        problems[0].kind,
        githearth_git::object_store::ObjectProblemKind::Orphan
    );

    // Damage the restored payload; a read must refuse it, not return bytes.
    let path = restored.data_path(tenant, &artifact);
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    let error = restored.get(tenant, &artifact, None).unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::ChecksumMismatch { .. }),
        "{error}"
    );
}

#[test]
fn a_committed_object_with_a_missing_payload_refuses_backup() {
    let installation = Installation::new("objects-missing-payload");
    installation.setup();
    githearth_git::layout::Storage::open(installation.storage()).unwrap();
    let tenant = TenantId::new(Uuid::new_v4());
    let artifact = ObjectKey::parse("ci/artifacts/release.bin").unwrap();
    let log = ObjectKey::parse("ci/logs/run.log").unwrap();
    let object_root = installation.storage().join("objects");
    {
        let store = FsObjectStore::open(&object_root).unwrap();
        store.put(tenant, &artifact, b"artifact bytes").unwrap();
        store.put(tenant, &log, b"log bytes").unwrap();
    }

    // A sidecar whose payload is gone is damage to a committed object, not the
    // unacknowledged payload-without-sidecar crash window. Removing the
    // payload must fail the backup instead of publishing a short object set.
    let payload = object_root
        .join("data")
        .join(tenant.as_uuid().simple().to_string())
        .join("ci/artifacts/release.bin");
    fs::remove_file(&payload).unwrap();

    let key = key_file(&installation.path, "objects.key", "objects-passphrase");
    let archive = installation.path.join("missing-payload.ghbackup");
    let output = installation.backup(&archive, &key);
    assert!(
        !output.status.success(),
        "a short object set must not be published: {}",
        both(&output)
    );
    let text = both(&output);
    assert!(text.contains("fails verification"), "{text}");
    assert!(text.contains("payload is missing"), "{text}");
    assert!(
        !archive.exists(),
        "a refused backup must not publish an archive"
    );
}

#[test]
fn a_committed_object_with_an_unreadable_sidecar_refuses_backup() {
    let installation = Installation::new("objects-corrupt-sidecar");
    installation.setup();
    githearth_git::layout::Storage::open(installation.storage()).unwrap();
    let tenant = TenantId::new(Uuid::new_v4());
    let artifact = ObjectKey::parse("ci/artifacts/release.bin").unwrap();
    let log = ObjectKey::parse("ci/logs/run.log").unwrap();
    let object_root = installation.storage().join("objects");
    {
        let store = FsObjectStore::open(&object_root).unwrap();
        store.put(tenant, &artifact, b"artifact bytes").unwrap();
        store.put(tenant, &log, b"log bytes").unwrap();
    }

    // A sidecar that cannot be read is not a missing sidecar: the payload's
    // commitment exists but is damaged, so the object must not vanish from the
    // manifest silently.
    let sidecar = object_root
        .join("meta")
        .join(tenant.as_uuid().simple().to_string())
        .join("ci/artifacts/release.bin.json");
    fs::write(&sidecar, b"not json").unwrap();

    let key = key_file(&installation.path, "objects.key", "objects-passphrase");
    let archive = installation.path.join("corrupt-sidecar.ghbackup");
    let output = installation.backup(&archive, &key);
    assert!(
        !output.status.success(),
        "an unreadable committed sidecar must not be published: {}",
        both(&output)
    );
    let text = both(&output);
    assert!(text.contains("fails verification"), "{text}");
    assert!(text.contains("sidecar is unreadable"), "{text}");
    assert!(
        !archive.exists(),
        "a refused backup must not publish an archive"
    );
}

#[test]
fn wrong_missing_corrupt_and_incomplete_archives_are_refused_distinctly_and_touch_nothing() {
    let mut installation = Installation::new("failures");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "baseline", &installation.url());
    let key = key_file(&installation.path, "good.key", "good-passphrase");
    let archive = installation.path.join("good.ghbackup");
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));
    installation.stop();

    let wrong_key = key_file(&installation.path, "wrong.key", "wrong-passphrase");
    let corrupt = installation.path.join("corrupt.ghbackup");
    let mut bytes = fs::read(&archive).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x80;
    fs::write(&corrupt, &bytes).unwrap();
    let truncated = installation.path.join("truncated.ghbackup");
    fs::write(
        &truncated,
        &fs::read(&archive).unwrap()[..fs::metadata(&archive).unwrap().len() as usize / 2],
    )
    .unwrap();

    let mut messages = Vec::new();
    for (label, archive, key, extra) in [
        ("missing", archive.clone(), None, "no backup key supplied"),
        (
            "wrong",
            archive.clone(),
            Some(wrong_key.clone()),
            "wrong backup key",
        ),
        (
            "corrupt",
            corrupt.clone(),
            Some(key.clone()),
            "backup archive is corrupt",
        ),
        (
            "incomplete",
            truncated.clone(),
            Some(key.clone()),
            "incomplete",
        ),
    ] {
        let mut destination = Installation::reserve(&format!("fail-{label}"));
        // The restore may create this database before a late failure; drop it.
        destination.database.created = true;
        let sentinel = destination.path.join("sentinel");
        fs::write(&sentinel, "untouched").unwrap();
        let storage = destination.path.join("would-be-storage");
        let config = destination.path.join("would-be-config.toml");
        let output = run(restore_command(
            &config,
            &archive,
            &destination.database.url,
            &storage,
            &destination.path.join("would-be-host-key"),
            key.as_deref(),
        ));
        assert!(
            !output.status.success(),
            "{label}: restore unexpectedly succeeded"
        );
        let text = both(&output);
        assert!(text.contains(extra), "{label} message was: {text}");
        messages.push(text);
        assert!(
            !destination.database.exists(),
            "{label}: destination database was created"
        );
        assert!(
            !storage.exists(),
            "{label}: destination storage was created"
        );
        assert!(!config.exists(), "{label}: destination config was created");
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "untouched");
    }
    for (index, message) in messages.iter().enumerate() {
        for (other, other_message) in messages.iter().enumerate() {
            if index != other {
                assert_ne!(message, other_message, "the four failures are not distinct");
            }
        }
    }
}

#[test]
fn manifest_checksums_catch_a_flipped_byte_in_every_part_and_never_hold_the_key() {
    let mut installation = Installation::new("manifest");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "manifest", &installation.url());
    let secret = "manifest-test-passphrase";
    let key = key_file(&installation.path, "manifest.key", secret);
    let archive = installation.path.join("manifest.ghbackup");
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));
    installation.stop();

    // The bytes on disk must not reveal the payload or the key.
    let raw = fs::read(&archive).unwrap();
    assert!(
        !raw.windows(secret.len())
            .any(|window| window == secret.as_bytes()),
        "the archive contains the key in plaintext"
    );
    assert!(
        !raw.windows(PASSWORD.len())
            .any(|window| window == PASSWORD.as_bytes()),
        "the archive contains the database password in plaintext"
    );
    assert!(
        !raw.windows(b"manifest.json".len())
            .any(|window| window == b"manifest.json"),
        "the archive leaks its internal layout"
    );

    let (root, header) = open_archive(&archive, secret);
    let manifest = githearth_server::backup::read_manifest(&root).unwrap();
    let text = fs::read_to_string(root.join("manifest.json")).unwrap();
    assert!(!text.contains(secret), "the manifest contains the key");
    assert_eq!(manifest.encryption.key_id, header);
    assert_eq!(manifest.encryption.algorithm, "AES-256-GCM");
    assert_eq!(manifest.encryption.wrap, "argon2id-symmetric");
    assert!(!manifest.encryption.key_id.is_empty());
    assert!(
        manifest.ssh_host_key.is_some(),
        "the host key part is missing"
    );

    let parts = manifest.parts.clone();
    for part in parts {
        let path = root.join(&part.name);
        let original = fs::read(&path).unwrap();
        let mut flipped = original.clone();
        let index = flipped.len() / 2;
        flipped[index] ^= 0x01;
        fs::write(&path, &flipped).unwrap();
        let error = githearth_server::backup::verify_parts(&root, &manifest)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&part.name),
            "a flip in {} was not named: {error}",
            part.name
        );
        fs::write(&path, &original).unwrap();
        assert!(
            githearth_server::backup::verify_parts(&root, &manifest).is_ok(),
            "the restored part should verify again"
        );
    }

    // Every configured part is covered: database, storage, configuration and
    // (because SSH is enabled) the host key.
    let names: Vec<&str> = manifest
        .parts
        .iter()
        .map(|part| part.name.as_str())
        .collect();
    for expected in [
        "database.dump",
        "storage.tar",
        "config.toml",
        "ssh_host_key",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} is not in the manifest: {names:?}"
        );
    }
}

#[test]
fn a_push_that_returned_stays_draining_until_its_journal_row_exists() {
    let mut installation = Installation::new("permit");
    installation.setup();
    // Keep background writers out of the count so `in_flight` is the push.
    installation.append_config(
        "merge_worker_interval_seconds = 3600\nreconcile_interval_seconds = 3600\n\
         housekeeping_enabled = false",
    );
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "baseline", &installation.url());
    wait_push_count(&installation, 1);
    wait_no_in_flight(&installation);
    let baseline = installation.push_count();
    assert_eq!(baseline, 1, "the baseline push must have a journal row");

    // Restart with a deterministic hold in the post-push task, still inside the
    // permit that was moved out of the response.
    installation.stop();
    let marker = installation.path.join("replay.marker");
    installation.start_with_env(&[(
        "GITHEARTH_TEST_HOLD_HTTP_PUSH_AFTER_EXIT",
        marker.to_str().unwrap(),
    )]);

    fs::write(work.join("racing.txt"), "racing").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "racing"], &work);
    let url = installation.url();
    let push_work = work.clone();
    let (returned_tx, returned_rx) = std::sync::mpsc::channel();
    let push = thread::spawn(move || {
        let output = git(&["push", &url, "main"], &push_work);
        let _ = returned_tx.send(());
        output
    });

    // The seam is reached after receive-pack exited and the response finished,
    // by the task that owns the second permit.
    wait_marker(&marker);
    returned_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("the client's push did not return while the post-push task was held");
    let pushed = push.join().unwrap();
    assert!(
        pushed.status.success(),
        "the push must be acknowledged: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    // After the client returned and before the replay ran: the permit must
    // still count it, and no `repository_pushes` row may exist yet.
    assert_eq!(
        installation.push_count(),
        baseline,
        "the journal row exists before the replay was released"
    );
    let active = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(active.phase, githearth_server::maintenance::Phase::Active);
    assert!(
        active.in_flight >= 1,
        "the post-push permit was dropped before the replay: {active:?}"
    );

    let started = installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    assert_eq!(
        started.phase,
        githearth_server::maintenance::Phase::Draining,
        "a push acknowledged before the fence must drain: {started:?}"
    );
    assert!(started.in_flight >= 1, "{started:?}");
    thread::sleep(Duration::from_millis(300));
    let still = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(
        still.phase,
        githearth_server::maintenance::Phase::Draining,
        "drained before the journal replay"
    );
    assert_eq!(installation.push_count(), baseline);

    release(&marker);
    let drained = wait_drained(&installation);
    assert_eq!(drained.in_flight, 0);
    assert_eq!(
        installation.push_count(),
        baseline + 1,
        "drained before the journal row was written"
    );
}

#[test]
fn an_ssh_push_is_refused_while_fenced() {
    let mut installation = Installation::new("sshfence");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let token = installation.login();
    let key = ssh_client_key(&installation, &token);
    let work = installation.path.join("work");
    prepare_work(&work);
    fs::write(work.join("first.txt"), "first").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "first"], &work);
    let url = installation.ssh_url();
    let first = ssh_git(&key, installation.ssh_port, &["push", &url, "main"], &work);
    assert!(
        first.status.success(),
        "baseline SSH push: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    wait_no_in_flight(&installation);

    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);

    fs::write(work.join("after-fence.txt"), "blocked").unwrap();
    git_ok(&["add", "."], &work);
    git_ok(&["commit", "-m", "blocked"], &work);
    let refused = ssh_git(&key, installation.ssh_port, &["push", &url, "main"], &work);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "an SSH push after the fence must be refused, not acknowledged"
    );
    assert!(
        stderr.contains("maintenance mode") || stderr.contains("writes are refused"),
        "the refusal must reach the client: {stderr}"
    );
    installation.maintenance("POST", githearth_server::maintenance::STOP_PATH);
}

#[test]
fn a_housekeeping_run_is_counted_and_keeps_the_fence_draining() {
    let mut installation = Installation::new("bgfence");
    installation.setup();
    let marker = installation.path.join("housekeeping.marker");
    installation.start_with_env(&[("GITHEARTH_TEST_HOLD_HOUSEKEEPING", marker.to_str().unwrap())]);
    let repo = installation.create_org_and_repo();
    let repository: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "baseline", &installation.url());
    wait_no_in_flight(&installation);

    installation.enqueue_housekeeping(repository);
    wait_marker(&marker);

    // The seam is inside `run_job`, after it took the write permit.
    let active = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(active.phase, githearth_server::maintenance::Phase::Active);
    assert!(
        active.in_flight >= 1,
        "a running housekeeping job is not counted in flight: {active:?}"
    );

    let started = installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    assert_eq!(
        started.phase,
        githearth_server::maintenance::Phase::Draining,
        "a background storage writer must keep the fence draining: {started:?}"
    );
    assert!(started.in_flight >= 1, "{started:?}");
    thread::sleep(Duration::from_millis(300));
    let still = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(
        still.phase,
        githearth_server::maintenance::Phase::Draining,
        "the fence reported drained while housekeeping was still running"
    );

    release(&marker);
    let drained = wait_drained(&installation);
    assert_eq!(drained.in_flight, 0);
}

#[test]
fn a_fence_lifted_between_the_dump_and_the_snapshot_aborts_the_backup() {
    let mut installation = Installation::new("latefence");
    installation.setup();
    // The fence path still proves the fence again before its storage tar.
    installation.append_config("backup_snapshot_mode = \"fence\"");
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit_push(&work, "baseline", &installation.url());
    wait_no_in_flight(&installation);
    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);

    let key = key_file(&installation.path, "late.key", "late-passphrase");
    let archive = installation.path.join("late.ghbackup");
    let marker = installation.path.join("dump.marker");
    let mut backup = installation.backup_held_at_dump(&archive, &key, &marker);
    backup
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = backup.spawn().unwrap();
    wait_marker(&marker);

    // Lift the fence after the dump's first proof and before the second.
    installation.maintenance("POST", githearth_server::maintenance::STOP_PATH);
    release(&marker);
    let output = child.wait_with_output().unwrap();
    let text = both(&output);
    assert!(
        !output.status.success(),
        "backup published an archive after the fence was lifted: {text}"
    );
    assert!(text.contains("accepting writes"), "{text}");
    assert!(
        !archive.exists(),
        "a backup that lost its fence left an archive behind"
    );
}

fn repo_path(storage: &Path, id: Uuid) -> PathBuf {
    let simple = id.simple().to_string();
    storage
        .join("repositories")
        .join(&simple[..2])
        .join(format!("{simple}.git"))
}

fn cat_file(repository: &Path, commit: &str) -> bool {
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .args([
            "--git-dir",
            repository.to_str().unwrap(),
            "cat-file",
            "-e",
            commit,
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
