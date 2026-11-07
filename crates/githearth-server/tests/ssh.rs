//! SSH transport integration tests (ADR 0005, Person B).
//!
//! Start one server state with SSH enabled on 127.0.0.1:0, inject a fake
//! authenticate mapping a generated client key's SHA-256 fingerprint to an
//! account, then exercise the system `ssh` and `git` clients against it.
//!
//! All tests are `#[ignore]` — they require a real PostgreSQL database via
//! `GITHEARTH_TEST_DATABASE_URL` and a system `ssh` client.

use githearth_server::{AppState, MIGRATOR, config::Config, connect, ssh};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        Uuid::new_v4().to_string().split('-').next().unwrap()
    )
}

fn has_ssh() -> bool {
    Command::new("ssh").arg("-V").output().is_ok()
}

fn ssh_keygen_generate(key_path: &Path) {
    let _ = std::fs::remove_file(key_path);
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &key_path.to_string_lossy(),
            "-N",
            "",
            "-C",
            "githearth-test",
            "-q",
        ])
        .output()
        .expect("ssh-keygen failed");
    assert!(
        output.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_fingerprint(key_path: &Path) -> String {
    let output = Command::new("ssh-keygen")
        .args(["-lf", &key_path.to_string_lossy(), "-E", "sha256"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split_whitespace().nth(1).unwrap().to_string()
}

fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_SSL_NO_VERIFY", "1")
        .args([
            "-c",
            "http.lowSpeedLimit=1",
            "-c",
            "http.lowSpeedTime=30",
            "-c",
            "user.name=githearth-test",
            "-c",
            "user.email=test@githearth.local",
        ])
        .args(args);
    cmd
}

async fn git_run(mut cmd: Command) -> std::process::Output {
    match tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || cmd.output()),
    )
    .await
    {
        Ok(Ok(Ok(out))) => out,
        Ok(Ok(Err(e))) => panic!("git command failed to launch: {e}"),
        Ok(Err(_)) => panic!("spawn_blocking join error"),
        Err(_) => panic!("git command timed out"),
    }
}

async fn git_output(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn git_fail_with(cmd: Command, expected_stderr: &str) {
    let out = git_run(cmd).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "expected git failure, got success");
    assert!(
        stderr.contains(expected_stderr),
        "expected '{expected_stderr}' in stderr, got: {stderr}"
    );
}

fn temp_storage_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("githearth-ssh-int-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn git_ssh_key_env(key_path: &Path, port: u16) -> String {
    format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key_path.display()
    )
}

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("githearth-{}-{}", label, Uuid::new_v4()))
}

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maint)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maint)
        .await
        .unwrap();
    format!("{origin}/{name}")
}

async fn fixture_account(pool: &PgPool, username: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused')")
        .bind(id)
        .bind(username)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("sshorg");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Org')")
        .bind(id)
        .bind(&slug)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    (id, slug)
}

async fn create_active_repo(pool: &PgPool, org_id: Uuid, name: &str, owner: Uuid) -> Uuid {
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind(name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    repo_id
}

async fn seed_bare(repo_path: &str, work_dir: &str) {
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work_dir,
        "HEAD:refs/heads/main",
    ]))
    .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_clone_works() {
    if !has_ssh() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }

    let storage_root = temp_storage_root();
    let db_name = format!(
        "githearth_tssh_c1_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let key_dir = temp_dir("ssh-key-c1");
    std::fs::create_dir_all(&key_dir).unwrap();

    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen_generate(&client_key);
    let fp = sha256_fingerprint(&client_key);

    let config = Config {
        database_url: database_url.clone(),
        storage_root: storage_root.clone(),
        ssh_enabled: true,
        // The hooks must exec the real server binary, not this test harness.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };

    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let account_id = fixture_account(&pool, "sshclone").await;
    let (org_id, org_slug) = fixture_organization(&pool, account_id).await;
    let repo_id = create_active_repo(&pool, org_id, "testrepo", account_id).await;

    // Create through the storage layer, as serve does (journal dir, config).
    let repo_path = githearth_git::layout::Storage::open(&storage_root)
        .unwrap()
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let work_dir = temp_dir("ssh-clone-w");
    std::fs::create_dir_all(&work_dir).unwrap();
    git_ok(git_cmd(&["init", &work_dir.to_string_lossy()])).await;
    std::fs::write(work_dir.join("README.md"), "# test\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        &work_dir.to_string_lossy(),
        "add",
        "README.md",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        &work_dir.to_string_lossy(),
        "commit",
        "-m",
        "init",
    ]))
    .await;
    seed_bare(&repo_path.to_string_lossy(), &work_dir.to_string_lossy()).await;

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();

    let authenticate: ssh::Authenticate = Arc::new({
        move |f: String| {
            let result = if f == fp {
                Ok(Some(ssh::SshIdentity {
                    account: account_id,
                    key_id: Uuid::new_v4(),
                    read_only: false,
                }))
            } else {
                Ok(None)
            };
            Box::pin(async move { result })
        }
    });

    let (_ssh_handle, port_rx) = ssh::spawn(state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();

    let clone_dir = temp_dir("ssh-clone");
    let _ = std::fs::remove_dir_all(&clone_dir);

    let ssh_cmd_env = git_ssh_key_env(&client_key, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{org_slug}/testrepo.git");
    let mut cmd = git_cmd(&["clone", &ssh_url, &clone_dir.to_string_lossy()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_cmd_env);

    git_ok(cmd).await;

    assert!(
        clone_dir.join("README.md").exists(),
        "clone produced no files"
    );
    eprintln!("SSH clone works at port {ssh_port}");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_push_works() {
    if !has_ssh() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }

    let storage_root = temp_storage_root();
    let db_name = format!(
        "githearth_tssh_p1_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let key_dir = temp_dir("ssh-key-p1");
    std::fs::create_dir_all(&key_dir).unwrap();

    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen_generate(&client_key);
    let fp = sha256_fingerprint(&client_key);

    let config = Config {
        database_url: database_url.clone(),
        storage_root: storage_root.clone(),
        ssh_enabled: true,
        // The hooks must exec the real server binary, not this test harness.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };

    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let account_id = fixture_account(&pool, "sshpush").await;
    let (org_id, org_slug) = fixture_organization(&pool, account_id).await;
    let repo_id = create_active_repo(&pool, org_id, "pushrepo", account_id).await;

    // Create through the storage layer, as serve does (journal dir, config).
    let repo_path = githearth_git::layout::Storage::open(&storage_root)
        .unwrap()
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    // Seed with initial commit so we can push changes
    let work_dir = temp_dir("ssh-push-w");
    std::fs::create_dir_all(&work_dir).unwrap();
    git_ok(git_cmd(&["init", &work_dir.to_string_lossy()])).await;
    std::fs::write(work_dir.join("seed.txt"), "seed\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        &work_dir.to_string_lossy(),
        "add",
        "seed.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        &work_dir.to_string_lossy(),
        "commit",
        "-m",
        "seed",
    ]))
    .await;
    seed_bare(&repo_path.to_string_lossy(), &work_dir.to_string_lossy()).await;

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();

    let authenticate: ssh::Authenticate = Arc::new({
        move |f: String| {
            let result = if f == fp {
                Ok(Some(ssh::SshIdentity {
                    account: account_id,
                    key_id: Uuid::new_v4(),
                    read_only: false,
                }))
            } else {
                Ok(None)
            };
            Box::pin(async move { result })
        }
    });

    let (_ssh_handle, port_rx) = ssh::spawn(state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();

    // Clone first to set up the remote, then push a change
    let clone_dir = temp_dir("ssh-push-clone");
    let _ = std::fs::remove_dir_all(&clone_dir);

    let ssh_cmd_env = git_ssh_key_env(&client_key, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{org_slug}/pushrepo.git");
    let mut cmd = git_cmd(&["clone", &ssh_url, &clone_dir.to_string_lossy()]);
    cmd.env("GIT_SSH_COMMAND", &ssh_cmd_env);
    git_ok(cmd).await;

    // Make a change and push
    std::fs::write(clone_dir.join("new.txt"), "pushed via SSH\n").unwrap();
    git_ok(git_cmd(&[
        "-C",
        &clone_dir.to_string_lossy(),
        "add",
        "new.txt",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        &clone_dir.to_string_lossy(),
        "commit",
        "-m",
        "ssh push",
    ]))
    .await;

    let mut push_cmd = git_cmd(&[
        "-C",
        &clone_dir.to_string_lossy(),
        "push",
        "origin",
        "HEAD:refs/heads/main",
    ]);
    push_cmd.env("GIT_SSH_COMMAND", &ssh_cmd_env);
    git_ok(push_cmd).await;

    // The push went through the same pipeline as HTTP: ref moved, journaled, replayed.
    let local = git_output(git_cmd(&[
        "-C",
        &clone_dir.to_string_lossy(),
        "rev-parse",
        "HEAD",
    ]))
    .await;
    let remote = git_output(git_cmd(&[
        "--git-dir",
        &repo_path.to_string_lossy(),
        "rev-parse",
        "refs/heads/main",
    ]))
    .await;
    assert_eq!(local.trim(), remote.trim(), "ref did not move");
    assert!(
        githearth_git::journal::verify(&repo_path).unwrap() >= 1,
        "push was not journaled"
    );
    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ref_updates WHERE repository_id = $1")
            .bind(repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        applied >= 1,
        "journal record was not replayed into PostgreSQL"
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_non_member_receives_not_found() {
    if !has_ssh() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }

    let storage_root = temp_storage_root();
    let db_name = format!(
        "githearth_tssh_nf1_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let key_dir = temp_dir("ssh-key-nf1");
    std::fs::create_dir_all(&key_dir).unwrap();

    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen_generate(&client_key);
    let fp = sha256_fingerprint(&client_key);

    let config = Config {
        database_url: database_url.clone(),
        storage_root: storage_root.clone(),
        ssh_enabled: true,
        // The hooks must exec the real server binary, not this test harness.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };

    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    // Create a non-member account
    let outsider_id = fixture_account(&pool, "outsider").await;
    // Create org + repo owned by a different account
    let owner_id = fixture_account(&pool, "rep3owner").await;
    let (org_id, org_slug) = fixture_organization(&pool, owner_id).await;
    let repo_id = create_active_repo(&pool, org_id, "secret", owner_id).await;

    // Create through the storage layer, as serve does (journal dir, config).
    githearth_git::layout::Storage::open(&storage_root)
        .unwrap()
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();

    let authenticate: ssh::Authenticate = Arc::new({
        move |f: String| {
            let result = if f == fp {
                Ok(Some(ssh::SshIdentity {
                    account: outsider_id,
                    key_id: Uuid::new_v4(),
                    read_only: false,
                }))
            } else {
                Ok(None)
            };
            Box::pin(async move { result })
        }
    });

    let (_ssh_handle, port_rx) = ssh::spawn(state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();

    let ssh_cmd_env = git_ssh_key_env(&client_key, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/{org_slug}/secret.git");
    let mut cmd = git_cmd(&["ls-remote", &ssh_url]);
    cmd.env("GIT_SSH_COMMAND", &ssh_cmd_env);

    git_fail_with(cmd, "repository not found").await;

    eprintln!("SSH not-found works at port {ssh_port}");
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_unregistered_key_rejected() {
    if !has_ssh() {
        eprintln!("SKIP: ssh binary not found");
        return;
    }

    let storage_root = temp_storage_root();
    let db_name = format!(
        "githearth_tssh_ur1_{}",
        Uuid::new_v4().to_string().replace('-', "_")
    );
    let database_url = fresh_database(&db_name).await;
    let key_dir = temp_dir("ssh-key-ur1");
    std::fs::create_dir_all(&key_dir).unwrap();

    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen_generate(&client_key);

    let config = Config {
        database_url: database_url.clone(),
        storage_root: storage_root.clone(),
        ssh_enabled: true,
        // The hooks must exec the real server binary, not this test harness.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };

    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();

    let authenticate: ssh::Authenticate = Arc::new(|_f: String| Box::pin(async { Ok(None) }));

    let (_ssh_handle, port_rx) = ssh::spawn(state.clone(), authenticate);
    let ssh_port = port_rx.await.unwrap().port();

    let ssh_cmd_env = git_ssh_key_env(&client_key, ssh_port);
    let ssh_url = format!("ssh://git@127.0.0.1:{ssh_port}/org/repo.git");
    let mut cmd = git_cmd(&["ls-remote", &ssh_url]);
    cmd.env("GIT_SSH_COMMAND", &ssh_cmd_env);

    let out = git_run(cmd).await;
    assert!(!out.status.success(), "expected auth failure");

    eprintln!("SSH unregistered key rejected at port {ssh_port}");
}

/// A running SSH listener over a fresh database with one active repository
/// `testrepo` (one commit on main) owned by the account the client key maps to.
struct Served {
    port: u16,
    client_key: PathBuf,
    org_slug: String,
}

async fn serve_with_repo(label: &str) -> Served {
    let storage_root = temp_storage_root();
    let database_url = fresh_database(&format!(
        "githearth_tssh_{label}_{}",
        Uuid::new_v4().simple()
    ))
    .await;
    let key_dir = temp_dir(&format!("ssh-key-{label}"));
    std::fs::create_dir_all(&key_dir).unwrap();
    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let client_key = key_dir.join("id_ed25519");
    ssh_keygen_generate(&client_key);
    let fp = sha256_fingerprint(&client_key);
    let config = Config {
        database_url,
        storage_root: storage_root.clone(),
        ssh_enabled: true,
        // The hooks must exec the real server binary, not this test harness.
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ssh_bind: "127.0.0.1:0".parse().unwrap(),
        ssh_host_key_path: host_key,
        git_timeout_seconds: 120,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let account_id = fixture_account(&pool, &format!("ssh{label}")).await;
    let (org_id, org_slug) = fixture_organization(&pool, account_id).await;
    let repo_id = create_active_repo(&pool, org_id, "testrepo", account_id).await;
    let repo_path = githearth_git::layout::Storage::open(&storage_root)
        .unwrap()
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();
    let work = temp_dir(&format!("ssh-{label}-w"));
    std::fs::create_dir_all(&work).unwrap();
    let w = work.to_string_lossy().to_string();
    git_ok(git_cmd(&["init", &w])).await;
    std::fs::write(work.join("README.md"), "# test\n").unwrap();
    git_ok(git_cmd(&["-C", &w, "add", "README.md"])).await;
    git_ok(git_cmd(&["-C", &w, "commit", "-m", "init"])).await;
    seed_bare(&repo_path.to_string_lossy(), &w).await;
    let state = AppState::new(pool, config).await.unwrap();
    let authenticate: ssh::Authenticate = Arc::new(move |f: String| {
        let found = (f == fp).then(|| ssh::SshIdentity {
            account: account_id,
            key_id: Uuid::new_v4(),
            read_only: false,
        });
        Box::pin(async move { Ok(found) })
    });
    let (_handle, port_rx) = ssh::spawn(state, authenticate);
    Served {
        port: port_rx.await.unwrap().port(),
        client_key,
        org_slug,
    }
}

/// Runs the system ssh client with `remote` as the command (None = shell).
async fn ssh_run(s: &Served, remote: Option<&str>) -> std::process::Output {
    ssh_run_with(s, &["-T"], remote).await
}

async fn ssh_run_with(s: &Served, flags: &[&str], remote: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new("ssh");
    cmd.args(flags)
        .arg("-i")
        .arg(&s.client_key)
        .args(["-p", &s.port.to_string()])
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
        ])
        .args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "git@127.0.0.1",
        ]);
    if let Some(remote) = remote {
        cmd.arg(remote);
    }
    cmd.env("DISPLAY", ":0");
    cmd.stdin(std::process::Stdio::null());
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_scp_form_with_protocol_v2_clones() {
    if !has_ssh() {
        return;
    }
    let s = serve_with_repo("v2").await;
    let dir = temp_dir("ssh-v2-clone");
    // scp-like form: ssh sends `git-upload-pack 'org/testrepo.git'` (no leading slash).
    let url = format!("git@127.0.0.1:{}/testrepo.git", s.org_slug);
    let mut cmd = git_cmd(&[
        "-c",
        "protocol.version=2",
        "clone",
        &url,
        &dir.to_string_lossy(),
    ]);
    cmd.env("GIT_SSH_COMMAND", git_ssh_key_env(&s.client_key, s.port));
    cmd.env("GIT_TRACE_PACKET", "1");
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("README.md").exists());
    // The server accepted GIT_PROTOCOL and spoke v2.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("version 2"),
        "protocol v2 not negotiated"
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_unknown_repo_and_unsupported_requests_look_not_found() {
    if !has_ssh() {
        return;
    }
    let s = serve_with_repo("nf").await;
    let org = s.org_slug.clone();
    let cases = [
        Some(format!("git-upload-pack '/{org}/missing.git'")),
        Some(format!("git-upload-archive '/{org}/testrepo.git'")),
        Some(format!("git-upload-pack '/{org}/testrepo'")),
        Some("ls -la".to_owned()),
        None, // shell
    ];
    for remote in cases {
        let out = ssh_run(&s, remote.as_deref()).await;
        assert_eq!(out.status.code(), Some(128), "{remote:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim_end(),
            "githearth: repository not found",
            "{remote:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_pty_and_subsystem_requests_are_refused() {
    if !has_ssh() {
        return;
    }
    let s = serve_with_repo("pty").await;
    // A forced PTY is refused, and the command is then still refused as not found.
    let out = ssh_run_with(&s, &["-tt"], Some("ls")).await;
    assert_ne!(out.status.code(), Some(0));
    // X11 forwarding is refused (ssh uses fake cookie data when xauth is absent).
    let out = ssh_run_with(&s, &["-T", "-X", "-o", "ForwardX11Trusted=no"], Some("ls")).await;
    assert_ne!(out.status.code(), Some(0));
    // A subsystem (sftp) is refused outright.
    let out = ssh_run_with(&s, &["-T", "-s"], Some("sftp")).await;
    assert_ne!(out.status.code(), Some(0));
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn ssh_listener_that_cannot_bind_ends_without_a_port() {
    let storage_root = temp_storage_root();
    let database_url =
        fresh_database(&format!("githearth_tssh_bind_{}", Uuid::new_v4().simple())).await;
    let host_key = storage_root.join("ssh_host_key");
    ssh::generate_host_key(&host_key).unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = Config {
        database_url,
        storage_root,
        ssh_enabled: true,
        ssh_bind: taken.local_addr().unwrap(),
        ssh_host_key_path: host_key,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = AppState::new(pool, config).await.unwrap();
    let authenticate: ssh::Authenticate = Arc::new(|_| Box::pin(async { Ok(None) }));
    let (handle, port_rx) = ssh::spawn(state, authenticate);
    handle.await.unwrap();
    assert!(
        port_rx.await.is_err(),
        "a port was reported although bind failed"
    );
}

#[test]
fn ssh_keygen_needs_no_database_and_never_overwrites() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("ssh-keygen-cli");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("host_key");
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_githearth-server"))
            .arg("ssh-keygen")
            // Well-formed but unreachable: ssh-keygen must not connect.
            .env(
                "GITHEARTH_DATABASE_URL",
                "postgres://nobody@127.0.0.1:1/none",
            )
            .env("GITHEARTH_SSH_HOST_KEY_PATH", &key)
            .env("GITHEARTH_SSH_BIND", "127.0.0.1:2299")
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    ssh::load_host_key(&key).unwrap();
    let second = run();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("refusing to overwrite"));
    let _ = std::fs::remove_dir_all(&dir);
}
