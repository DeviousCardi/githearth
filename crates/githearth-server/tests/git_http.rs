use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {cred}")
}

/// Build a sanitised git Command with stall-detection timeouts and the given
/// arguments appended.  The caller may add further arguments via `.arg()` /
/// `.args()` before passing the Command to the run helpers.
fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("GIT_SSL_NO_VERIFY", "1")
        .args(["-c", "http.lowSpeedLimit=1", "-c", "http.lowSpeedTime=30"])
        .args(args);
    cmd
}

/// Run a blocking git command on the blocking thread pool with a 60 s timeout.
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
        Err(_) => panic!("git command timed out after 60 s"),
    }
}

/// Run a blocking git command, assert success, return stdout as string.
async fn git_output(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Run a blocking git command, assert success.
async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a blocking git command, expect failure.
async fn git_fail(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(!out.status.success());
}

/// Seed a bare repository with the commit at a work tree's HEAD, bypassing
/// the receive-pack hooks (which now refuse updates that do not come through
/// the GitHearth server).
async fn seed_bare(repo_path: &str, work_dir: &str) {
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work_dir,
        "main:refs/heads/main",
    ]))
    .await;
}

// ---------------------------------------------------------------------------
// Database fixtures
// ---------------------------------------------------------------------------

async fn fixture_account(pool: &PgPool, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
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

async fn set_password(pool: &PgPool, account: Uuid, password: &str) {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2")
        .bind(hash)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
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

// ---------------------------------------------------------------------------
// Setup: database + bare repository on disk with one commit
// ---------------------------------------------------------------------------

async fn setup_with_repo_on_disk() -> (
    Router,
    PgPool,
    PathBuf,
    Uuid,
    String,
    String,
    String,
    Uuid,
    String,
) {
    let storage_root = std::env::temp_dir().join(format!("githearth-git-http-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();

    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        ..Config::default()
    };

    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool, &tag("owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;

    // Create the bare repository on disk via Storage::create.
    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    let repo_path = state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    // Put at least one commit into the bare repository.
    let tmp_work = std::env::temp_dir().join(format!("git-http-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();

    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;

    let test_file = tmp_work.join("README.md");
    std::fs::write(&test_file, "# Hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]))
    .await;

    // Seed the bare repo without receive-pack (its hooks would refuse this
    // direct update).
    seed_bare(repo_path.to_str().unwrap(), tw).await;

    // Record the commit hash.
    let head_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    let _ = std::fs::remove_dir_all(&tmp_work);

    let app = router(state);

    (
        app,
        pool,
        storage_root,
        owner,
        username,
        org_slug,
        repo_name,
        repo_id,
        head_commit,
    )
}

fn cleanup(storage_root: &Path) {
    let _ = std::fs::remove_dir_all(storage_root);
}

// ---------------------------------------------------------------------------
// Raw HTTP helper for non-JSON responses
// ---------------------------------------------------------------------------

async fn raw_request(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let resp_headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, resp_headers, bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Spin the router on a real TCP port for git CLI tests
// ---------------------------------------------------------------------------

async fn find_port_and_serve(app: &Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    port
}

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn clone_with_protocol_v2() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, head_commit) =
        setup_with_repo_on_disk().await;

    let clone_dir = std::env::temp_dir().join(format!("git-clone-v2-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let port = find_port_and_serve(&app).await;
    let url = format!("http://{username}:testpass@127.0.0.1:{port}/{org_slug}/{repo_name}.git");

    git_ok(git_cmd(&[
        "-c",
        "protocol.version=2",
        "clone",
        "--",
        &url,
        cd,
    ]))
    .await;

    let cloned_head = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    assert_eq!(cloned_head, head_commit);

    let readme = clone_dir.join("README.md");
    assert_eq!(std::fs::read_to_string(&readme).unwrap(), "# Hello\n");

    let _ = std::fs::remove_dir_all(&clone_dir);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn clone_with_protocol_v0() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, head_commit) =
        setup_with_repo_on_disk().await;

    let clone_dir = std::env::temp_dir().join(format!("git-clone-v0-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let port = find_port_and_serve(&app).await;
    let url = format!("http://{username}:testpass@127.0.0.1:{port}/{org_slug}/{repo_name}.git");

    git_ok(git_cmd(&[
        "-c",
        "protocol.version=0",
        "clone",
        "--",
        &url,
        cd,
    ]))
    .await;

    let cloned_head = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    assert_eq!(cloned_head, head_commit);

    let _ = std::fs::remove_dir_all(&clone_dir);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fetch_after_new_commit() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, repo_id, head_commit) =
        setup_with_repo_on_disk().await;

    // Clone first.
    let clone_dir = std::env::temp_dir().join(format!("git-fetch-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let port = find_port_and_serve(&app).await;
    let url = format!("http://{username}:testpass@127.0.0.1:{port}/{org_slug}/{repo_name}.git");

    git_ok(git_cmd(&["clone", "--", &url, cd])).await;

    // Add a new commit to the bare repo on disk.
    let tmp_work = std::env::temp_dir().join(format!("git-http-fetch-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    let rp = state_repo_path(&storage_root, repo_id);
    let rps = rp.to_str().unwrap();

    git_ok(git_cmd(&["-C", tw, "clone", "--", rps, "."])).await;

    std::fs::write(tmp_work.join("NEW.md"), "# new\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "NEW.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "second",
    ]))
    .await;
    let new_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    // Fetch into the clone.
    git_ok(git_cmd(&["-C", cd, "fetch", "origin"])).await;

    let fetched_head = git_output(git_cmd(&["-C", cd, "rev-parse", "origin/main"])).await;
    assert_eq!(fetched_head, new_head);
    assert_ne!(fetched_head, head_commit);

    let _ = std::fs::remove_dir_all(&clone_dir);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn wrong_password_git_fails_and_raw_401_with_www_authenticate() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let port = find_port_and_serve(&app).await;

    // git with wrong password
    let clone_dir = std::env::temp_dir().join(format!("git-wrong-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let url = format!("http://{username}:wrongpass@127.0.0.1:{port}/{org_slug}/{repo_name}.git");
    git_fail(git_cmd(&["clone", "--", &url, cd])).await;

    // Raw request to info/refs without auth → 401 with WWW-Authenticate.
    let (status, headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs?service=git-upload-pack"),
        &[],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.contains_key("www-authenticate"));

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn account_without_access_404() {
    let (app, pool, storage_root, _owner, _username, org_slug, repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    set_password(&pool, stranger, "strangerpass").await;
    let stranger_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(stranger)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (status, _headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs?service=git-upload-pack"),
        &[(
            "authorization",
            &basic_auth(&stranger_username, "strangerpass"),
        )],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archived_repository_still_cloneable() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, repo_id, head_commit) =
        setup_with_repo_on_disk().await;

    // Archive the repo.
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1")
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();

    let clone_dir = std::env::temp_dir().join(format!("git-clone-archived-{}", Uuid::new_v4()));
    let cd = clone_dir.to_str().unwrap();
    let port = find_port_and_serve(&app).await;
    let url = format!("http://{username}:testpass@127.0.0.1:{port}/{org_slug}/{repo_name}.git");

    git_ok(git_cmd(&["clone", "--", &url, cd])).await;

    let cloned_head = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    assert_eq!(cloned_head, head_commit);

    let _ = std::fs::remove_dir_all(&clone_dir);
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_active_repository_404() {
    let (app, pool, storage_root, _owner, username, org_slug, _repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();

    let dead_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,'dead-repo',$3,$4,'creating','private')")
        .bind(dead_id)
        .bind(org_id)
        .bind(_owner)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

    let (status, _headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/dead-repo.git/info/refs?service=git-upload-pack"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn active_row_without_directory_storage_error() {
    let (app, pool, storage_root, _owner, username, org_slug, _repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let ghost_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,'ghost-repo',$3,$4,'active','private')")
        .bind(ghost_id)
        .bind(org_id)
        .bind(_owner)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

    // Fetch and upload-pack resolve the (missing) directory and fail 500.
    let (status, _headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/ghost-repo.git/info/refs?service=git-upload-pack"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let (status, _headers, _body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/ghost-repo.git/git-upload-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-type", "application/x-git-upload-pack-request"),
        ],
        b"0000".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    // Receive-pack replays the (missing) journal first: a Journal error is a
    // 500, not a divergence.
    let (status, _headers, _body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/ghost-repo.git/git-receive-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-type", "application/x-git-receive-pack-request"),
        ],
        b"0000".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn missing_or_unknown_service_400() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    // No service query param.
    let (status, _headers, body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("dumb HTTP protocol is not supported"));

    // Unknown service.
    let (status, _headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs?service=git-upload-archive"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn path_without_dot_git_404() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let (status, _headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}/info/refs?service=git-upload-pack"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn gzip_encoded_post_body_accepted() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, head_commit) =
        setup_with_repo_on_disk().await;

    let want_line = format!("0032want {}\n", head_commit);
    let payload = format!("{want_line}00000009done\n");

    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();

    let (status, _headers, body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/{repo_name}.git/git-upload-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-encoding", "gzip"),
            ("content-type", "application/x-git-upload-pack-request"),
        ],
        compressed,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unsupported_content_encoding_415() {
    let (app, pool, storage_root, _owner, username, org_slug, repo_name, _repo_id, _head) =
        setup_with_repo_on_disk().await;

    let (status, _headers, _body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/{repo_name}.git/git-upload-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-encoding", "deflate"),
            ("content-type", "application/x-git-upload-pack-request"),
        ],
        b"fake body".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn request_over_limit_content_length_413() {
    let storage_root = std::env::temp_dir().join(format!("githearth-limit-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_request_limit_bytes: 1024,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("limit-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("limit-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-limit-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let app = router(state);

    // Content-Length exceeds limit → 413 before spawning git.
    let big_body = vec![b'x'; 2048];
    let (status, _headers, _body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/{repo_name}.git/git-upload-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-type", "application/x-git-upload-pack-request"),
            ("content-length", &format!("{}", big_body.len())),
        ],
        big_body,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn request_over_limit_streaming_kills_git() {
    let storage_root = std::env::temp_dir().join(format!("githearth-limit2-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_request_limit_bytes: 1024,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("limit2-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("limit2-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-limit2-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let app = router(state);

    // Send without Content-Length — streaming limit detection kills git.
    // Status is 200 (headers flushed before body stream processes), but the
    // body must be truncated because git was killed.
    let big_body = vec![b'x'; 2048];
    let (_status, _headers, body) = raw_request(
        &app,
        "POST",
        &format!("/{org_slug}/{repo_name}.git/git-upload-pack"),
        &[
            ("authorization", &basic_auth(&username, "testpass")),
            ("content-type", "application/x-git-upload-pack-request"),
        ],
        big_body,
    )
    .await;
    // Body must be empty or not a valid packfile (PACK signature).
    assert!(
        body.is_empty() || !body.starts_with(b"PACK"),
        "body should be truncated after git kill"
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn git_timeout_kills_child() {
    // Build an app with the minimum git timeout and concurrency 1 so we can
    // verify the permit is released after the deadline fires.
    let storage_root = std::env::temp_dir().join(format!("githearth-timeout-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_timeout_seconds: 10,
        git_concurrency: 1,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("timeout-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("timeout-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-timeout-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    // Read the real commit SHA from the bare repo.
    let rps = repo_path.to_str().unwrap();
    let real_sha = git_output(git_cmd(&["-C", rps, "rev-parse", "main"])).await;

    let app = router(state.clone());

    // Complete valid upload-pack request — git finishes quickly and the
    // pack sits in the pipe buffer.  The DeadlineStream enforces the deadline
    // on the response body.
    let body_bytes = format!(
        "{:04x}want {}\n0000{:04x}done\n",
        real_sha.len() + 10,
        real_sha,
        9
    );

    // Drop probe for the feeder.
    let feeder_dropped = Arc::new(AtomicBool::new(false));
    struct DropProbe(Arc<AtomicBool>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let probe = DropProbe(feeder_dropped.clone());
    let req_body = axum::body::Body::from_stream(futures_util::stream::unfold(
        (Some(body_bytes), probe),
        |(data, probe): (Option<String>, DropProbe)| async move {
            match data {
                Some(d) => Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(d)),
                    (None, probe),
                )),
                None => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        },
    ));

    let req = Request::builder()
        .method("POST")
        .uri(format!("/{org_slug}/{repo_name}.git/git-upload-pack"))
        .header("authorization", basic_auth(&username, "testpass"))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(req_body)
        .unwrap();

    let start = tokio::time::Instant::now();

    // Get the response but do NOT read the body.
    let response = app.clone().oneshot(req).await.expect("request failed");
    assert_eq!(response.status(), StatusCode::OK);

    // Sleep past the deadline.  The DeadlineStream will error on next poll.
    tokio::time::sleep(Duration::from_secs(11)).await;

    // Before touching the body (whose deadline would also cancel the transfer),
    // the supervisor's own deadline must already have killed git, stopped the
    // feeder and released the permit.
    for _ in 0..20 {
        if state.git_slots.available_permits() == 1 && feeder_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        state.git_slots.available_permits(),
        1,
        "supervisor deadline did not release the permit before the body was read"
    );
    assert!(
        feeder_dropped.load(Ordering::SeqCst),
        "supervisor deadline did not destroy the feeder before the body was read"
    );

    // Now try to read the body — must be empty or error (TimedOut).
    let body_result = response.into_body().collect().await;
    if let Ok(collected) = body_result {
        assert!(
            collected.to_bytes().is_empty(),
            "body must be empty after deadline"
        );
    }

    let elapsed = start.elapsed();

    // Permit must be released.
    assert_eq!(
        state.git_slots.available_permits(),
        1,
        "permit not released (elapsed={:.1}s)",
        elapsed.as_secs_f64()
    );
    assert!(
        elapsed >= Duration::from_secs(9),
        "elapsed too short ({:.1}s)",
        elapsed.as_secs_f64()
    );
    assert!(
        elapsed <= Duration::from_secs(20),
        "elapsed too long ({:.1}s)",
        elapsed.as_secs_f64()
    );

    // Feeder must be destroyed.
    assert!(
        feeder_dropped.load(Ordering::SeqCst),
        "feeder not destroyed"
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn slow_reader_gets_timeout() {
    let storage_root = std::env::temp_dir().join(format!("githearth-slow-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_timeout_seconds: 10,
        git_concurrency: 2,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("slow-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("slow-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-slow-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let real_sha = git_output(git_cmd(&["-C", rps, "rev-parse", "main"])).await;
    let app = router(state);

    // Complete valid upload-pack request — git will finish quickly.
    let body_bytes = format!(
        "{:04x}want {}\n0000{:04x}done\n",
        real_sha.len() + 10,
        real_sha,
        9
    );

    let req = Request::builder()
        .method("POST")
        .uri(format!("/{org_slug}/{repo_name}.git/git-upload-pack"))
        .header("authorization", basic_auth(&username, "testpass"))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(Body::from(body_bytes))
        .unwrap();

    let response = app.clone().oneshot(req).await.expect("request failed");
    assert_eq!(response.status(), StatusCode::OK);

    // Do NOT read the body.  Sleep past the deadline.
    tokio::time::sleep(Duration::from_secs(11)).await;

    // Now try to read the body — it must be empty or error (TimedOut).
    let body = response.into_body().collect().await;
    match body {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            assert!(
                bytes.is_empty(),
                "body must be empty after deadline (got {} bytes)",
                bytes.len()
            );
        }
        Err(_) => {
            // Error is also acceptable (connection closed).
        }
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn v0_info_refs_prefix_governed_by_deadline() {
    let storage_root = std::env::temp_dir().join(format!("githearth-v0dead-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_timeout_seconds: 10,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("v0dead-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("v0dead-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-v0dead-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let app = router(state);

    // v0 info/refs: do NOT set Git-Protocol header so we get the prefix.
    let (status, _headers, body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs?service=git-upload-pack"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Body should contain the service prefix immediately.
    assert!(String::from_utf8_lossy(&body).contains("# service=git-upload-pack"));

    // Now get a fresh response, don't read the body, sleep past the deadline,
    // then collect — the prefix must NOT appear.
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/{org_slug}/{repo_name}.git/info/refs?service=git-upload-pack"
        ))
        .header("authorization", basic_auth(&username, "testpass"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.expect("request failed");
    assert_eq!(response.status(), StatusCode::OK);

    // Sleep past the deadline.
    tokio::time::sleep(Duration::from_secs(11)).await;

    // Inspect the FIRST frame: `collect()` would discard a data frame that is
    // followed by an error, hiding a prefix sent outside the deadline.
    let mut body = response.into_body();
    match tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("first poll after the deadline hung")
    {
        None | Some(Err(_)) => {}
        Some(Ok(frame)) => {
            let data = frame.into_data().unwrap_or_default();
            panic!(
                "first frame after the deadline must be a timeout or end, got data: {}",
                String::from_utf8_lossy(&data)
            );
        }
    }

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn client_disconnect_releases_permit() {
    let storage_root = std::env::temp_dir().join(format!("githearth-drop-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_timeout_seconds: 600,
        git_concurrency: 1,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("drop-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("drop-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-drop-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let real_sha = git_output(git_cmd(&["-C", rps, "rev-parse", "main"])).await;
    let app = router(state.clone());

    // Never-ending request body with drop probe.
    let want_pkt = format!("{:04x}want {}\n", real_sha.len() + 10, real_sha);
    let feeder_dropped = Arc::new(AtomicBool::new(false));
    struct DropProbe2(Arc<AtomicBool>);
    impl Drop for DropProbe2 {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let probe = DropProbe2(feeder_dropped.clone());
    let hang_body = axum::body::Body::from_stream(futures_util::stream::unfold(
        (Some(want_pkt), probe),
        |(data, probe): (Option<String>, DropProbe2)| async move {
            match data {
                Some(d) => Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(d)),
                    (None, probe),
                )),
                None => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        },
    ));

    let req = Request::builder()
        .method("POST")
        .uri(format!("/{org_slug}/{repo_name}.git/git-upload-pack"))
        .header("authorization", basic_auth(&username, "testpass"))
        .header("content-type", "application/x-git-upload-pack-request")
        .body(hang_body)
        .unwrap();

    let response = app.clone().oneshot(req).await.expect("request failed");
    assert_eq!(response.status(), StatusCode::OK);

    // Drop the response immediately — must release permit and kill feeder.
    drop(response);

    // Permit must be released within 3 s.
    let permit_ok = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state.git_slots.available_permits() == 1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(permit_ok, "permit not released after client disconnect");

    // Feeder must be destroyed within 3 s.
    let feeder_ok = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if feeder_dropped.load(Ordering::SeqCst) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(feeder_ok, "feeder not destroyed after client disconnect");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn git_concurrency_exhausted_503_with_retry_after() {
    let storage_root = std::env::temp_dir().join(format!("githearth-conc-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        git_concurrency: 1,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("conc-owner")).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_name = tag("conc-repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let repo_path = state_repo_path(&storage_root, repo_id);
    let rps = repo_path.to_str().unwrap();
    let tmp_work = std::env::temp_dir().join(format!("git-conc-work-{}", Uuid::new_v4()));
    let tw = tmp_work.to_str().unwrap();
    std::fs::create_dir_all(&tmp_work).unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(tmp_work.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    seed_bare(rps, tw).await;
    let _ = std::fs::remove_dir_all(&tmp_work);

    let app = router(state.clone());

    // Hold the only git permit.
    let _held = state.git_slots.clone().try_acquire_owned().unwrap();

    let (status, headers, _body) = raw_request(
        &app,
        "GET",
        &format!("/{org_slug}/{repo_name}.git/info/refs?service=git-upload-pack"),
        &[("authorization", &basic_auth(&username, "testpass"))],
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("5")
    );

    drop(_held);
    cleanup(&storage_root);
    pool.close().await;
}
