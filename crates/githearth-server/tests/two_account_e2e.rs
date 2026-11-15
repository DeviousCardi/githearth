//! The two-account end-to-end journey (M4.1 acceptance case U21).
//!
//! One test drives the whole product journey the way two people would: `setup`
//! installs the server and its first administrator, the administrator creates
//! three more accounts through the administration API, an organization and a
//! repository are created, a team grant and email invitations join accounts two
//! and three, and account two clones, branches, commits, pushes, opens a pull
//! request, gets a review, and merges it — first over Git HTTP and then over
//! Git SSH. Denials along the way prove the journey is a forge and not a
//! script: an outside account reads neither the organization nor the
//! repository on the API or either transport, account three cannot push a
//! protected branch, an approval is required before a merge, and a removed
//! member's existing session and SSH key are refused at the next admission on
//! both transports.
//!
//! The acceptance path takes no fact from PostgreSQL. The only SQL in this
//! file creates and drops the test's disposable database, which is the fixture
//! the other PostgreSQL suites also use; every asserted fact comes from a
//! product API response, a product page, or a real `git` command. The
//! administrator, accounts, membership, team, invitation, protection, pull
//! request and merge are all reached through the running server.

use base64::Engine;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const ADMIN_USERNAME: &str = "admin";
const ADMIN_PASSWORD: &str = "administrator password for e2e";
const ACCOUNT_PASSWORD: &str = "e2e account password";
const USER_TWO: &str = "e2e-alice";
const USER_THREE: &str = "e2e-bob";
const USER_FOUR: &str = "e2e-mallory";
const ORG_SLUG: &str = "e2e-org";
const REPO_NAME: &str = "journey";
const TEAM_SLUG: &str = "platform";

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

fn with_database(base: &str, database: &str) -> String {
    let (origin, _) = base
        .rsplit_once('/')
        .expect("URL must end in a database name");
    format!("{origin}/{database}")
}

/// One disposable database for this run, removed on drop. This is the same
/// fixture shape the PostgreSQL suites use: SQL creates the empty database,
/// then `setup` owns every fact inside it.
struct Database {
    admin_url: String,
    name: String,
    url: String,
}

impl Database {
    fn new() -> Self {
        let admin_url = base_url();
        let name = format!("githearth_e2e_{}", Uuid::new_v4().simple());
        let url = with_database(&admin_url, &name);
        let create = name.clone();
        let connect = admin_url.clone();
        runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&connect)
                .await
                .expect("cannot connect to the disposable PostgreSQL database");
            sqlx::query(&format!("CREATE DATABASE {create}"))
                .execute(&pool)
                .await
                .expect("cannot create the test database");
            pool.close().await;
        });
        Self {
            admin_url,
            name,
            url,
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = runtime().block_on(async move {
            let pool = sqlx::PgPool::connect(&admin_url).await?;
            sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&pool)
                .await?;
            pool.close().await;
            Ok::<(), sqlx::Error>(())
        });
    }
}

// ---------------------------------------------------------------------------
// Installation and process control
// ---------------------------------------------------------------------------

fn nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// A private temporary installation directory. The name is short because the
/// hook socket path is bounded by the Unix socket limit.
struct Installation {
    path: PathBuf,
    http_port: u16,
    ssh_port: u16,
}

impl Installation {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gb7-{}-{}", std::process::id(), nonce()));
        std::fs::create_dir(&path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
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

    fn storage(&self) -> PathBuf {
        self.join("data/repositories")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn server_binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_githearth-server"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GITHEARTH_") {
            command.env_remove(name);
        }
    }
    command
}

fn run_setup(installation: &Installation, url: &str) -> Output {
    let mut command = server_binary();
    command
        .env("GITHEARTH_DATABASE_URL", url)
        .env("GITHEARTH_DATABASE_TIMEOUT_SECONDS", "5")
        .args([
            "setup",
            "--config",
            installation.config().to_str().unwrap(),
            "--storage-root",
            installation.storage().to_str().unwrap(),
            "--http-port",
            &installation.http_port.to_string(),
            "--ssh-port",
            &installation.ssh_port.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("setup must start");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ADMIN_PASSWORD.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

fn wait_for_ready(child: &mut Child, port: u16) -> bool {
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

/// The running `serve` child and its listeners. Killing it on drop keeps the
/// database droppable even when an assertion fails.
struct Running {
    child: Child,
    http_port: u16,
    ssh_port: u16,
    base: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(installation: &Installation, smtp_port: u16) -> Running {
    let log = installation.join("serve.log");
    let stderr = File::create(&log).unwrap();
    let stdout = File::create(installation.join("serve.out")).unwrap();
    let mut command = server_binary();
    command
        .args(["--config", installation.config().to_str().unwrap(), "serve"])
        // Product configuration, set the way an operator would: a loopback
        // SMTP relay (the test's sink) and the public URL the mail links use.
        .env("GITHEARTH_SMTP_HOST", "127.0.0.1")
        .env("GITHEARTH_SMTP_PORT", smtp_port.to_string())
        .env("GITHEARTH_SMTP_TLS", "none")
        .env("GITHEARTH_SMTP_FROM", "githearth@example.test")
        .env(
            "GITHEARTH_PUBLIC_URL",
            format!("http://127.0.0.1:{}", installation.http_port),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()));
    let mut child = command.spawn().expect("serve must start");
    let ready = wait_for_ready(&mut child, installation.http_port);
    if !ready {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        panic!(
            "serve never became ready on {}:\n{text}",
            installation.http_port
        );
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "serve exited after reporting ready:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    Running {
        child,
        http_port: installation.http_port,
        ssh_port: installation.ssh_port,
        base: format!("http://127.0.0.1:{}", installation.http_port),
    }
}

// ---------------------------------------------------------------------------
// A loopback SMTP sink, so verification mail is read the way a person would
// ---------------------------------------------------------------------------

struct MailSink {
    port: u16,
    messages: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MailSink {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = messages.clone();
        let halt = stop.clone();
        let join = thread::spawn(move || {
            for stream in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let received = accepted.clone();
                thread::spawn(move || handle_smtp(stream, received));
            }
        });
        Self {
            port,
            messages,
            stop,
            join: Some(join),
        }
    }

    /// Waits for the verification message addressed to `address` and returns
    /// the single-use token from its link. The message is removed, so a second
    /// call waits for a new one rather than replaying an old token.
    fn verification_token(&self, address: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(message) = self.take_for(address) {
                return extract_token(&message);
            }
            assert!(
                Instant::now() < deadline,
                "no verification mail arrived for {address}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn take_for(&self, address: &str) -> Option<String> {
        let mut messages = self.messages.lock().unwrap();
        let needle = format!("To: {address}");
        let index = messages
            .iter()
            .position(|message| message.contains(&needle) && message.contains("/verify-email/"))?;
        Some(messages.remove(index))
    }
}

impl Drop for MailSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle_smtp(mut stream: TcpStream, messages: Arc<Mutex<Vec<String>>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.write_all(b"220 localhost ESMTP\r\n");
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let command = line.trim_end_matches(['\r', '\n']).to_ascii_uppercase();
        if command.starts_with("EHLO") {
            let _ = stream.write_all(b"250-localhost\r\n250 OK\r\n");
        } else if command == "DATA" {
            let _ = stream.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n");
            let mut body = String::new();
            loop {
                let mut data = String::new();
                if reader.read_line(&mut data).unwrap_or(0) == 0 {
                    break;
                }
                if data.trim_end() == "." {
                    break;
                }
                body.push_str(&data);
            }
            messages.lock().unwrap().push(body);
            let _ = stream.write_all(b"250 OK\r\n");
        } else if command == "QUIT" {
            let _ = stream.write_all(b"221 Bye\r\n");
            break;
        } else {
            let _ = stream.write_all(b"250 OK\r\n");
        }
    }
}

fn extract_token(message: &str) -> String {
    let marker = "/verify-email/";
    let rest = message
        .rfind(marker)
        .map(|index| &message[index + marker.len()..])
        .expect("verification link in the message");
    let token: String = rest
        .chars()
        .take_while(|character| character.is_ascii_hexdigit())
        .collect();
    assert_eq!(token.len(), 64, "verification token in {message:?}");
    token
}

// ---------------------------------------------------------------------------
// Product API helpers (the server is a real process on a real socket)
// ---------------------------------------------------------------------------

fn http_request(url: &str, method: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    // outbound-exempt: test-only client of the test server's own socket
    let mut command = Command::new("curl");
    command.args([
        "-sS",
        "--max-time",
        "60",
        "-X",
        method,
        "-w",
        "\n%{http_code}",
    ]);
    if let Some(token) = token {
        command.args(["-H", &format!("authorization: Bearer {token}")]);
    }
    if let Some(body) = body {
        command.args([
            "-H",
            "content-type: application/json",
            "--data-binary",
            body,
        ]);
    }
    command.arg(url);
    let output = command.output().expect("curl must be installed");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "curl failed: {} {}",
        text,
        String::from_utf8_lossy(&output.stderr)
    );
    let (body, code) = text.rsplit_once('\n').expect("curl status suffix");
    (code.parse().expect("HTTP status code"), body.to_string())
}

fn api(
    server: &Running,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let body = body.map(|value| value.to_string());
    let (status, text) = http_request(
        &format!("{}{path}", server.base),
        method,
        token,
        body.as_deref(),
    );
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("non-JSON API response {status} for {method} {path}: {error}: {text}")
        })
    };
    (status, value)
}

fn login(server: &Running, username: &str, password: &str) -> String {
    let (status, body) = api(
        server,
        "POST",
        "/api/v1/login",
        None,
        Some(json!({"username": username, "password": password})),
    );
    assert_eq!(status, 200, "login for {username}: {body}");
    body["token"].as_str().expect("login token").to_owned()
}

fn create_account(server: &Running, admin: &str, username: &str) -> String {
    let (status, body) = api(
        server,
        "POST",
        "/api/v1/admin/accounts",
        Some(admin),
        Some(json!({"username": username, "password": ACCOUNT_PASSWORD})),
    );
    assert_eq!(status, 201, "create {username}: {body}");
    body["id"].as_str().expect("account id").to_owned()
}

fn verify_email(server: &Running, admin: &str, account_id: &str, address: &str, sink: &MailSink) {
    let (status, body) = api(
        server,
        "POST",
        &format!("/api/v1/admin/accounts/{account_id}/email-verification"),
        Some(admin),
        Some(json!({"email": address})),
    );
    assert_eq!(status, 202, "request verification for {address}: {body}");
    // The token reaches the account only as mail, through the outbox consumer.
    let token = sink.verification_token(address);
    let (status, body) = api(
        server,
        "POST",
        "/api/v1/email-verification/redeem",
        None,
        Some(json!({"token": token})),
    );
    assert_eq!(status, 204, "redeem verification for {address}: {body}");
}

fn invite(server: &Running, owner: &str, email: &str) -> String {
    let (status, body) = api(
        server,
        "POST",
        &format!("/api/v1/organizations/{ORG_SLUG}/invitations"),
        Some(owner),
        Some(json!({"email": email, "role": "member", "team": TEAM_SLUG})),
    );
    assert_eq!(status, 201, "invite {email}: {body}");
    body["token"].as_str().expect("invitation token").to_owned()
}

fn accept_invitation(server: &Running, token: &str, invitee: &str, org_id: &str) {
    let (status, body) = api(
        server,
        "POST",
        &format!("/api/v1/invitations/{token}"),
        Some(invitee),
        None,
    );
    assert_eq!(status, 200, "accept invitation: {body}");
    assert_eq!(body["role"], "member");
    assert_eq!(body["organization_id"], org_id);
}

fn register_ssh_key(server: &Running, token: &str, name: &str, key_path: &Path) {
    let public_key = std::fs::read_to_string(format!("{}.pub", key_path.display())).unwrap();
    let (status, body) = api(
        server,
        "POST",
        "/api/v1/user/ssh-keys",
        Some(token),
        Some(json!({"name": name, "public_key": public_key.trim()})),
    );
    assert_eq!(status, 201, "register ssh key for {name}: {body}");
}

fn wait_for_merged(server: &Running, token: &str, path: &str, number: i64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, pull) = api(
            server,
            "GET",
            &format!("{path}/{number}"),
            Some(token),
            None,
        );
        assert_eq!(status, 200, "read pull request {number}: {pull}");
        if pull["state"] == "merged" {
            assert!(pull["merged_at"].is_string(), "merged_at: {pull}");
            return pull;
        }
        assert!(
            Instant::now() < deadline,
            "pull request {number} never merged: {pull}"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Real Git, over both transports
// ---------------------------------------------------------------------------

fn git_command(args: &[&str], http_auth: Option<&str>, ssh: Option<(&Path, u16)>) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_SSL_NO_VERIFY", "1");
    if let Some(auth) = http_auth {
        command.args([
            "-c",
            &format!("http.extraHeader=Authorization: Basic {auth}"),
        ]);
    }
    if let Some((key, port)) = ssh {
        command.env(
            "GIT_SSH_COMMAND",
            format!(
                "ssh -i {} -p {port} -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes -o BatchMode=yes",
                key.display()
            ),
        );
    }
    command.args([
        "-c",
        "user.name=GitHearth E2E",
        "-c",
        "user.email=e2e@example.test",
        "-c",
        "http.lowSpeedLimit=1",
        "-c",
        "http.lowSpeedTime=30",
    ]);
    command.args(args);
    command
}

fn git_in(
    directory: Option<&Path>,
    args: &[&str],
    http_auth: Option<&str>,
    ssh: Option<(&Path, u16)>,
) -> Command {
    let mut command = git_command(args, http_auth, ssh);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command
}

fn git_run(mut command: Command) -> Output {
    command
        .output()
        .expect("git must be installed and the command launchable")
}

fn git_ok(command: Command) -> String {
    let output = git_run(command);
    assert!(
        output.status.success(),
        "git failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_fail(command: Command) -> String {
    let output = git_run(command);
    assert!(
        !output.status.success(),
        "expected git to fail, but it succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn basic_auth(username: &str, password: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
}

fn generate_ssh_key(path: &Path) {
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "githearth-e2e",
            "-q",
        ])
        .output()
        .expect("ssh-keygen must be installed");
    assert!(
        output.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_file(directory: &Path, name: &str, contents: &str) {
    std::fs::write(directory.join(name), contents).unwrap();
}

fn commit(directory: &Path, message: &str) {
    git_ok(git_in(Some(directory), &["add", "."], None, None));
    git_ok(git_in(
        Some(directory),
        &["commit", "-m", message],
        None,
        None,
    ));
}

fn head(directory: &Path) -> String {
    git_ok(git_in(Some(directory), &["rev-parse", "HEAD"], None, None))
}

// ---------------------------------------------------------------------------
// The journey
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
fn two_new_accounts_complete_the_journey_on_both_transports_without_sql() {
    // The fixture is the only SQL here: create an empty database. `setup` owns
    // every fact the test asserts, and every assertion below reads a product
    // response, a product page, or `git` output.
    let database = Database::new();
    let installation = Installation::new();
    let sink = MailSink::spawn();

    let setup = run_setup(&installation, &database.url);
    assert!(
        setup.status.success(),
        "setup stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );
    let installed = String::from_utf8_lossy(&setup.stdout);
    assert!(installed.contains("setup complete"), "{installed}");
    assert!(
        !installed.contains(ADMIN_PASSWORD),
        "password in setup output"
    );
    assert!(!installed.contains(&database.url), "database URL in output");

    // ---------------------------------------------------------------------
    // 1. Administrator, accounts and verified addresses
    // ---------------------------------------------------------------------
    let server = start_server(&installation, sink.port);
    let admin = login(&server, ADMIN_USERNAME, ADMIN_PASSWORD);

    let alice_id = create_account(&server, &admin, USER_TWO);
    let bob_id = create_account(&server, &admin, USER_THREE);
    let _mallory_id = create_account(&server, &admin, USER_FOUR);

    // The administrator asks for the link, the product mails it, and the
    // account redeems it; no SQL touches the verification row.
    verify_email(&server, &admin, &alice_id, "alice@example.test", &sink);
    verify_email(&server, &admin, &bob_id, "bob@example.test", &sink);

    let alice = login(&server, USER_TWO, ACCOUNT_PASSWORD);
    let bob = login(&server, USER_THREE, ACCOUNT_PASSWORD);
    let mallory = login(&server, USER_FOUR, ACCOUNT_PASSWORD);

    let work = installation.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let alice_auth = basic_auth(USER_TWO, ACCOUNT_PASSWORD);
    let bob_auth = basic_auth(USER_THREE, ACCOUNT_PASSWORD);
    let mallory_auth = basic_auth(USER_FOUR, ACCOUNT_PASSWORD);

    let alice_key = work.join("alice_ed25519");
    let bob_key = work.join("bob_ed25519");
    let mallory_key = work.join("mallory_ed25519");
    for (path, token, name) in [
        (&alice_key, &alice, "alice"),
        (&bob_key, &bob, "bob"),
        (&mallory_key, &mallory, "mallory"),
    ] {
        generate_ssh_key(path);
        register_ssh_key(&server, token, name, path);
    }

    // ---------------------------------------------------------------------
    // 2. Organization, private repository, team with a repository grant
    // ---------------------------------------------------------------------
    let (status, org) = api(
        &server,
        "POST",
        "/api/v1/organizations",
        Some(&admin),
        Some(json!({"slug": ORG_SLUG, "name": "E2E Organization"})),
    );
    assert_eq!(status, 201, "create organization: {org}");
    let org_id = org["id"].as_str().expect("organization id").to_owned();

    let repo_path = format!("/api/v1/organizations/{ORG_SLUG}/repositories/{REPO_NAME}");
    let (status, repo) = api(
        &server,
        "POST",
        &format!("/api/v1/organizations/{ORG_SLUG}/repositories"),
        Some(&admin),
        Some(json!({"name": REPO_NAME})),
    );
    assert_eq!(status, 201, "create repository: {repo}");
    assert_eq!(repo["visibility"], "private");
    assert_eq!(repo["organization_id"], org_id);

    let team_path = format!("/api/v1/organizations/{ORG_SLUG}/teams/{TEAM_SLUG}");
    let (status, team) = api(
        &server,
        "POST",
        &format!("/api/v1/organizations/{ORG_SLUG}/teams"),
        Some(&admin),
        Some(json!({"slug": TEAM_SLUG, "name": "Platform"})),
    );
    assert_eq!(status, 201, "create team: {team}");
    let (status, grant) = api(
        &server,
        "POST",
        &format!("{team_path}/grants"),
        Some(&admin),
        Some(json!({"repository": REPO_NAME, "role": "write"})),
    );
    assert_eq!(status, 201, "grant the team write access: {grant}");
    assert_eq!(grant["repository"], REPO_NAME);
    assert_eq!(grant["role"], "write");

    // ---------------------------------------------------------------------
    // 3. Invite and join, including the team path
    // ---------------------------------------------------------------------
    let invite_alice = invite(&server, &admin, "alice@example.test");
    let invite_bob = invite(&server, &admin, "bob@example.test");

    // Invited is not joined: the account still cannot read the organization.
    let (status, _) = api(
        &server,
        "GET",
        &format!("/api/v1/organizations/{org_id}"),
        Some(&alice),
        None,
    );
    assert_eq!(status, 404, "an invitee is not a member before acceptance");

    accept_invitation(&server, &invite_alice, &alice, &org_id);
    accept_invitation(&server, &invite_bob, &bob, &org_id);

    for (name, token) in [(USER_TWO, &alice), (USER_THREE, &bob)] {
        let (status, org) = api(
            &server,
            "GET",
            &format!("/api/v1/organizations/{org_id}"),
            Some(token),
            None,
        );
        assert_eq!(
            status, 200,
            "{name} can read the joined organization: {org}"
        );
        // The team grant, not only the membership, authorizes the repository.
        let (status, repo) = api(&server, "GET", &repo_path, Some(token), None);
        assert_eq!(status, 200, "{name} reads the granted repository: {repo}");
    }
    let (status, members) = api(
        &server,
        "GET",
        &format!("{team_path}/members"),
        Some(&admin),
        None,
    );
    assert_eq!(status, 200, "team members: {members}");
    let usernames: Vec<&str> = members
        .as_array()
        .expect("team member list")
        .iter()
        .filter_map(|member| member["username"].as_str())
        .collect();
    assert!(usernames.contains(&USER_TWO), "{usernames:?}");
    assert!(usernames.contains(&USER_THREE), "{usernames:?}");

    let http_url = format!(
        "http://127.0.0.1:{}/{ORG_SLUG}/{REPO_NAME}.git",
        server.http_port
    );
    let ssh_url = format!(
        "ssh://git@127.0.0.1:{}/{ORG_SLUG}/{REPO_NAME}.git",
        server.ssh_port
    );

    // ---------------------------------------------------------------------
    // 4. The outside account reads neither organization nor repository
    // ---------------------------------------------------------------------
    let (status, _) = api(
        &server,
        "GET",
        &format!("/api/v1/organizations/{org_id}"),
        Some(&mallory),
        None,
    );
    assert_eq!(status, 404, "outside account reads the organization");
    let (status, _) = api(&server, "GET", &repo_path, Some(&mallory), None);
    assert_eq!(status, 404, "outside account reads the repository");

    let stderr = git_fail(git_in(
        Some(&work),
        &["clone", http_url.as_str(), "mallory-http"],
        Some(&mallory_auth),
        None,
    ));
    let lower = stderr.to_ascii_lowercase();
    assert!(
        lower.contains("not found") || lower.contains("404"),
        "outside HTTP clone: {stderr}"
    );
    let stderr = git_fail(git_in(
        Some(&work),
        &["clone", ssh_url.as_str(), "mallory-ssh"],
        None,
        Some((&mallory_key, server.ssh_port)),
    ));
    assert!(
        stderr.contains("repository not found"),
        "outside SSH clone: {stderr}"
    );

    // ---------------------------------------------------------------------
    // 5. HTTP round: clone, branch, commit, push; then protect main
    // ---------------------------------------------------------------------
    let alice_http = work.join("alice-http");
    git_ok(git_in(
        Some(&work),
        &["clone", http_url.as_str(), "alice-http"],
        Some(&alice_auth),
        None,
    ));
    git_ok(git_in(
        Some(&alice_http),
        &["checkout", "-b", "main"],
        None,
        None,
    ));
    write_file(&alice_http, "README.md", "# Journey\n");
    commit(&alice_http, "Initial commit");
    git_ok(git_in(
        Some(&alice_http),
        &["push", "-u", "origin", "main"],
        Some(&alice_auth),
        None,
    ));

    let protection_path =
        format!("/api/v1/organizations/{ORG_SLUG}/repositories/{REPO_NAME}/protection");
    let (status, rule) = api(
        &server,
        "POST",
        &protection_path,
        Some(&admin),
        Some(json!({
            "pattern": "main",
            "require_pull_request": true,
            "required_approvals": 1,
            "allow_force_push": false,
            "allow_deletion": false,
        })),
    );
    assert_eq!(status, 201, "protect main: {rule}");
    let (status, rules) = api(&server, "GET", &protection_path, Some(&admin), None);
    assert_eq!(status, 200, "list protection: {rules}");
    assert_eq!(rules["items"][0]["required_approvals"], 1);
    assert_eq!(rules["items"][0]["require_pull_request"], true);

    // ---------------------------------------------------------------------
    // 6. Account three cannot push the protected branch, on both transports
    // ---------------------------------------------------------------------
    let bob_http = work.join("bob-http");
    git_ok(git_in(
        Some(&work),
        &["clone", http_url.as_str(), "bob-http"],
        Some(&bob_auth),
        None,
    ));
    write_file(&bob_http, "bob-direct.txt", "direct\n");
    commit(&bob_http, "Bob tries main over HTTP");
    let stderr = git_fail(git_in(
        Some(&bob_http),
        &["push", "origin", "main"],
        Some(&bob_auth),
        None,
    ));
    assert!(
        stderr.contains("protected branch: ref.push"),
        "protected HTTP push: {stderr}"
    );

    let bob_ssh = work.join("bob-ssh");
    git_ok(git_in(
        Some(&work),
        &["clone", ssh_url.as_str(), "bob-ssh"],
        None,
        Some((&bob_key, server.ssh_port)),
    ));
    write_file(&bob_ssh, "bob-direct-ssh.txt", "direct over ssh\n");
    commit(&bob_ssh, "Bob tries main over SSH");
    let stderr = git_fail(git_in(
        Some(&bob_ssh),
        &["push", "origin", "main"],
        None,
        Some((&bob_key, server.ssh_port)),
    ));
    assert!(
        stderr.contains("protected branch: ref.push"),
        "protected SSH push: {stderr}"
    );

    // ---------------------------------------------------------------------
    // 7. HTTP round: feature branch, pull request, approval, merge
    // ---------------------------------------------------------------------
    git_ok(git_in(
        Some(&alice_http),
        &["checkout", "-b", "http-feature"],
        None,
        None,
    ));
    write_file(&alice_http, "http-feature.txt", "HTTP transport\n");
    commit(&alice_http, "Add the HTTP feature");
    git_ok(git_in(
        Some(&alice_http),
        &["push", "origin", "http-feature"],
        Some(&alice_auth),
        None,
    ));

    let pulls = format!("/api/v1/organizations/{ORG_SLUG}/repositories/{REPO_NAME}/pulls");
    let (status, pull) = api(
        &server,
        "POST",
        &pulls,
        Some(&alice),
        Some(json!({
            "title": "HTTP transport round",
            "source_branch": "http-feature",
            "target_branch": "main",
        })),
    );
    assert_eq!(status, 201, "open the HTTP pull request: {pull}");
    let number = pull["number"].as_i64().expect("pull request number");
    let head_id = pull["head_id"]
        .as_str()
        .expect("pull request head")
        .to_owned();
    assert_eq!(head_id, head(&alice_http), "the push is the reviewed head");

    let merge_path = format!("{pulls}/{number}/merge");
    let (status, body) = api(
        &server,
        "POST",
        &merge_path,
        Some(&alice),
        Some(json!({"method": "merge", "expected_head_oid": head_id})),
    );
    assert_eq!(
        status, 403,
        "a merge without the required approval must be refused: {body}"
    );

    let (status, body) = api(
        &server,
        "POST",
        &format!("{pulls}/{number}/comments"),
        Some(&bob),
        Some(json!({"body": "Read the diff; it does what it says."})),
    );
    assert_eq!(status, 201, "review comment: {body}");
    let (status, body) = api(
        &server,
        "POST",
        &format!("{pulls}/{number}/reviews"),
        Some(&bob),
        Some(json!({
            "verdict": "approved",
            "commit_id": head_id,
            "body": "Approved after reading the diff.",
        })),
    );
    assert_eq!(status, 201, "approve the HTTP pull request: {body}");
    let (status, detail) = api(
        &server,
        "GET",
        &format!("{pulls}/{number}"),
        Some(&alice),
        None,
    );
    assert_eq!(status, 200, "read the reviewed pull request: {detail}");
    assert_eq!(detail["approvals"], 1);

    let (status, body) = api(
        &server,
        "POST",
        &merge_path,
        Some(&alice),
        Some(json!({"method": "merge", "expected_head_oid": head_id})),
    );
    assert_eq!(status, 201, "merge after approval: {body}");
    let merged = wait_for_merged(&server, &alice, &pulls, number);
    assert_eq!(merged["state"], "merged");
    // The merge result is readable by the next HTTP clone/fetch.
    git_ok(git_in(
        Some(&alice_http),
        &["fetch", "origin"],
        Some(&alice_auth),
        None,
    ));
    let shown = git_ok(git_in(
        Some(&alice_http),
        &["show", "origin/main:http-feature.txt"],
        None,
        None,
    ));
    assert!(shown.contains("HTTP transport"), "{shown}");

    // ---------------------------------------------------------------------
    // 8. SSH round: clone, branch, commit, push; then approval and merge
    // ---------------------------------------------------------------------
    let alice_ssh = work.join("alice-ssh");
    git_ok(git_in(
        Some(&work),
        &["clone", ssh_url.as_str(), "alice-ssh"],
        None,
        Some((&alice_key, server.ssh_port)),
    ));
    git_ok(git_in(
        Some(&alice_ssh),
        &["checkout", "-b", "ssh-feature"],
        None,
        None,
    ));
    write_file(&alice_ssh, "ssh-feature.txt", "SSH transport\n");
    commit(&alice_ssh, "Add the SSH feature");
    git_ok(git_in(
        Some(&alice_ssh),
        &["push", "origin", "ssh-feature"],
        None,
        Some((&alice_key, server.ssh_port)),
    ));

    let (status, pull) = api(
        &server,
        "POST",
        &pulls,
        Some(&alice),
        Some(json!({
            "title": "SSH transport round",
            "source_branch": "ssh-feature",
            "target_branch": "main",
        })),
    );
    assert_eq!(status, 201, "open the SSH pull request: {pull}");
    let number = pull["number"].as_i64().expect("pull request number");
    let head_id = pull["head_id"]
        .as_str()
        .expect("pull request head")
        .to_owned();
    assert_eq!(
        head_id,
        head(&alice_ssh),
        "the SSH push is the reviewed head"
    );
    let merge_path = format!("{pulls}/{number}/merge");

    // The same approval gate applies to the SSH round.
    let (status, body) = api(
        &server,
        "POST",
        &merge_path,
        Some(&alice),
        Some(json!({"method": "merge", "expected_head_oid": head_id})),
    );
    assert_eq!(
        status, 403,
        "an SSH round merge without approval must be refused: {body}"
    );
    let (status, body) = api(
        &server,
        "POST",
        &format!("{pulls}/{number}/reviews"),
        Some(&bob),
        Some(json!({
            "verdict": "approved",
            "commit_id": head_id,
            "body": "Approved after reviewing the SSH round.",
        })),
    );
    assert_eq!(status, 201, "approve the SSH pull request: {body}");
    let (status, body) = api(
        &server,
        "POST",
        &merge_path,
        Some(&alice),
        Some(json!({"method": "merge", "expected_head_oid": head_id})),
    );
    assert_eq!(status, 201, "merge the SSH round: {body}");
    let merged = wait_for_merged(&server, &alice, &pulls, number);
    assert_eq!(merged["state"], "merged");
    git_ok(git_in(
        Some(&alice_ssh),
        &["fetch", "origin"],
        None,
        Some((&alice_key, server.ssh_port)),
    ));
    let shown = git_ok(git_in(
        Some(&alice_ssh),
        &["show", "origin/main:ssh-feature.txt"],
        None,
        None,
    ));
    assert!(shown.contains("SSH transport"), "{shown}");

    // ---------------------------------------------------------------------
    // 9. A removed member's existing session and SSH key are refused next
    // ---------------------------------------------------------------------
    let (status, body) = api(
        &server,
        "DELETE",
        &format!("/api/v1/organizations/{ORG_SLUG}/members/{USER_THREE}"),
        Some(&admin),
        None,
    );
    assert_eq!(status, 204, "remove account three: {body}");

    let (status, _) = api(
        &server,
        "GET",
        &format!("/api/v1/organizations/{org_id}"),
        Some(&bob),
        None,
    );
    assert_eq!(
        status, 404,
        "removed member's session reads the organization"
    );
    let (status, _) = api(&server, "GET", &repo_path, Some(&bob), None);
    assert_eq!(status, 404, "removed member's session reads the repository");

    let stderr = git_fail(git_in(
        Some(&work),
        &["ls-remote", http_url.as_str()],
        Some(&bob_auth),
        None,
    ));
    let lower = stderr.to_ascii_lowercase();
    assert!(
        lower.contains("not found") || lower.contains("404"),
        "removed member's HTTP admission: {stderr}"
    );
    let stderr = git_fail(git_in(
        Some(&work),
        &["ls-remote", ssh_url.as_str()],
        None,
        Some((&bob_key, server.ssh_port)),
    ));
    assert!(
        stderr.contains("repository not found"),
        "removed member's SSH admission: {stderr}"
    );

    // The surviving member is still admitted on both transports, so the
    // removal denied the person it named rather than the installation.
    git_ok(git_in(
        Some(&work),
        &["ls-remote", http_url.as_str()],
        Some(&alice_auth),
        None,
    ));
    git_ok(git_in(
        Some(&work),
        &["ls-remote", ssh_url.as_str()],
        None,
        Some((&alice_key, server.ssh_port)),
    ));
}
