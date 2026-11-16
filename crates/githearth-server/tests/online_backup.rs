//! Single-node online backup and the operator's key custody (M4.2 7C, U29).
//!
//! These cases run the real binary: `setup`, `serve`, `backup` and `restore`.
//! The online path is selected by probing the storage filesystem for
//! copy-on-write cloning. A write that starts before the recorded position is
//! either in the restore or was refused before acknowledgement; a write
//! acknowledged after the recorded position is outside that guarantee and is
//! absent by point-in-time design, which its case states. The fallback case
//! forces `backup_snapshot_mode = "fence"` to prove the maintenance-window path
//! still works when the filesystem offers no snapshot.
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
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

const PASSWORD: &str = "correct horse battery staple for online backup";

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

static DB: Mutex<()> = Mutex::new(());

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
        let name = format!("githearth_ob_{}_{}", label, Uuid::new_v4().simple());
        let url = with_database(&admin_url, &name);
        Self {
            admin_url,
            name,
            url,
            created: false,
        }
    }

    fn create(&mut self) {
        let _one = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
}

impl Installation {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ghob-{}-{}-{label}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            path,
            database: Database::new(label),
            http_port: free_port(),
            ssh_port: free_port(),
            child: None,
        }
    }

    fn reserve(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ghob-{}-{}-{label}-restore",
            std::process::id(),
            nonce()
        ));
        fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
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

    fn installation_key(&self) -> PathBuf {
        self.path.join("githearth/installation.key")
    }

    fn setup(&self) {
        let _one = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let log = fs::File::create(self.path.join("serve.log")).unwrap();
        let err = log.try_clone().unwrap();
        let mut command = server();
        command
            .args(["--config", self.config().to_str().unwrap(), "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err));
        let mut child = command.spawn().unwrap();
        assert!(
            wait_ready(&mut child, self.http_port),
            "serve never became ready: {}",
            fs::read_to_string(self.path.join("serve.log")).unwrap_or_default()
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

    fn maintenance_token(&self) -> String {
        fs::read_to_string(self.storage().join("run/maintenance.token"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn maintenance(&self, method: &str, path: &str) -> githearth_server::maintenance::Status {
        let token = self.maintenance_token();
        let (status, body) = http(self.http_port, method, path, Some(&token), None);
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

    /// A `backup` that holds at one of the backup's test seams
    /// (`BACKUP_ONLINE_GATE`, `BACKUP_AFTER_SNAPSHOT`, `BACKUP_BEFORE_DUMP`).
    fn backup_held_at(&self, output: &Path, key: &Path, seam: &str, marker: &Path) -> Command {
        let mut command = server();
        command.env(format!("GITHEARTH_TEST_HOLD_{seam}"), marker);
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

    fn backup_held_during_gate(&self, output: &Path, key: &Path, marker: &Path) -> Command {
        self.backup_held_at(output, key, "BACKUP_ONLINE_GATE", marker)
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
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("serve exited before ready: {status}");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_refused(http_port: u16, ssh_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", http_port)).is_err()
            && TcpStream::connect(("127.0.0.1", ssh_port)).is_err()
        {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("listeners still accept connections");
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    } else {
        request.push_str("Content-Length: 0\r\n");
    }
    request.push_str("\r\n");
    if let Some(body) = body {
        request.push_str(body);
    }
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response).unwrap();
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (code, body.to_owned())
}

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

fn prepare_work(work: &Path) {
    fs::create_dir_all(work).unwrap();
    git_ok(&["init", "-b", "main", "."], work);
    git_ok(&["config", "user.email", "test@example.com"], work);
    git_ok(&["config", "user.name", "Test"], work);
}

fn commit(work: &Path, message: &str) -> String {
    fs::write(
        work.join(format!("{}.txt", message.replace(' ', "-"))),
        message,
    )
    .unwrap();
    git_ok(&["add", "."], work);
    git_ok(&["commit", "-m", message], work);
    String::from_utf8(git(&["rev-parse", "HEAD"], work).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn push(work: &Path, url: &str) -> Output {
    git(&["push", url, "main"], work)
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "marker never appeared");
        thread::sleep(Duration::from_millis(50));
    }
}

fn release(marker: &Path) {
    fs::write(marker.with_extension("release"), b"release").unwrap();
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

fn key_file(directory: &Path, name: &str, secret: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, secret).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn open_archive(archive: &Path, secret: &str) -> OpenedArchive {
    let work = std::env::temp_dir().join(format!("ghob-open-{}-{}", std::process::id(), nonce()));
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
    OpenedArchive {
        key_id: header.key_id.clone(),
        header,
        root,
        work,
    }
}

/// A decrypted archive extracted under a temporary directory that is removed
/// when the test case ends.
struct OpenedArchive {
    key_id: String,
    header: githearth_server::backup::ArchiveHeader,
    root: PathBuf,
    work: PathBuf,
}

impl OpenedArchive {
    fn manifest(&self) -> githearth_server::backup::Manifest {
        githearth_server::backup::read_manifest(&self.root).unwrap()
    }
}

impl Drop for OpenedArchive {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.work);
    }
}

fn restore_command(
    config: &Path,
    archive: &Path,
    database_url: &str,
    storage: &Path,
    host_key: &Path,
    key: Option<&Path>,
    installation_key: Option<&Path>,
) -> Command {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", database_url);
    if let Some(path) = installation_key {
        command.env("GITHEARTH_INSTALLATION_KEY_FILE", path);
    }
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

/// Runs one `git` command against a bare repository. `core.hooksPath` is
/// cleared so a test can move a ref the way a pre-0005 or out-of-band writer
/// would; the installed hooks deliberately refuse any other path.
fn git_bare(bare: &Path, args: &[&str]) -> Output {
    let mut all = vec![
        "--git-dir",
        bare.to_str().unwrap(),
        "-c",
        "core.hooksPath=/nonexistent",
    ];
    all.extend_from_slice(args);
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .args(&all)
        .output()
        .unwrap()
}

/// Creates a client-visible ref in the live bare repository with no journal
/// record and no metadata push: the shape of a pre-0005, imported or forked
/// ref. It is not moved by any journaled action, so only the recorded ref
/// digest can prove it survived the snapshot.
fn create_unjournaled_ref(bare: &Path, name: &str) {
    let head = String::from_utf8(git_bare(bare, &["rev-parse", "refs/heads/main"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let output = git_bare(bare, &["update-ref", name, &head]);
    assert!(
        output.status.success(),
        "cannot create {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn delete_ref(bare: &Path, name: &str) {
    let output = git_bare(bare, &["update-ref", "-d", name]);
    assert!(
        output.status.success(),
        "cannot delete {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Whether this filesystem supports the copy-on-write clone the online path
/// needs. The project's Linux CI runner is ext4, which does not, so the online
/// cases report a skip there instead of failing; the fence fallback and the
/// gate's own cases still run on every filesystem.
fn copy_on_write(installation: &Installation, case: &str) -> bool {
    let supported =
        githearth_server::backup::copy_on_write_supported(&installation.storage()).unwrap_or(false);
    if !supported {
        eprintln!(
            "skipping {case}: {} does not support copy-on-write cloning, so `auto` selects the \
             maintenance fence",
            installation.storage().display()
        );
    }
    supported
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn online_backup_with_a_writer_running_restores_consistently() {
    let mut installation = Installation::new("online");
    installation.setup();
    if !copy_on_write(
        &installation,
        "online_backup_with_a_writer_running_restores_consistently",
    ) {
        return;
    }
    let marker = installation.path.join("hook.marker");
    let hook = installation.path.join("slow-hook.sh");
    slow_hook_script(&hook, &marker);
    installation.append_config(&format!("hook_program = {:?}", hook.to_str().unwrap()));
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    let baseline = commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    let _ = fs::remove_file(&marker);

    // A writer is already running when `backup` starts. The online path gates
    // it just long enough to drain and record the position; it is not an
    // operator maintenance window.
    let racing = commit(&work, "racing");
    let url = installation.url();
    let push_work = work.clone();
    let push_thread = thread::spawn(move || push(&push_work, &url));
    wait_marker(&marker);

    let key = key_file(&installation.path, "online.key", "online-passphrase");
    let archive = installation.path.join("online.ghbackup");
    let output = installation.backup(&archive, &key);
    let push = push_thread.join().unwrap();
    assert!(
        push.status.success(),
        "the writer that started before the gate is drained, not killed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert!(output.status.success(), "online backup: {}", both(&output));
    assert!(
        stdout(&output).contains("snapshot: copy-on-write"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("fence used: online-snapshot"),
        "{}",
        stdout(&output)
    );

    let opened = open_archive(&archive, "online-passphrase");
    let manifest = opened.manifest();
    assert_eq!(manifest.fence.kind, "online-snapshot");
    let snapshot = manifest.snapshot.as_ref().expect("snapshot record");
    assert_eq!(snapshot.mode, "copy-on-write");
    assert!(snapshot.refs_verified);
    assert!(manifest.installation.is_some());

    // The server was never left in maintenance mode.
    let status = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(status.phase, githearth_server::maintenance::Phase::Active);

    installation.stop();
    let mut destination = Installation::reserve("online-dest");
    destination.database.created = true;
    let restored_storage = destination.path.join("restored-storage");
    let restored_config = destination.path.join("restored-config.toml");
    let key_for_restore = key_file(&destination.path, "online.key", "online-passphrase");
    let output = run(restore_command(
        &restored_config,
        &archive,
        &destination.database.url,
        &restored_storage,
        &destination.path.join("restored-ssh-host-key"),
        Some(&key_for_restore),
        Some(&installation.installation_key()),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));
    let bare = repo_path(&restored_storage, repo_id);
    assert!(cat_file(&bare, &baseline), "the baseline commit is missing");
    assert!(
        cat_file(&bare, &racing),
        "the commit of the writer that was running during the backup is missing"
    );
}

#[test]
fn a_write_that_starts_before_the_recorded_position_is_restored_or_refused() {
    let mut installation = Installation::new("race-online");
    installation.setup();
    if !copy_on_write(
        &installation,
        "a_write_that_starts_before_the_recorded_position_is_restored_or_refused",
    ) {
        return;
    }
    let marker = installation.path.join("hook.marker");
    let hook = installation.path.join("slow-hook.sh");
    slow_hook_script(&hook, &marker);
    installation.append_config(&format!("hook_program = {:?}", hook.to_str().unwrap()));
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    let _ = fs::remove_file(&marker);

    // A push starts before the gate, so it must be included.
    let included = commit(&work, "included");
    let url = installation.url();
    let push_work = work.clone();
    let push_thread = thread::spawn(move || push(&push_work, &url));
    wait_marker(&marker);

    let key = key_file(
        &installation.path,
        "race-online.key",
        "race-online-passphrase",
    );
    let archive = installation.path.join("race-online.ghbackup");
    let gate_marker = installation.path.join("gate.marker");
    let mut command = installation.backup_held_during_gate(&archive, &key, &gate_marker);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    // The gate drains the push that started first, records the position, then
    // holds before the snapshot.
    wait_marker(&gate_marker);

    // A push that starts while the gate is held is refused before it can be
    // acknowledged.
    let refused = commit(&work, "refused");
    let refused_output = push(&work, &installation.url());
    assert!(
        !refused_output.status.success(),
        "a push during the online gate must be refused"
    );
    let refused_text = String::from_utf8_lossy(&refused_output.stderr);
    assert!(
        refused_text.contains("503") || refused_text.contains("maintenance"),
        "{refused_text}"
    );

    release(&gate_marker);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "online backup: {}", both(&output));
    let push = push_thread.join().unwrap();
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );

    installation.stop();
    let mut destination = Installation::reserve("race-online-dest");
    destination.database.created = true;
    let restored_storage = destination.path.join("restored-storage");
    let key_for_restore = key_file(
        &destination.path,
        "race-online.key",
        "race-online-passphrase",
    );
    let output = run(restore_command(
        &destination.path.join("restored-config.toml"),
        &archive,
        &destination.database.url,
        &restored_storage,
        &destination.path.join("restored-ssh-host-key"),
        Some(&key_for_restore),
        Some(&installation.installation_key()),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));
    let bare = repo_path(&restored_storage, repo_id);
    assert!(
        cat_file(&bare, &included),
        "the writer that raced the snapshot is not in the restore"
    );
    assert!(
        !cat_file(&bare, &refused),
        "a refused writer appeared in the restore"
    );
}

#[test]
fn key_failure_modes_are_distinct_and_never_start_with_unreadable_secrets() {
    let mut installation = Installation::new("keys");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());

    let secret = "key-mode-passphrase";
    let key = key_file(&installation.path, "good.key", secret);
    let archive = installation.path.join("keys.ghbackup");
    // Stopping the server first keeps this case about the key failure modes:
    // the archive is taken under the stopped-server fence, so it does not need
    // a copy-on-write filesystem.
    installation.stop();
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));

    let unreadable_archive_key = key_file(&installation.path, "unreadable.key", secret);
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&unreadable_archive_key, fs::Permissions::from_mode(0o000)).unwrap();

    let wrong_installation = installation.path.join("wrong-installation.key");
    githearth_server::secrets::generate_raw_key_file(&wrong_installation).unwrap();
    let unreadable_installation = installation.path.join("unreadable-installation.key");
    fs::copy(installation.installation_key(), &unreadable_installation).unwrap();
    fs::set_permissions(&unreadable_installation, fs::Permissions::from_mode(0o644)).unwrap();
    let missing_installation = installation.path.join("missing-installation.key");

    let cases = [
        ("missing-archive-key", None, None, "no backup key supplied"),
        (
            "wrong-archive-key",
            Some(key_file(
                &installation.path,
                "wrong.key",
                "wrong-passphrase",
            )),
            None,
            "wrong backup key",
        ),
        (
            "unreadable-archive-key",
            Some(unreadable_archive_key.clone()),
            None,
            "cannot read key file",
        ),
        (
            "missing-installation-key",
            Some(key.clone()),
            Some(missing_installation.clone()),
            "installation key file",
        ),
        (
            "unreadable-installation-key",
            Some(key.clone()),
            Some(unreadable_installation.clone()),
            "group or others could read",
        ),
        (
            "wrong-installation-key",
            Some(key.clone()),
            Some(wrong_installation.clone()),
            "wrong installation key",
        ),
    ];

    let mut messages = Vec::new();
    for (label, archive_key, installation_key, expected) in cases {
        let mut destination = Installation::reserve(&format!("key-{label}"));
        destination.database.created = true;
        let storage = destination.path.join("would-be-storage");
        let config = destination.path.join("would-be-config.toml");
        let host_key = destination.path.join("would-be-host-key");
        let output = run(restore_command(
            &config,
            &archive,
            &destination.database.url,
            &storage,
            &host_key,
            archive_key.as_deref(),
            installation_key.as_deref(),
        ));
        assert!(
            !output.status.success(),
            "{label}: restore unexpectedly succeeded"
        );
        let text = both(&output);
        assert!(text.contains(expected), "{label}: {text}");
        // Compare the refusal itself, not the whole output: the paths in it
        // differ per case, which would make the distinctness check vacuous.
        messages.push(
            stderr(&output)
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .to_owned(),
        );
        assert!(
            !destination.database.exists(),
            "{label}: a failed restore created the destination database"
        );
        assert!(!storage.exists(), "{label}: storage was published");
        assert!(!config.exists(), "{label}: configuration was published");
        assert!(!host_key.exists(), "{label}: host key was published");
    }
    for (index, message) in messages.iter().enumerate() {
        for (other, other_message) in messages.iter().enumerate() {
            if index != other {
                assert_ne!(message, other_message, "key failures are not distinct");
            }
        }
    }
}

#[test]
fn a_snapshotless_filesystem_falls_back_to_the_maintenance_fence() {
    let mut installation = Installation::new("fence-fallback");
    installation.setup();
    // This is the fallback an operator gets when the filesystem offers no
    // copy-on-write clone; the probe is skipped so the case is deterministic.
    installation.append_config("backup_snapshot_mode = \"fence\"");
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());

    let key = key_file(&installation.path, "fallback.key", "fallback-passphrase");
    let refused = installation.path.join("refused.ghbackup");
    let output = installation.backup(&refused, &key);
    assert!(
        !output.status.success(),
        "the fence path refuses while serve accepts writes"
    );
    let text = both(&output);
    assert!(text.contains("accepting writes"), "{text}");
    assert!(!refused.exists());

    installation.maintenance("POST", githearth_server::maintenance::START_PATH);
    wait_drained(&installation);
    let archive = installation.path.join("fallback.ghbackup");
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));
    assert!(
        stdout(&output).contains("fence used: maintenance"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("snapshot: maintenance fence"),
        "{}",
        stdout(&output)
    );

    let opened = open_archive(&archive, "fallback-passphrase");
    let manifest = opened.manifest();
    assert_eq!(manifest.fence.kind, "maintenance");
    assert_eq!(
        manifest
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.mode.as_str()),
        Some("maintenance-fence")
    );
    assert!(
        manifest
            .repositories
            .iter()
            .all(|position| position.refs_digest.is_some()),
        "the fence path records the live ref digest too"
    );
}

#[test]
fn the_archive_carries_the_installation_key_id_and_no_key() {
    let mut installation = Installation::new("nokey");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());

    let secret = "no-key-in-archive-passphrase";
    let key = key_file(&installation.path, "nokey.key", secret);
    let archive = installation.path.join("nokey.ghbackup");
    // The stopped-server fence keeps this case about the manifest, not about
    // the snapshot machinery.
    installation.stop();
    let output = installation.backup(&archive, &key);
    assert!(output.status.success(), "{}", both(&output));

    // The descriptor is TOML with `key = "<base64 of the 32-byte key>"`. The
    // meaningful search is for that key, not for the file: a manifest field
    // carrying the raw key's base64 would pass a search for the file's base64.
    let descriptor = fs::read_to_string(installation.installation_key()).unwrap();
    let encoded = descriptor
        .lines()
        .find_map(|line| line.strip_prefix("key = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("the installation key descriptor has a key line");
    let installation_key = BASE64.decode(encoded).unwrap();
    assert_eq!(installation_key.len(), 32, "the descriptor holds one key");
    let installation_key_b64 = BASE64.encode(&installation_key);
    let installation_key_hex = hex::encode(&installation_key);

    let raw = fs::read(&archive).unwrap();
    assert!(
        !raw.windows(installation_key.len())
            .any(|window| window == installation_key.as_slice()),
        "the raw archive contains the installation key bytes"
    );
    assert!(
        !raw.windows(installation_key_b64.len())
            .any(|window| window == installation_key_b64.as_bytes()),
        "the raw archive contains the base64 installation key"
    );
    assert!(
        !raw.windows(installation_key_hex.len())
            .any(|window| window == installation_key_hex.as_bytes()),
        "the raw archive contains the hex installation key"
    );
    assert!(
        !raw.windows(secret.len())
            .any(|window| window == secret.as_bytes()),
        "the archive contains the backup key"
    );

    let opened = open_archive(&archive, secret);
    let manifest = opened.manifest();
    let text = fs::read_to_string(opened.root.join("manifest.json")).unwrap();
    let record = manifest.installation.expect("installation record");
    let expected = githearth_server::secrets::InstallationKeySet::from_paths(
        &installation.installation_key(),
        None,
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        record.key_id,
        expected.current().id(),
        "the manifest names the installation key that wrapped the secrets"
    );
    assert_eq!(record.algorithm, "AES-256-GCM");
    assert!(!record.wrap_method.is_empty());
    assert_eq!(manifest.encryption.key_id, opened.key_id);

    // The installation record is exactly the three fields, and the whole
    // decrypted manifest carries none of the key in raw, base64 or hex form.
    let value: Value = serde_json::from_str(&text).unwrap();
    let mut keys: Vec<&str> = value["installation"]
        .as_object()
        .expect("installation is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["algorithm", "key_id", "wrap_method"]);
    for needle in [installation_key_b64.as_str(), installation_key_hex.as_str()] {
        assert!(
            !text.contains(needle),
            "the decrypted manifest contains the installation key encoded as {needle}"
        );
    }
    assert!(
        !text
            .as_bytes()
            .windows(installation_key.len())
            .any(|window| window == installation_key.as_slice()),
        "the decrypted manifest contains the raw installation key"
    );
    assert!(!text.contains(secret));
}

/// A fresh destination for a restore: the database is created by `restore`, so
/// the guard must drop it afterwards.
fn restore_destination(label: &str) -> Installation {
    let mut destination = Installation::reserve(label);
    destination.database.created = true;
    destination
}

fn restore_into(
    destination: &Installation,
    archive: &Path,
    key_secret: &str,
    installation_key: &Path,
) -> Output {
    let key = key_file(&destination.path, "restore.key", key_secret);
    run(restore_command(
        &destination.path.join("restored-config.toml"),
        archive,
        &destination.database.url,
        &destination.path.join("restored-storage"),
        &destination.path.join("restored-ssh-host-key"),
        Some(&key),
        Some(installation_key),
    ))
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).unwrap();
    let mut digest = Sha256::new();
    digest.update(&bytes);
    hex::encode(digest.finalize())
}

/// An archive the current `backup` refuses to write: it wraps one data key
/// wrongly, which can only happen to an archive written by an older binary or
/// edited by hand. `restore` must refuse it end to end — after `pg_restore`,
/// while verifying the restored database — and publish no database, storage,
/// configuration or host key.
#[test]
fn a_damaged_wrapper_archive_is_refused_end_to_end_and_published_nowhere() {
    let mut installation = Installation::new("damaged-archive");
    installation.setup();
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    installation.stop();

    let key = key_file(
        &installation.path,
        "damaged-archive.key",
        "damaged-archive-passphrase",
    );
    let good = installation.path.join("good.ghbackup");
    let output = installation.backup(&good, &key);
    assert!(output.status.success(), "backup: {}", both(&output));

    // Rebuild a well-formed container around a dump that carries one damaged
    // wrapper under the current key id. The manifest part checksum is updated,
    // so the refusal below is the wrapper check, not a corrupt archive.
    let opened = open_archive(&good, "damaged-archive-passphrase");
    let mut manifest = opened.manifest();
    let installation_record = manifest
        .installation
        .clone()
        .expect("a format-3 archive records the installation key");
    let data_key_id = Uuid::new_v4();
    runtime().block_on(async {
        let pool = sqlx::PgPool::connect(&installation.database.url)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO secret_data_keys (id, kek_id, algorithm, wrap_method, wrapped_key) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(data_key_id)
        .bind(&installation_record.key_id)
        .bind(&installation_record.algorithm)
        .bind(&installation_record.wrap_method)
        // Correct length (12-byte nonce + 32-byte key + 16-byte tag), so the
        // refusal is authentication, not a shape check.
        .bind(vec![0u8; 60])
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    });
    let dump = opened.root.join("database.dump");
    let status = Command::new("pg_dump")
        .args(["--format=custom", "--no-owner", "--no-acl", "--file"])
        .arg(&dump)
        .arg(&installation.database.url)
        .status()
        .unwrap();
    assert!(status.success(), "pg_dump of the damaged database failed");
    let part = manifest
        .parts
        .iter_mut()
        .find(|part| part.name == "database.dump")
        .expect("the archive carries database.dump");
    part.bytes = fs::metadata(&dump).unwrap().len();
    part.sha256 = sha256_file(&dump);
    fs::write(
        opened.root.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let payload = opened.work.join("rebuilt.tar");
    let status = Command::new("tar")
        .args(["-cf"])
        .arg(&payload)
        .arg("-C")
        .arg(&opened.root)
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success(), "cannot rebuild the archive payload");
    let damaged = installation.path.join("damaged-wrapper.ghbackup");
    let salt: [u8; 16] = BASE64
        .decode(&opened.header.salt)
        .unwrap()
        .try_into()
        .unwrap();
    // Same passphrase and KDF salt, so the archive key id (and therefore
    // `manifest.encryption.key_id`) still matches the header; only the GCM
    // nonce is fresh.
    githearth_server::backup::encrypt_file_with_salt(
        &payload,
        &damaged,
        b"damaged-archive-passphrase",
        &salt,
    )
    .unwrap();

    let destination = restore_destination("damaged-archive-dest");
    let output = restore_into(
        &destination,
        &damaged,
        "damaged-archive-passphrase",
        &installation.installation_key(),
    );
    assert!(
        !output.status.success(),
        "restore must refuse an archive whose wrapper cannot be opened: {}",
        both(&output)
    );
    let text = both(&output);
    assert!(
        text.contains("cannot be opened") || text.contains("do not match the installation key"),
        "the refusal must name the unreadable wrapper: {text}"
    );
    assert!(
        !destination.database.exists(),
        "the partially restored database was not dropped"
    );
    assert!(
        !destination.path.join("restored-storage").exists(),
        "a refused restore published storage"
    );
    assert!(
        !destination.path.join("restored-config.toml").exists(),
        "a refused restore wrote configuration"
    );
    assert!(
        !destination.path.join("restored-ssh-host-key").exists(),
        "a refused restore wrote the host key"
    );
}

#[test]
fn a_snapshot_that_loses_a_journaled_ref_is_refused_and_published_nowhere() {
    let mut installation = Installation::new("short-journaled");
    installation.setup();
    if !copy_on_write(
        &installation,
        "a_snapshot_that_loses_a_journaled_ref_is_refused_and_published_nowhere",
    ) {
        return;
    }
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    let bare = repo_path(&installation.storage(), repo_id);

    let key = key_file(
        &installation.path,
        "short-journaled.key",
        "short-journaled-passphrase",
    );
    let archive = installation.path.join("short-journaled.ghbackup");
    let marker = installation.path.join("gate.marker");
    let mut command = installation.backup_held_during_gate(&archive, &key, &marker);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_marker(&marker);

    // The ref the journal committed at the recorded position disappears from
    // the live repository before the clone.
    delete_ref(&bare, "refs/heads/main");

    release(&marker);
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "a short snapshot must not be published: {}",
        both(&output)
    );
    let text = both(&output);
    assert!(
        text.contains("committed") || text.contains("client-visible refs"),
        "{text}"
    );
    assert!(
        !archive.exists(),
        "the archive was published even though the snapshot lost a journaled ref"
    );
    installation.stop();
}

#[test]
fn a_snapshot_that_loses_a_ref_with_no_journal_record_is_refused() {
    let mut installation = Installation::new("short-unjournaled");
    installation.setup();
    if !copy_on_write(
        &installation,
        "a_snapshot_that_loses_a_ref_with_no_journal_record_is_refused",
    ) {
        return;
    }
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    let bare = repo_path(&installation.storage(), repo_id);
    // A client-visible ref with no journal record and no metadata push: the
    // pre-0005, imported and forked shape.
    create_unjournaled_ref(&bare, "refs/heads/legacy");

    let key = key_file(
        &installation.path,
        "short-unjournaled.key",
        "short-unjournaled-passphrase",
    );
    let archive = installation.path.join("short-unjournaled.ghbackup");
    let marker = installation.path.join("gate.marker");
    let mut command = installation.backup_held_during_gate(&archive, &key, &marker);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_marker(&marker);

    // The unjournaled ref disappears before the clone. The journal replay knows
    // nothing about it; only the recorded ref digest can catch this.
    delete_ref(&bare, "refs/heads/legacy");

    release(&marker);
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "an unjournaled ref lost by the snapshot must refuse the backup: {}",
        both(&output)
    );
    let text = both(&output);
    assert!(text.contains("client-visible refs"), "{text}");
    assert!(
        !archive.exists(),
        "the archive was published with a short snapshot"
    );
    installation.stop();
}

#[test]
fn a_write_between_the_snapshot_and_the_ref_check_is_refused() {
    let mut installation = Installation::new("after-snapshot");
    installation.setup();
    if !copy_on_write(
        &installation,
        "a_write_between_the_snapshot_and_the_ref_check_is_refused",
    ) {
        return;
    }
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    let baseline = commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());

    let key = key_file(
        &installation.path,
        "after-snapshot.key",
        "after-snapshot-passphrase",
    );
    let archive = installation.path.join("after-snapshot.ghbackup");
    let marker = installation.path.join("snapshot.marker");
    let mut command = installation.backup_held_at(&archive, &key, "BACKUP_AFTER_SNAPSHOT", &marker);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_marker(&marker);

    // The clone exists and the gate is still held: a write here is refused
    // before acknowledgement, so it can be neither silently lost nor half in
    // the snapshot.
    let refused = commit(&work, "after-snapshot");
    let refused_output = push(&work, &installation.url());
    assert!(
        !refused_output.status.success(),
        "a write between the snapshot and the ref check must be refused"
    );
    let refused_text = String::from_utf8_lossy(&refused_output.stderr);
    assert!(
        refused_text.contains("503") || refused_text.contains("maintenance"),
        "{refused_text}"
    );

    release(&marker);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "online backup: {}", both(&output));

    installation.stop();
    let destination = restore_destination("after-snapshot-dest");
    let key_for_restore = key_file(
        &destination.path,
        "after-snapshot.key",
        "after-snapshot-passphrase",
    );
    let output = run(restore_command(
        &destination.path.join("restored-config.toml"),
        &archive,
        &destination.database.url,
        &destination.path.join("restored-storage"),
        &destination.path.join("restored-ssh-host-key"),
        Some(&key_for_restore),
        Some(&installation.installation_key()),
    ));
    assert!(output.status.success(), "restore: {}", both(&output));
    let bare = repo_path(&destination.path.join("restored-storage"), repo_id);
    assert!(cat_file(&bare, &baseline), "the baseline commit is missing");
    assert!(
        !cat_file(&bare, &refused),
        "a refused writer appeared in the restore"
    );
}

#[test]
fn a_write_after_the_recorded_position_is_acknowledged_and_absent_by_design() {
    let mut installation = Installation::new("after-gate");
    installation.setup();
    if !copy_on_write(
        &installation,
        "a_write_after_the_recorded_position_is_acknowledged_and_absent_by_design",
    ) {
        return;
    }
    installation.start();
    let repo = installation.create_org_and_repo();
    let repo_id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    let work = installation.path.join("work");
    prepare_work(&work);
    let baseline = commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());

    let key = key_file(
        &installation.path,
        "after-gate.key",
        "after-gate-passphrase",
    );
    let archive = installation.path.join("after-gate.ghbackup");
    let marker = installation.path.join("dump.marker");
    let mut command = installation.backup_held_at(&archive, &key, "BACKUP_BEFORE_DUMP", &marker);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_marker(&marker);

    // The gate is released before the dump runs. This write is acknowledged
    // with no fence in the way, but it starts *after* the recorded position, so
    // it is outside U29's write-racing guarantee (a write that starts before
    // the recorded position is in the restore or was refused). The dump is
    // pinned to the exported snapshot, so the write must be absent from the
    // restore by point-in-time design.
    let after_gate = commit(&work, "after-gate");
    let pushed = push(&work, &installation.url());
    assert!(
        pushed.status.success(),
        "the gate is released before the dump, so this write is accepted: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    release(&marker);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "online backup: {}", both(&output));

    installation.stop();
    let destination = restore_destination("after-gate-dest");
    let output = restore_into(
        &destination,
        &archive,
        "after-gate-passphrase",
        &installation.installation_key(),
    );
    assert!(output.status.success(), "restore: {}", both(&output));
    let bare = repo_path(&destination.path.join("restored-storage"), repo_id);
    assert!(cat_file(&bare, &baseline), "the baseline commit is missing");
    assert!(
        !cat_file(&bare, &after_gate),
        "a write after the recorded position is in the restore; the dump is not at the exported \
         snapshot"
    );
}

#[test]
fn a_gate_that_cannot_drain_is_released_and_the_server_keeps_writing() {
    let mut installation = Installation::new("drain-timeout");
    installation.setup();
    let marker = installation.path.join("hook.marker");
    let hook = installation.path.join("slow-hook.sh");
    slow_hook_script(&hook, &marker);
    installation.append_config(&format!("hook_program = {:?}", hook.to_str().unwrap()));
    installation.append_config("maintenance_drain_timeout_seconds = 1");
    installation.start();
    installation.create_org_and_repo();
    let work = installation.path.join("work");
    prepare_work(&work);
    commit(&work, "baseline");
    assert!(push(&work, &installation.url()).status.success());
    let _ = fs::remove_file(&marker);

    // One write is in flight for longer than the drain timeout, so the gate can
    // never drain.
    let _racing = commit(&work, "racing-timeout");
    let url = installation.url();
    let push_work = work.clone();
    let push_thread = thread::spawn(move || push(&push_work, &url));
    wait_marker(&marker);

    let config = githearth_server::config::Config::load(Some(&installation.config())).unwrap();
    let error = match runtime().block_on(githearth_server::maintenance::begin_online(&config)) {
        Ok(_) => panic!("the gate drained while a write was in flight"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("did not drain within"), "{error}");
    assert!(
        error.contains("was released"),
        "the failure must say the gate was released: {error}"
    );

    // The failed gate must not leave the server refusing writes until an
    // operator runs `maintenance stop`.
    let status = installation.maintenance("GET", githearth_server::maintenance::STATUS_PATH);
    assert_eq!(
        status.phase,
        githearth_server::maintenance::Phase::Active,
        "a failed gate left the server fenced: {status:?}"
    );
    let drained = push_thread.join().unwrap();
    assert!(
        drained.status.success(),
        "the in-flight writer was killed, not drained: {}",
        String::from_utf8_lossy(&drained.stderr)
    );
    commit(&work, "after-timeout");
    let pushed = push(&work, &installation.url());
    assert!(
        pushed.status.success(),
        "the server still refuses writes after the failed gate: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
}
