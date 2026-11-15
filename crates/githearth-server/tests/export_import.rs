//! U30a: the export contract round-trips, and a round trip that cannot fail
//! is not evidence.
//!
//! The harness exports a real installation, imports it into a fresh
//! installation, exports that installation, and compares counts, checksums and
//! sampled content. The seeded metadata is the ground truth: every count and
//! every sampled field below is asserted against what `seed_installation`
//! created, not merely against the second export. Assertions read the two
//! exports and native Git; no acceptance assertion queries PostgreSQL. The
//! database is used only to create the source installation and to reserve a
//! fresh destination, as the backup suite does.
//!
//! Deliberate failures damage one object's bytes, change a count in the
//! manifest, drop an object from the manifest (restating its part checksum),
//! alter one metadata field (restating the metadata part), and set an unknown
//! format version; every import must refuse before writing anything. A fork's
//! parent is checked in both UUID orders, because the export used to lose the
//! parent whose id sorted after its child's.

use githearth_git::object_store::{FsObjectStore, ObjectKey, ObjectStore, TenantId};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
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

const PASSWORD: &str = "correct horse battery staple for export";

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
        let name = format!("githearth_u30a_{}_{}", label, Uuid::new_v4().simple());
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

fn both(output: &Output) -> String {
    format!(
        "{}\n{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    )
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
            std::env::temp_dir().join(format!("ghu30a-{}-{}-{label}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let http_port = free_port();
        let mut ssh_port = free_port();
        while ssh_port == http_port {
            ssh_port = free_port();
        }
        Self {
            log: path.join("serve.log"),
            path,
            database: Database::new(label),
            http_port: http_port.max(1),
            ssh_port: ssh_port.max(1),
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
    }

    fn start(&mut self) {
        let log = fs::File::create(&self.log).unwrap();
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

    fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String) {
        http(self.http_port, method, path, token, body)
    }

    fn export(&self, archive: &Path) {
        let mut command = server();
        command
            .args(["--config", self.config().to_str().unwrap(), "export"])
            .arg("--archive")
            .arg(archive);
        let output = run(command);
        assert!(output.status.success(), "export: {}", both(&output));
    }
}

fn export_from_parts(database_url: &str, storage: &Path, archive: &Path) -> Output {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", database_url);
    command.args(["export", "--archive"]);
    command.arg(archive);
    command.arg("--storage-root").arg(storage);
    run(command)
}

fn import_export(archive: &Path, database_url: &str, storage: &Path) -> Output {
    let mut command = server();
    command.env("GITHEARTH_DATABASE_URL", database_url);
    command.args(["import", "--archive"]);
    command.arg(archive);
    command.arg("--storage-root").arg(storage);
    run(command)
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
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body.to_owned())
}

fn git(args: &[&str], directory: &Path) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Admin")
        .env("GIT_AUTHOR_EMAIL", "admin@example.test")
        .env("GIT_COMMITTER_NAME", "Admin")
        .env("GIT_COMMITTER_EMAIL", "admin@example.test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn push_history(installation: &Installation) -> (String, String) {
    let work = installation.path.join("work");
    fs::create_dir(&work).unwrap();
    git(&["init", "-q", "-b", "main"], &work);
    fs::write(work.join("README.md"), "first export sample\n").unwrap();
    fs::write(work.join("data.txt"), "sampled content\n").unwrap();
    git(&["add", "."], &work);
    git(&["commit", "-q", "-m", "first commit"], &work);
    git(&["push", "-q", &installation.url(), "main"], &work);
    git(&["checkout", "-q", "-b", "feature"], &work);
    fs::write(work.join("feature.txt"), "feature content\n").unwrap();
    git(&["add", "."], &work);
    git(&["commit", "-q", "-m", "feature commit"], &work);
    git(&["push", "-q", &installation.url(), "feature"], &work);
    let head = git(&["rev-parse", "HEAD"], &work);
    (head, git(&["rev-parse", "HEAD~1"], &work))
}

fn seed_installation(installation: &mut Installation) -> Uuid {
    installation.setup();
    installation.start();
    let token = installation.login();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations",
        Some(&token),
        Some(r#"{"slug":"acme","name":"Acme"}"#),
    );
    assert_eq!(status, 201, "create organization: {body}");
    let organization: Value = serde_json::from_str(&body).unwrap();
    let organization_id = organization["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/repositories",
        Some(&token),
        Some(r#"{"name":"widget","visibility":"private"}"#),
    );
    assert_eq!(status, 201, "create repository: {body}");

    let (commit, _parent) = push_history(installation);

    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/repositories/widget/issues",
        Some(&token),
        Some(r#"{"title":"Export issue","body":"Issue body travels in metadata.jsonl"}"#),
    );
    assert_eq!(status, 201, "create issue: {body}");

    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/repositories/widget/pulls",
        Some(&token),
        Some(
            r#"{"title":"Export pull","body":"Pull body","source_branch":"feature","target_branch":"main"}"#,
        ),
    );
    assert_eq!(status, 201, "create pull: {body}");
    let pull: Value = serde_json::from_str(&body).unwrap();
    let number = pull["number"].as_i64().unwrap();

    let review = serde_json::json!({
        "verdict": "commented",
        "commit_id": commit,
        "body": "A sampled review"
    });
    let (status, body) = installation.http(
        "POST",
        &format!("/api/v1/organizations/acme/repositories/widget/pulls/{number}/reviews"),
        Some(&token),
        Some(&review.to_string()),
    );
    assert_eq!(status, 201, "create review: {body}");

    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/teams",
        Some(&token),
        Some(r#"{"slug":"core","name":"Core"}"#),
    );
    assert_eq!(status, 201, "create team: {body}");
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/teams/core/members",
        Some(&token),
        Some(r#"{"username":"admin"}"#),
    );
    assert_eq!(status, 201, "add team member: {body}");
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/teams/core/grants",
        Some(&token),
        Some(r#"{"repository":"widget","role":"read"}"#),
    );
    assert_eq!(status, 201, "grant team: {body}");

    installation.stop();
    organization_id
}

fn add_object(installation: &Installation, tenant: Uuid) {
    let store = FsObjectStore::open(installation.storage().join("objects")).unwrap();
    let object = ObjectKey::parse("ci/artifacts/release.bin").unwrap();
    store
        .put(TenantId::new(tenant), &object, b"sampled object bytes\n")
        .unwrap();
}

fn create_repository(installation: &Installation, token: &str, name: &str) -> Uuid {
    let body = serde_json::json!({"name": name, "visibility": "private"}).to_string();
    let (status, body) = installation.http(
        "POST",
        "/api/v1/organizations/acme/repositories",
        Some(token),
        Some(&body),
    );
    assert_eq!(status, 201, "create repository {name}: {body}");
    serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// A fork's parent is one column, set at fork creation. The API assigns both
/// UUIDs, so to seed a chosen order the test names the parent after both ids
/// are known; the export reads the same column a real fork sets.
fn set_fork_parent(database_url: &str, child: Uuid, parent: Uuid) {
    let url = database_url.to_owned();
    runtime().block_on(async move {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query("UPDATE repositories SET parent_repository_id = $1 WHERE id = $2")
            .bind(parent)
            .bind(child)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    });
}

fn manifest(archive: &Path) -> Value {
    serde_json::from_slice(&fs::read(archive.join("manifest.json")).unwrap()).unwrap()
}

fn write_manifest(archive: &Path, manifest: &Value) {
    fs::write(
        archive.join("manifest.json"),
        serde_json::to_vec_pretty(manifest).unwrap(),
    )
    .unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Import a damaged export and require the refusal, the diagnostic, and that
/// nothing was created. Returns the diagnostic line for the report.
fn refused(archive: &Path, label: &str, expected: &str) -> String {
    let destination = Installation::reserve_destination(label);
    let storage = destination.path.join("imported-storage");
    let output = import_export(archive, &destination.database.url, &storage);
    let message = both(&output);
    assert!(
        !output.status.success(),
        "{label}: the import must refuse: {message}"
    );
    assert!(
        message.contains(expected),
        "{label}: expected {expected:?} in the refusal: {message}"
    );
    assert!(
        !destination.database.exists(),
        "{label}: a refused import must not leave a database behind"
    );
    assert!(
        !storage.exists(),
        "{label}: a refused import must not publish storage"
    );
    message
        .lines()
        .find(|line| line.contains(expected))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Assert one repository record names `parent`, in this export's metadata.
fn assert_parent(lines: &[String], child: Uuid, parent: Uuid) {
    let record = lines
        .iter()
        .find(|line| {
            line.contains("\"kind\":\"repository\"")
                && line.contains(&format!("\"id\":\"{child}\""))
        })
        .unwrap_or_else(|| panic!("no repository record for {child}"));
    assert!(
        record.contains(&format!("\"parent_repository_id\":\"{parent}\"")),
        "repository {child} must name parent {parent}: {record}"
    );
}

fn lines_without_header(path: &Path) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["format_version"], 1);
    lines
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn object_payload(archive: &Path) -> (String, Vec<u8>) {
    let text = fs::read_to_string(archive.join("objects.jsonl")).unwrap();
    let record: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    let relative = record["path"].as_str().unwrap();
    let digest = record["sha256"].as_str().unwrap().to_owned();
    (digest, fs::read(archive.join(relative)).unwrap())
}

/// U30a: export, import into a fresh installation, export again, and compare
/// counts, checksums and sampled content.
#[test]
#[ignore = "requires PostgreSQL and git; run with --include-ignored"]
fn export_import_round_trip_agrees_on_counts_checksums_and_content() {
    let mut source = Installation::new("roundtrip");
    let tenant = seed_installation(&mut source);
    add_object(&source, tenant);

    let first = source.path.join("first-export");
    source.export(&first);

    let mut destination = Installation::reserve_destination("roundtrip-dest");
    let imported_storage = destination.path.join("imported-storage");
    let output = import_export(&first, &destination.database.url, &imported_storage);
    assert!(output.status.success(), "import: {}", both(&output));
    destination.database.created = true;

    let second = destination.path.join("second-export");
    let output = export_from_parts(&destination.database.url, &imported_storage, &second);
    assert!(output.status.success(), "re-export: {}", both(&output));

    let first_manifest = manifest(&first);
    let second_manifest = manifest(&second);
    assert_eq!(first_manifest["format"], "githearth-export");
    assert_eq!(first_manifest["format_version"], 1);
    assert_eq!(
        first_manifest["counts"], second_manifest["counts"],
        "metadata counts must match"
    );
    assert_eq!(
        first_manifest["objects"]["records"], second_manifest["objects"]["records"],
        "object counts must match"
    );
    assert_eq!(
        first_manifest["objects"]["payload_bytes"], second_manifest["objects"]["payload_bytes"],
        "object payload total must match"
    );
    assert_eq!(
        lines_without_header(&first.join("objects.jsonl")),
        lines_without_header(&second.join("objects.jsonl")),
        "every object's tenant, key, size and checksum must match"
    );
    assert_eq!(
        first_manifest["git"], second_manifest["git"],
        "repository refs checksums and HEADs must match"
    );

    let first_lines = lines_without_header(&first.join("metadata.jsonl"));
    let second_lines = lines_without_header(&second.join("metadata.jsonl"));
    assert_eq!(
        first_lines, second_lines,
        "every metadata record, including sampled issue/PR/review/team/grant content, must \
         agree"
    );

    // Anchor the metadata to the seeded installation: comparing the two exports
    // to each other alone would accept an exporter that dropped a kind or
    // swapped two fields, because both exports would lose it identically.
    let counts = first_manifest["counts"]
        .as_object()
        .expect("the manifest has counts");
    for (kind, expected) in [
        ("account", 1u64),
        ("organization", 1),
        ("membership", 1),
        ("team", 1),
        ("team_member", 1),
        ("repository", 1),
        // The creator's owner grant on the organization, plus the team's read
        // grant on the repository.
        ("grant", 2),
        ("issue", 1),
        ("pull_request", 1),
        ("pull_request_review", 1),
    ] {
        assert_eq!(
            counts.get(kind).and_then(Value::as_u64),
            Some(expected),
            "the seed creates exactly {expected} {kind} record(s)"
        );
    }
    assert_eq!(
        counts.len(),
        10,
        "the export carries exactly the ten covered kinds: {counts:?}"
    );

    let issue_line = first_lines
        .iter()
        .find(|line| line.contains("\"kind\":\"issue\""))
        .expect("the seeded issue is exported");
    assert!(
        issue_line.contains(r#""title":"Export issue""#),
        "{issue_line}"
    );
    assert!(
        issue_line.contains("Issue body travels in metadata.jsonl"),
        "{issue_line}"
    );
    let pull_line = first_lines
        .iter()
        .find(|line| line.contains("\"kind\":\"pull_request\""))
        .expect("the seeded pull request is exported");
    assert!(
        pull_line.contains(r#""title":"Export pull""#),
        "{pull_line}"
    );
    assert!(pull_line.contains(r#""body":"Pull body""#), "{pull_line}");
    assert!(
        pull_line.contains(r#""source_branch":"feature""#)
            && pull_line.contains(r#""target_branch":"main""#),
        "{pull_line}"
    );
    let review_line = first_lines
        .iter()
        .find(|line| line.contains("\"kind\":\"pull_request_review\""))
        .expect("the seeded review is exported");
    assert!(review_line.contains("A sampled review"), "{review_line}");
    assert!(
        review_line.contains(r#""verdict":"commented""#),
        "{review_line}"
    );
    let repository_line = first_lines
        .iter()
        .find(|line| line.contains("\"kind\":\"repository\""))
        .expect("the seeded repository is exported");
    assert!(
        repository_line.contains(r#""name":"widget""#),
        "{repository_line}"
    );
    assert!(
        repository_line.contains(r#""parent_repository_id":null"#),
        "a plain repository is not a fork: {repository_line}"
    );
    let account_line = first_lines
        .iter()
        .find(|line| line.contains("\"kind\":\"account\""))
        .expect("the admin account is exported");
    assert!(
        account_line.contains(r#""username":"admin""#),
        "{account_line}"
    );
    assert!(
        !account_line.contains("password"),
        "the account record carries no credential: {account_line}"
    );
    assert!(
        first_lines
            .iter()
            .any(|line| line.contains("\"kind\":\"team_member\"")),
        "the seeded team member is exported"
    );

    // The manifest's omitted set is part of the contract; assert the classes
    // the review found unrecorded are now named.
    assert_eq!(
        first_manifest["excluded"], second_manifest["excluded"],
        "the omitted set is stable across a round trip"
    );
    let excluded = first_manifest["excluded"]
        .as_array()
        .expect("excluded is a list")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    for phrase in [
        "invitations",
        "review requests",
        "redirects",
        "auto-merge",
        "notification preferences",
    ] {
        assert!(
            excluded.contains(phrase),
            "excluded[] must record {phrase:?}: {excluded}"
        );
    }

    let (first_digest, first_bytes) = object_payload(&first);
    let (second_digest, second_bytes) = object_payload(&second);
    assert_eq!(first_digest, second_digest);
    assert_eq!(first_bytes, second_bytes, "sampled object bytes must agree");
    assert_eq!(first_bytes, b"sampled object bytes\n");

    // Native Git: the exported bare repository is the history. Compare the
    // refs checksum already above, plus a sampled commit and file.
    let git_part = first_manifest["git"]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| part["refs"].as_u64().unwrap_or(0) >= 2)
        .expect("the seeded repository has main and feature");
    let repository = git_part["repository"].as_str().unwrap();
    let first_git = first.join(format!("git/{repository}.git"));
    let second_git = second.join(format!("git/{repository}.git"));
    let first_head = Command::new("git")
        .args(["--git-dir"])
        .arg(&first_git)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let second_head = Command::new("git")
        .args(["--git-dir"])
        .arg(&second_git)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(first_head.stdout, second_head.stdout);
    let readme = Command::new("git")
        .args(["--git-dir"])
        .arg(&second_git)
        .args(["show", "HEAD:README.md"])
        .output()
        .unwrap();
    assert!(readme.status.success());
    assert_eq!(readme.stdout, b"first export sample\n");

    // The imported installation is not a byte copy of the configuration or of
    // any secret: password hashes were not exported.
    assert!(
        !fs::read_to_string(first.join("metadata.jsonl"))
            .unwrap()
            .contains("password_hash")
    );
}

/// The deliberate failures: damaged bytes, a count disagreement, an object
/// dropped from the manifest, and one altered metadata field. Each must make
/// the import refuse before it writes anything.
#[test]
#[ignore = "requires PostgreSQL and git; run with --include-ignored"]
fn deliberate_failures_are_refused_before_writing() {
    let mut source = Installation::new("deliberate");
    let tenant = seed_installation(&mut source);
    add_object(&source, tenant);

    let pristine = source.path.join("pristine-export");
    source.export(&pristine);

    // 1. Alter one sampled object's bytes.
    let archive = source.path.join("case-bytes");
    copy_tree(&pristine, &archive);
    let (_, payload) = object_payload(&archive);
    assert_eq!(payload, b"sampled object bytes\n");
    let mut bytes = payload;
    bytes[0] ^= 0xff;
    let text = fs::read_to_string(archive.join("objects.jsonl")).unwrap();
    let record: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    fs::write(archive.join(record["path"].as_str().unwrap()), &bytes).unwrap();
    let failure = refused(&archive, "case-bytes-dest", "failed verification");
    println!("case bytes: {failure}");

    // 2. Change a count in the metadata's manifest.
    let archive = source.path.join("case-count");
    copy_tree(&pristine, &archive);
    let mut changed = manifest(&archive);
    assert_eq!(changed["counts"]["issue"], 1);
    changed["counts"]["issue"] = Value::from(2);
    write_manifest(&archive, &changed);
    let failure = refused(
        &archive,
        "case-count-dest",
        "metadata counts do not match its manifest",
    );
    println!("case count: {failure}");

    // 3. Delete the object record from objects.jsonl, leave the payload, and
    // restate the part checksum and counts so only the exact-set check can
    // catch it.
    let archive = source.path.join("case-manifest");
    copy_tree(&pristine, &archive);
    let objects_path = archive.join("objects.jsonl");
    let text = fs::read_to_string(&objects_path).unwrap();
    let header = text.lines().next().unwrap().to_owned();
    let trimmed = format!("{header}\n");
    fs::write(&objects_path, &trimmed).unwrap();
    let mut changed = manifest(&archive);
    changed["objects"]["records"] = Value::from(0);
    changed["objects"]["payload_bytes"] = Value::from(0);
    changed["objects"]["bytes"] = Value::from(trimmed.len() as u64);
    changed["objects"]["sha256"] = Value::from(sha256_hex(trimmed.as_bytes()));
    write_manifest(&archive, &changed);
    let failure = refused(
        &archive,
        "case-manifest-dest",
        "object payloads are not exactly the manifest",
    );
    println!("case manifest: {failure}");

    // 4. Alter one metadata field (the issue's repository) and restate the
    // metadata part, so the checksum cannot catch it and the reference check
    // must.
    let archive = source.path.join("case-field");
    copy_tree(&pristine, &archive);
    let metadata_path = archive.join("metadata.jsonl");
    let text = fs::read_to_string(&metadata_path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let issue = lines
        .iter_mut()
        .find(|line| line.contains("\"kind\":\"issue\""))
        .expect("the seeded issue is exported");
    let record: Value = serde_json::from_str(issue).unwrap();
    let repository_id = record["repository_id"].as_str().unwrap().to_owned();
    *issue = issue.replace(&repository_id, &Uuid::new_v4().to_string());
    let rewritten = format!("{}\n", lines.join("\n"));
    fs::write(&metadata_path, &rewritten).unwrap();
    let mut changed = manifest(&archive);
    changed["metadata"]["bytes"] = Value::from(rewritten.len() as u64);
    changed["metadata"]["sha256"] = Value::from(sha256_hex(rewritten.as_bytes()));
    write_manifest(&archive, &changed);
    let failure = refused(&archive, "case-field-dest", "names repository");
    println!("case field: {failure}");
}

/// B1: a fork's parent must survive export and import whichever UUID sorts
/// first, because the export query orders by id.
#[test]
#[ignore = "requires PostgreSQL and git; run with --include-ignored"]
fn a_fork_parent_survives_in_both_uuid_orders() {
    let mut source = Installation::new("fork-order");
    source.setup();
    source.start();
    let token = source.login();
    let (status, body) = source.http(
        "POST",
        "/api/v1/organizations",
        Some(&token),
        Some(r#"{"slug":"acme","name":"Acme"}"#),
    );
    assert_eq!(status, 201, "create organization: {body}");
    let first = create_repository(&source, &token, "first");
    let second = create_repository(&source, &token, "second");
    let third = create_repository(&source, &token, "third");
    let fourth = create_repository(&source, &token, "fourth");
    source.stop();

    // Both orders, deterministically: parent > child and parent < child.
    let (parent_after_child, child_before_parent) = if first > second {
        (first, second)
    } else {
        (second, first)
    };
    let (parent_before_child, child_after_parent) = if third < fourth {
        (third, fourth)
    } else {
        (fourth, third)
    };
    set_fork_parent(
        &source.database.url,
        child_before_parent,
        parent_after_child,
    );
    set_fork_parent(
        &source.database.url,
        child_after_parent,
        parent_before_child,
    );

    let archive = source.path.join("fork-export");
    source.export(&archive);
    let first_lines = lines_without_header(&archive.join("metadata.jsonl"));
    assert_parent(&first_lines, child_before_parent, parent_after_child);
    assert_parent(&first_lines, child_after_parent, parent_before_child);

    let mut destination = Installation::reserve_destination("fork-order-dest");
    let storage = destination.path.join("imported-storage");
    let output = import_export(&archive, &destination.database.url, &storage);
    assert!(output.status.success(), "import: {}", both(&output));
    destination.database.created = true;
    let reexported = destination.path.join("fork-reexport");
    let output = export_from_parts(&destination.database.url, &storage, &reexported);
    assert!(output.status.success(), "re-export: {}", both(&output));
    let second_lines = lines_without_header(&reexported.join("metadata.jsonl"));
    assert_parent(&second_lines, child_before_parent, parent_after_child);
    assert_parent(&second_lines, child_after_parent, parent_before_child);
}

/// An unknown version and a non-empty destination are both refused; the second
/// run over a successful import proves the command is refusable, not idempotent.
#[test]
#[ignore = "requires PostgreSQL and git; run with --include-ignored"]
fn unknown_versions_and_non_empty_destinations_are_refused() {
    let mut source = Installation::new("refusals");
    source.setup();
    source.start();
    let token = source.login();
    let (status, body) = source.http(
        "POST",
        "/api/v1/organizations",
        Some(&token),
        Some(r#"{"slug":"acme","name":"Acme"}"#),
    );
    assert_eq!(status, 201, "create organization: {body}");
    source.stop();

    let archive = source.path.join("refusals-export");
    source.export(&archive);

    // A non-empty destination storage is refused and left untouched, and no
    // database is created.
    let non_empty = Installation::reserve_destination("non-empty-dest");
    let occupied = non_empty.path.join("imported-storage");
    fs::create_dir_all(&occupied).unwrap();
    let marker = occupied.join("keep.txt");
    fs::write(&marker, "do not touch\n").unwrap();
    let output = import_export(&archive, &non_empty.database.url, &occupied);
    assert!(
        !output.status.success(),
        "a non-empty storage root must not import: {}",
        both(&output)
    );
    assert!(
        both(&output).contains("is not empty"),
        "the refusal names the destination storage: {}",
        both(&output)
    );
    assert!(!non_empty.database.exists());
    assert_eq!(fs::read_to_string(&marker).unwrap(), "do not touch\n");

    // Unknown version: bump the manifest only. The reader must refuse it
    // before the metadata header could disagree.
    let mut changed = manifest(&archive);
    changed["format_version"] = Value::from(2);
    fs::write(
        archive.join("manifest.json"),
        serde_json::to_vec_pretty(&changed).unwrap(),
    )
    .unwrap();
    let mut destination = Installation::reserve_destination("version-dest");
    let storage = destination.path.join("imported-storage");
    let output = import_export(&archive, &destination.database.url, &storage);
    assert!(!output.status.success(), "{}", both(&output));
    assert!(
        both(&output).contains("unsupported export format version 2"),
        "{}",
        both(&output)
    );
    assert!(!destination.database.exists());
    assert!(!storage.exists());

    // Restore version 1 and import for real; then the same destination must be
    // refused on a re-run.
    changed["format_version"] = Value::from(1);
    fs::write(
        archive.join("manifest.json"),
        serde_json::to_vec_pretty(&changed).unwrap(),
    )
    .unwrap();
    let output = import_export(&archive, &destination.database.url, &storage);
    assert!(output.status.success(), "first import: {}", both(&output));
    destination.database.created = true;
    let output = import_export(&archive, &destination.database.url, &storage);
    assert!(!output.status.success(), "a re-run must be refused");
    assert!(
        both(&output).contains("already exists"),
        "the refusal names the destination database: {}",
        both(&output)
    );
}

impl Installation {
    fn reserve_destination(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ghu30a-{}-{}-{label}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            log: path.join("serve.log"),
            path,
            database: Database::reserve(label),
            http_port: 1,
            ssh_port: 1,
            child: None,
        }
    }
}
