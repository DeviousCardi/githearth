//! End-to-end push pipeline tests: real Git clients pushing to the in-process
//! server, plus journal replay into PostgreSQL (ADR 0003).
use axum::Router;
use githearth_git::journal::{Journal, Kind, Origin, Record, Update};
use githearth_git::layout::ObjectFormat;
use githearth_server::{AppState, MIGRATOR, config::Config, connect, pushes, reconcile, router};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// JSON API helpers (for token creation in push_credentials tests)
// ---------------------------------------------------------------------------

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

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

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn git_output(cmd: Command) -> String {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Seed a bare repo with the commit at a work tree's HEAD, bypassing hooks.
async fn seed_bare(repo_path: &str, work_dir: &str, refspec: &str) {
    git_ok(git_cmd(&[
        "--git-dir",
        repo_path,
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        work_dir,
        refspec,
    ]))
    .await;
}

/// Generate a complete `git-receive-pack` stateless-rpc request body: one
/// `report-status` command pkt-line, a flush, then the pack for the commit at
/// `work_dir`'s HEAD.
async fn receive_pack_request_body(fx: &Fixture, work_dir: &str, refspec: &str) -> Vec<u8> {
    use std::io::Write;

    let old = git_output(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "rev-parse",
        "refs/heads/main",
    ]))
    .await;
    let new = git_output(git_cmd(&["-C", work_dir, "rev-parse", "HEAD"])).await;

    let mut pack_cmd = git_cmd(&["-C", work_dir, "pack-objects", "--stdout", "--revs"]);
    pack_cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = pack_cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{new}\n").as_bytes())
        .unwrap();
    let pack = child.wait_with_output().unwrap().stdout;
    assert!(pack.starts_with(b"PACK"), "pack-objects produced a pack");

    let payload = format!("{old} {new} {refspec}\0report-status\n");
    let mut body = Vec::with_capacity(payload.len() + 8 + pack.len());
    body.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    body.extend_from_slice(payload.as_bytes());
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pack);
    body
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {cred}")
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

async fn set_password(pool: &PgPool, account: Uuid, password: &str) {
    let hash = githearth_server::hash_password(password).unwrap();
    sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2")
        .bind(hash)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
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

async fn member_with_repo_grant(
    pool: &PgPool,
    org_id: Uuid,
    repo_id: Uuid,
    role: &str,
) -> (Uuid, String) {
    let account = fixture_account(pool, &tag("user")).await;
    set_password(pool, account, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(account)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) VALUES ($1,$2,'account',$3,'repository',$4,$5,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(account)
        .bind(org_id)
        .bind(repo_id)
        .execute(pool)
        .await
        .unwrap();
    (account, username)
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    username: String,
    org_slug: String,
    repo_name: String,
    repo_id: Uuid,
    repo_path: PathBuf,
    base_commit: String,
    port: u16,
}

impl Fixture {
    fn url(&self) -> String {
        format!(
            "http://{}:testpass@127.0.0.1:{}/{}/{}.git",
            self.username, self.port, self.org_slug, self.repo_name
        )
    }

    fn repo_path_str(&self) -> &str {
        self.repo_path.to_str().unwrap()
    }
}

async fn setup(git_push_limit_bytes: usize) -> Fixture {
    setup_with_concurrency(git_push_limit_bytes, 8).await
}

async fn setup_with_concurrency(git_push_limit_bytes: usize, git_concurrency: usize) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-push-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        git_push_limit_bytes,
        git_concurrency,
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
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;
    let repo_name = tag("repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let repo_path = state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    // Seed one commit on main without running the hooks.
    let work = std::env::temp_dir().join(format!("githearth-push-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
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
    let base_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    seed_bare(repo_path.to_str().unwrap(), tw, "main:refs/heads/main").await;
    let _ = std::fs::remove_dir_all(&work);

    let app = router(state.clone());
    let port = find_port_and_serve(&app).await;

    Fixture {
        app,
        state,
        pool,
        storage_root,
        username,
        org_slug,
        repo_name,
        repo_id,
        repo_path,
        base_commit,
        port,
    }
}

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

fn cleanup(storage_root: &Path) {
    let _ = std::fs::remove_dir_all(storage_root);
}

fn journal_records(repo_path: &Path) -> Vec<Record> {
    Journal::open(repo_path)
        .unwrap()
        .records_after(0)
        .cloned()
        .collect()
}

/// The server applies a committed push after git exits (stage 7); wait for the
/// high-water mark to catch up before asserting on metadata.
async fn wait_for_journal_applied(pool: &PgPool, repo_id: Uuid, min: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
            .bind(repo_id)
            .fetch_one(pool)
            .await
            .unwrap();
        if ja >= min {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "journal_applied {ja} never reached {min}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Metadata / journal assertions
// ---------------------------------------------------------------------------

async fn assert_push_metadata(pool: &PgPool, repo_id: Uuid, expected_pushes: i64) {
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(repo_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(pc, expected_pushes, "repository_pushes count");
    let rc: i64 = sqlx::query_scalar("SELECT count(*) FROM ref_updates WHERE repository_id=$1")
        .bind(repo_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(rc >= expected_pushes, "ref_updates count {rc}");
    let ac: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE kind='repository.pushed'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(ac >= expected_pushes, "audit_events count {ac}");
    let oc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE kind='repository.pushed'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(oc >= expected_pushes, "outbox_events count {oc}");
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(ja > 0, "journal_applied must advance");
}

async fn assert_push_credentials(
    pool: &PgPool,
    repo_id: Uuid,
    expected_kind: &str,
    expected_id: Option<Uuid>,
) {
    let ops: Vec<(Uuid,)> =
        sqlx::query_as("SELECT operation_id FROM repository_pushes WHERE repository_id=$1")
            .bind(repo_id)
            .fetch_all(pool)
            .await
            .unwrap();
    let creds: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT pc.operation_id, pc.credential_kind, pc.credential_id \
         FROM push_credentials pc \
         JOIN repository_pushes rp ON rp.operation_id = pc.operation_id \
         WHERE rp.repository_id = $1",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await
    .unwrap();
    for (op_id,) in &ops {
        let found = creds.iter().find(|(o, _, _)| o == op_id);
        assert!(
            found.is_some(),
            "no push_credentials row for operation {op_id}"
        );
        let (_, kind, cred_id) = found.unwrap();
        assert_eq!(
            kind, expected_kind,
            "credential_kind for {op_id}: expected {expected_kind}, got {kind}"
        );
        assert_eq!(
            cred_id, &expected_id,
            "credential_id for {op_id}: expected {expected_id:?}, got {cred_id:?}"
        );
    }
    assert_eq!(
        creds.len(),
        ops.len(),
        "push_credentials rows ({}) != push operations ({})",
        creds.len(),
        ops.len()
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_new_branch_fast_forward_tag_and_clone_back() {
    let fx = setup(1 << 30).await;

    // Clone over HTTP.
    let clone = std::env::temp_dir().join(format!("git-push-clone-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // New branch.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "feature"])).await;
    std::fs::write(clone.join("FEATURE.md"), "# feature\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "FEATURE.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "feature",
    ]))
    .await;
    let feature_sha = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;

    // Fast-forward main.
    git_ok(git_cmd(&["-C", cd, "checkout", "main"])).await;
    std::fs::write(clone.join("MAIN.md"), "# main\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "MAIN.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "main",
    ]))
    .await;
    let main_sha = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    // Tag.
    git_ok(git_cmd(&["-C", cd, "tag", "v1"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "v1"])).await;

    // Clone back and compare.
    let clone2 = std::env::temp_dir().join(format!("git-push-clone2-{}", Uuid::new_v4()));
    let cd2 = clone2.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd2])).await;
    assert_eq!(
        git_output(git_cmd(&["-C", cd2, "rev-parse", "HEAD"])).await,
        main_sha
    );
    assert_eq!(
        git_output(git_cmd(&["-C", cd2, "rev-parse", "origin/feature"])).await,
        feature_sha
    );
    assert_eq!(
        git_output(git_cmd(&["-C", cd2, "rev-parse", "v1"])).await,
        main_sha
    );

    assert_push_metadata(&fx.pool, fx.repo_id, 3).await;
    // Password auth: every push records a password credential with null id.
    assert_push_credentials(&fx.pool, fx.repo_id, "password", None).await;

    // Journal has prepared+committed pairs.
    let records = journal_records(&fx.repo_path);
    let prepared = records.iter().filter(|r| r.kind == Kind::Prepared).count();
    let committed = records.iter().filter(|r| r.kind == Kind::Committed).count();
    assert!(prepared >= 3, "prepared records: {prepared}");
    assert_eq!(prepared, committed, "one marker per prepared record");

    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(&clone2);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn force_push_and_delete_allowed_for_owner() {
    let fx = setup(1 << 30).await;

    let clone = std::env::temp_dir().join(format!("git-push-ff-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Build a commit, then rewind and force-push it.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "wip"])).await;
    std::fs::write(clone.join("a"), "a").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "a"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "wip1",
    ]))
    .await;
    let wip1 = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "wip"])).await;

    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "HEAD~1"])).await;
    let wip2 = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    assert_ne!(wip1, wip2);
    git_ok(git_cmd(&["-C", cd, "push", "--force", "origin", "wip"])).await;

    // Delete the branch.
    git_ok(git_cmd(&["-C", cd, "push", "origin", "--delete", "wip"])).await;

    // Packed-refs deletion: journal the deletion exactly once, not the
    // zero-to-zero packed-refs half-transaction Git also reports.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "packed"])).await;
    std::fs::write(clone.join("p"), "p").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "p"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "p",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "packed"])).await;
    git_ok(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "-c",
        "core.hooksPath=/dev/null",
        "pack-refs",
        "--all",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "--delete", "packed"])).await;

    // Wait for the stage-7 metadata apply of the last push.
    wait_for_journal_applied(&fx.pool, fx.repo_id, 10).await;

    // Force push and delete were allowed; both journaled as committed.
    let records = journal_records(&fx.repo_path);
    let committed = records.iter().filter(|r| r.kind == Kind::Committed).count();
    assert!(
        committed >= 3,
        "force push + two deletes all committed; got {committed}"
    );
    let deletes: Vec<&Record> = records
        .iter()
        .filter(|r| {
            r.kind == Kind::Prepared && r.updates.iter().any(|u| u.new.bytes().all(|b| b == b'0'))
        })
        .collect();
    assert_eq!(deletes.len(), 2, "each deletion journaled exactly once");

    // Each deletion leaves one ref_updates row with an all-zero new oid.
    let del_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id=$1 AND new_oid ~ '^0+$'",
    )
    .bind(fx.repo_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(del_rows, 2, "two all-zero ref_updates rows");

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_push_refused_at_admission_before_pack() {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;

    let fx = setup(1 << 30).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let (_reader, username) = member_with_repo_grant(&fx.pool, org_id, fx.repo_id, "read").await;
    let auth = basic_auth(&username, "testpass");

    // Both the advertisement and the pack upload are refused before git runs.
    let advertise = Request::get(format!(
        "/{}/{}.git/info/refs?service=git-receive-pack",
        fx.org_slug, fx.repo_name
    ))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
    let response = fx.app.clone().oneshot(advertise).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    let _ = response.into_body().collect().await;

    let upload = Request::post(format!(
        "/{}/{}.git/git-receive-pack",
        fx.org_slug, fx.repo_name
    ))
    .header("authorization", &auth)
    .header("content-type", "application/x-git-receive-pack-request")
    .body(Body::from(vec![b'x'; 2048]))
    .unwrap();
    let response = fx.app.clone().oneshot(upload).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    let _ = response.into_body().collect().await;

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 0);

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn read_only_user_push_denied_and_ref_unchanged() {
    let fx = setup(1 << 30).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let (_reader, username) = member_with_repo_grant(&fx.pool, org_id, fx.repo_id, "read").await;

    let clone = std::env::temp_dir().join(format!("git-push-ro-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    let url = format!(
        "http://{username}:testpass@127.0.0.1:{}/{}/{}.git",
        fx.port, fx.org_slug, fx.repo_name
    );
    git_ok(git_cmd(&["clone", "--", &url, cd])).await;
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "feature"])).await;
    std::fs::write(clone.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;

    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;
    assert!(!out.status.success(), "read-only push must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Admission refuses the receive-pack advertisement before any pack is
    // uploaded, so the transport reports 403 rather than a hook rejection.
    assert!(
        stderr.contains("403"),
        "read-only push should be refused at admission: {stderr}"
    );

    // Nothing journaled as committed.
    assert!(journal_records(&fx.repo_path).is_empty());
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 0);

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn writer_can_push_force_push_and_delete_per_policy() {
    // The authz engine puts ref.push, ref.force_push and ref.delete all in the
    // "write" group; Basic push counts as fresh authentication, so a plain
    // write grant admits force pushes and deletions too.
    let fx = setup(1 << 30).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let (_writer, username) = member_with_repo_grant(&fx.pool, org_id, fx.repo_id, "write").await;

    let clone = std::env::temp_dir().join(format!("git-push-writer-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    let url = format!(
        "http://{username}:testpass@127.0.0.1:{}/{}/{}.git",
        fx.port, fx.org_slug, fx.repo_name
    );
    git_ok(git_cmd(&["clone", "--", &url, cd])).await;
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "wip"])).await;
    std::fs::write(clone.join("a"), "a").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "a"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "wip1",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "wip"])).await;
    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "HEAD~1"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "--force", "origin", "wip"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "--delete", "wip"])).await;

    let records = journal_records(&fx.repo_path);
    let committed = records.iter().filter(|r| r.kind == Kind::Committed).count();
    assert!(
        committed >= 3,
        "push + force push + delete committed; got {committed}"
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn receive_pack_concurrency_exhausted_503() {
    let fx = setup_with_concurrency(1 << 30, 1).await;

    use axum::{body::Body, http::Request};
    let _held = fx.state.git_slots.clone().try_acquire_owned().unwrap();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!(
                "/{}/{}.git/git-receive-pack",
                fx.org_slug, fx.repo_name
            ))
            .header("authorization", basic_auth(&fx.username, "testpass"))
            .header("content-type", "application/x-git-receive-pack-request")
            .body(Body::from(Vec::<u8>::new()))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn chunked_and_gzip_push_over_limit_413() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    let fx = setup(1024).await;
    let path = format!("/{}/{}.git/git-receive-pack", fx.org_slug, fx.repo_name);

    // No Content-Length (chunked): the size is only known while streaming, but
    // the push must still be refused with a clean 413 (issue #142).
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("authorization", basic_auth(&fx.username, "testpass"))
                .header("content-type", "application/x-git-receive-pack-request")
                .body(Body::from(vec![b'x'; 2048]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    let _ = response.into_body().collect().await;

    // Gzip: the decompressed body exceeds the limit even though the compressed
    // body is small, so the Content-Length pre-check cannot catch it.
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![b'x'; 2048]).unwrap();
    let compressed = encoder.finish().unwrap();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("authorization", basic_auth(&fx.username, "testpass"))
                .header("content-type", "application/x-git-receive-pack-request")
                .header("content-encoding", "gzip")
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    let _ = response.into_body().collect().await;

    // Neither over-limit request applied a push.
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id = $1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 0);

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn no_credentials_401_and_unrelated_user_404() {
    let fx = setup(1 << 30).await;

    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;

    // No credentials: 401 with WWW-Authenticate on the advertise.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::get(format!(
                "/{}/{}.git/info/refs?service=git-receive-pack",
                fx.org_slug, fx.repo_name
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));

    // Unrelated user: 404.
    let stranger = fixture_account(&fx.pool, &tag("stranger")).await;
    set_password(&fx.pool, stranger, "strangerpass").await;
    let stranger_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(stranger)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::get(format!(
                "/{}/{}.git/info/refs?service=git-receive-pack",
                fx.org_slug, fx.repo_name
            ))
            .header(
                "authorization",
                basic_auth(&stranger_username, "strangerpass"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    let _ = response.into_body().collect().await;

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archived_repository_push_denied() {
    let fx = setup(1 << 30).await;
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1")
        .bind(fx.repo_id)
        .execute(&fx.pool)
        .await
        .unwrap();

    let clone = std::env::temp_dir().join(format!("git-push-arch-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "feature"])).await;
    std::fs::write(clone.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("permission denied: ref.push"));
    assert!(journal_records(&fx.repo_path).is_empty());

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn foreign_ref_namespace_denied() {
    let fx = setup(1 << 30).await;

    let clone = std::env::temp_dir().join(format!("git-push-foreign-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    std::fs::write(clone.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    let sha = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;

    let out = git_run(git_cmd(&[
        "-C",
        cd,
        "push",
        "origin",
        &format!("{sha}:refs/notes/commits"),
    ]))
    .await;
    assert!(!out.status.success(), "foreign ref push must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("read-only") || stderr.contains("ref namespace"),
        "reason: {stderr}"
    );
    assert!(journal_records(&fx.repo_path).is_empty());

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn atomic_and_non_atomic_multi_ref_one_record_per_transaction() {
    let fx = setup(1 << 30).await;

    let clone = std::env::temp_dir().join(format!("git-push-atomic-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "b1"])).await;
    std::fs::write(clone.join("x"), "1").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "x"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "b1",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "checkout", "main"])).await;
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "b2"])).await;
    std::fs::write(clone.join("y"), "2").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "y"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "b2",
    ]))
    .await;

    // Non-atomic: two transactions, two prepared records (one update each).
    git_ok(git_cmd(&["-C", cd, "push", "origin", "b1", "b2"])).await;
    let non_atomic = journal_records(&fx.repo_path);
    let prepared_two: Vec<&Record> = non_atomic
        .iter()
        .filter(|r| r.kind == Kind::Prepared)
        .collect();
    assert_eq!(prepared_two.len(), 2, "non-atomic: one prepared per ref");
    assert!(prepared_two.iter().all(|r| r.updates.len() == 1));

    // Atomic: one transaction holding both refs (new commits force a real update).
    git_ok(git_cmd(&["-C", cd, "checkout", "b1"])).await;
    std::fs::write(clone.join("x"), "1b").unwrap();
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "b1b",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "checkout", "b2"])).await;
    std::fs::write(clone.join("y"), "2b").unwrap();
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "b2b",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C", cd, "push", "--atomic", "origin", "b1", "b2",
    ]))
    .await;
    let atomic = journal_records(&fx.repo_path);
    let prepared_after: Vec<&Record> = atomic.iter().filter(|r| r.kind == Kind::Prepared).collect();
    assert_eq!(prepared_after.len(), 3, "atomic added one prepared record");
    let atomic_record = prepared_after.last().unwrap();
    assert_eq!(
        atomic_record.updates.len(),
        2,
        "atomic transaction holds both refs"
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Build a child commit on top of the bare repo's `main` and fetch it in.
async fn make_child(fx: &Fixture) -> (String, String) {
    let work = std::env::temp_dir().join(format!("git-push-child-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("child"), "child").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "child"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "child",
    ]))
    .await;
    let child = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    seed_bare(fx.repo_path_str(), tw, "main:refs/heads/main").await;
    let _ = std::fs::remove_dir_all(&work);
    (fx.base_commit.clone(), child)
}

async fn append_prepared(fx: &Fixture, actor: Uuid, old: &str, new: &str) -> u64 {
    let mut journal = Journal::open(&fx.repo_path).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            Some(actor),
            vec![Update {
                old: old.to_owned(),
                new: new.to_owned(),
                reference: "refs/heads/main".into(),
            }],
            None,
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_prepared_only_refs_at_new_applies_and_appends_marker() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    let (old, new) = make_child(&fx).await; // refs now at new
    append_prepared(&fx, owner_id, &old, &new).await;

    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(summary.committed, 1);

    let records = journal_records(&fx.repo_path);
    assert!(records.iter().any(|r| r.kind == Kind::Committed));

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 1);

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_prepared_only_refs_at_old_appends_aborted_marker() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    // Build a child commit but leave the ref at the old value.
    let work = std::env::temp_dir().join(format!("git-push-old-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("child"), "child").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "child"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "child",
    ]))
    .await;
    let new = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    // Fetch the object but keep the ref at old.
    git_ok(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        tw,
        "main:refs/remotes/tmp/child",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);

    append_prepared(&fx, owner_id, &fx.base_commit, &new).await;
    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(summary.aborted, 1);

    let records = journal_records(&fx.repo_path);
    assert!(records.iter().any(|r| r.kind == Kind::Aborted));
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 0, "aborted push must not create metadata");

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_marker_lost_does_not_skip_later_records() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    // Two branches on top of base: main_child and feature_child.
    let work = std::env::temp_dir().join(format!("git-push-marker-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("m"), "m").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "m"])).await;
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
    let main_child = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-b", "feature"])).await;
    std::fs::write(work.join("f"), "f").unwrap();
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
        "f",
    ]))
    .await;
    let feature_child = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    seed_bare(fx.repo_path_str(), tw, "main:refs/heads/main").await;
    seed_bare(fx.repo_path_str(), tw, "feature:refs/heads/feature").await;
    let _ = std::fs::remove_dir_all(&work);

    // prepared A (marker lost), then prepared B + committed B.
    let mut journal = Journal::open(&fx.repo_path).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            Some(owner_id),
            vec![Update {
                old: fx.base_commit.clone(),
                new: main_child,
                reference: "refs/heads/main".into(),
            }],
            None,
        )
        .unwrap();
    let op_b = Uuid::new_v4();
    journal
        .append_prepared(
            op_b,
            Some(owner_id),
            vec![Update {
                old: fx.base_commit.clone(),
                new: feature_child,
                reference: "refs/heads/feature".into(),
            }],
            None,
        )
        .unwrap();
    journal.append_marker(op_b, Kind::Committed).unwrap();

    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(summary.committed, 2, "both records applied");

    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(
        ja, 3,
        "advance stops at the applied records, not the appended marker"
    );

    let rc: i64 = sqlx::query_scalar("SELECT count(*) FROM ref_updates WHERE repository_id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(rc, 2);

    let second = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!((second.committed, second.aborted), (0, 0));
    let ja2: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(ja2, 4, "the appended marker is skipped on the next pass");

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_diverged_then_next_push_409() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    // Mixed: main moved to a child commit, but `feature` is absent — the
    // prepared record claims both, so classification diverges.
    let work = std::env::temp_dir().join(format!("git-push-mixed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("x"), "x").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "x"])).await;
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
    let main_new = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    // Move main to the child; leave `feature` untouched (it does not exist).
    seed_bare(fx.repo_path_str(), tw, "main:refs/heads/main").await;
    let _ = std::fs::remove_dir_all(&work);

    let mut journal = Journal::open(&fx.repo_path).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            Some(owner_id),
            vec![
                Update {
                    old: fx.base_commit.clone(),
                    new: main_new.clone(),
                    reference: "refs/heads/main".into(),
                },
                Update {
                    old: fx.base_commit.clone(),
                    new: main_new,
                    reference: "refs/heads/feature".into(),
                },
            ],
            None,
        )
        .unwrap();

    let err = pushes::replay(&fx.state, fx.repo_id).await.unwrap_err();
    assert!(matches!(err, pushes::ReplayError::Diverged { .. }));

    // The reconciler logs the divergence and keeps going.
    let mut attempts = std::collections::HashMap::new();
    reconcile::reconcile_once(&fx.state, &mut attempts)
        .await
        .unwrap();

    // The next push is refused with 409.
    let clone = std::env::temp_dir().join(format!("git-push-div-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    std::fs::write(clone.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("409") || stderr.contains("needs repair"),
        "{stderr}"
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_twice_is_a_noop() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    let (old, new) = make_child(&fx).await;
    append_prepared(&fx, owner_id, &old, &new).await;

    let first = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(first.committed, 1);
    let second = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!((second.committed, second.aborted), (0, 0));

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 1);

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_prepared_plus_aborted_marker_skips() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    // Build a child commit but leave the ref at the old value.
    let work = std::env::temp_dir().join(format!("git-push-ab-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("child"), "child").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "child"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "child",
    ]))
    .await;
    let new = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        tw,
        "main:refs/remotes/tmp/child",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);

    // prepared + aborted marker: classify as Aborted, skip, no metadata.
    let op = Uuid::new_v4();
    let mut journal = Journal::open(&fx.repo_path).unwrap();
    journal
        .append_prepared(
            op,
            Some(owner_id),
            vec![Update {
                old: fx.base_commit.clone(),
                new,
                reference: "refs/heads/main".into(),
            }],
            None,
        )
        .unwrap();
    journal.append_marker(op, Kind::Aborted).unwrap();

    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!((summary.committed, summary.aborted), (0, 1));

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 0, "aborted push creates no metadata");
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(ja, 2, "advanced past prepared and its aborted marker");

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reconciler_replays_pending_and_skips_missing_journal() {
    let fx = setup(1 << 30).await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let (old, new) = make_child(&fx).await;
    append_prepared(&fx, owner_id, &old, &new).await;

    // An active row with no on-disk directory: its journal open fails and the
    // pass must keep going.
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    create_active_repo(&fx.pool, org_id, &tag("no-dir"), owner_id).await;

    let mut attempts = std::collections::HashMap::new();
    reconcile::reconcile_once(&fx.state, &mut attempts)
        .await
        .unwrap();

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 1, "reconciler applied the pending push");

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn oversized_push_rejected() {
    // A tiny push limit: the git pack exceeds it before receive-pack runs.
    let fx = setup(1024).await;

    let clone = std::env::temp_dir().join(format!("git-push-big-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    // Incompressible bytes: zlib would shrink repetitive data below the limit.
    let mut data = Vec::with_capacity(64 * 1024);
    let mut x = 0x9e3779b9u32;
    while data.len() < 64 * 1024 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        data.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(clone.join("big"), data).unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "big"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "big",
    ]))
    .await;

    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(!out.status.success(), "oversized push must fail");
    assert!(journal_records(&fx.repo_path).is_empty());

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_ref_locks_cleaned_before_push() {
    let fx = setup(1 << 30).await;

    // A receive-pack killed mid-update leaves these behind.
    let nested_lock = fx.repo_path.join("refs").join("heads").join("main.lock");
    std::fs::write(&nested_lock, b"").unwrap();
    let packed_lock = fx.repo_path.join("packed-refs.lock");
    std::fs::write(&packed_lock, b"").unwrap();

    let clone = std::env::temp_dir().join(format!("git-push-lock-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    std::fs::write(clone.join("f"), "x").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m",
    ]))
    .await;
    let new_sha = git_output(git_cmd(&["-C", cd, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    assert!(!nested_lock.exists(), "stale ref lock removed before push");
    assert!(
        !packed_lock.exists(),
        "stale packed-refs lock removed before push"
    );
    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        new_sha,
        "push landed"
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn receive_pack_answers_only_after_the_whole_pack() {
    // A half-duplex proxy (nginx with proxy_request_buffering off) stops sending
    // the request body once response headers arrive, so receive-pack must not
    // answer before it has read the pack. A raw HTTP/1.1 client over TCP sees
    // exactly what such a proxy sees.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fx = setup(1 << 30).await;
    let work = std::env::temp_dir().join(format!("git-push-half-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    // Incompressible and larger than socket and pipe buffers, like a real push.
    let mut big = vec![0u8; 8 << 20];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").unwrap(), &mut big).unwrap();
    std::fs::write(work.join("f"), big).unwrap();
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
    let new_sha = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    let request = receive_pack_request_body(&fx, tw, "refs/heads/main").await;
    let (first, rest) = request.split_at(request.len() / 2);
    let chunk = |data: &[u8]| {
        let mut c = format!("{:x}\r\n", data.len()).into_bytes();
        c.extend_from_slice(data);
        c.extend_from_slice(b"\r\n");
        c
    };

    let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", fx.port))
        .await
        .unwrap();
    let head = format!(
        "POST /{}/{}.git/git-receive-pack HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Authorization: {}\r\nContent-Type: application/x-git-receive-pack-request\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        fx.org_slug,
        fx.repo_name,
        basic_auth(&fx.username, "testpass")
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    conn.write_all(&chunk(first)).await.unwrap();

    let mut buf = [0u8; 1];
    let early = tokio::time::timeout(Duration::from_millis(500), conn.read(&mut buf)).await;
    assert!(
        early.is_err(),
        "the server answered before the pack was fully received"
    );

    conn.write_all(&chunk(rest)).await.unwrap();
    conn.write_all(b"0\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), conn.read_to_end(&mut response))
        .await
        .expect("no response after the full pack")
        .unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 200"), "response: {text}");
    assert!(text.contains("unpack ok"), "response: {text}");
    wait_for_journal_applied(&fx.pool, fx.repo_id, 1).await;
    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        new_sha
    );

    let _ = std::fs::remove_dir_all(&work);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disconnect_after_full_pack_applies_and_cleans_token() {
    use axum::{body::Body, http::Request};

    let fx = setup(1 << 30).await;

    // Build a child commit in a work tree.
    let work = std::env::temp_dir().join(format!("git-push-disc-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("f"), "x").unwrap();
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
    let new_sha = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    let request = receive_pack_request_body(&fx, tw, "refs/heads/main").await;
    assert!(!request.is_empty());

    // Send the complete pack, then drop the response without reading it —
    // the client has gone away mid-push.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!(
                "/{}/{}.git/git-receive-pack",
                fx.org_slug, fx.repo_name
            ))
            .header("authorization", basic_auth(&fx.username, "testpass"))
            .header("content-type", "application/x-git-receive-pack-request")
            .body(Body::from(request))
            .unwrap(),
        )
        .await
        .unwrap();
    drop(response);

    // Git ran to completion and the push was applied, without the reconciler.
    wait_for_journal_applied(&fx.pool, fx.repo_id, 1).await;
    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        new_sha,
        "push committed even though the client disconnected"
    );
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 1, "post-push replay applied without the reconciler");

    // The one-use hook token is gone.
    let tokens_empty = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fx.state.hook_tokens.lock().unwrap().is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(tokens_empty, "hook token leaked after disconnect");

    // A later push to the same ref still succeeds (no stale locks).
    let clone = std::env::temp_dir().join(format!("git-push-disc2-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    std::fs::write(clone.join("g"), "y").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "g"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "m2",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

// ---------------------------------------------------------------------------
// Branch protection (M3): protected refs refuse force-push, delete and direct
// pushes unless the rule allows them.
// ---------------------------------------------------------------------------

async fn add_protection_rule(
    pool: &PgPool,
    repo_id: Uuid,
    created_by: Uuid,
    pattern: &str,
    allow_force_push: bool,
    allow_deletion: bool,
    require_pull_request: bool,
) {
    sqlx::query(
        "INSERT INTO branch_protection_rules \
         (id, repository_id, pattern, allow_force_push, allow_deletion, require_pull_request, required_approvals, required_status_checks, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, 0, '{}', $7)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(pattern)
    .bind(allow_force_push)
    .bind(allow_deletion)
    .bind(require_pull_request)
    .bind(created_by)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO protection_generations (repository_id, generation) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET generation = protection_generations.generation + 1",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn add_signed_commits_rule(pool: &PgPool, repo_id: Uuid, created_by: Uuid, pattern: &str) {
    sqlx::query(
        "INSERT INTO branch_protection_rules \
         (id, repository_id, pattern, allow_force_push, allow_deletion, require_pull_request, required_approvals, required_status_checks, require_signed_commits, created_by) \
         VALUES ($1, $2, $3, false, false, false, 0, '{}', true, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(pattern)
    .bind(created_by)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO protection_generations (repository_id, generation) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET generation = protection_generations.generation + 1",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn add_tag_protection_rule(
    pool: &PgPool,
    repo_id: Uuid,
    created_by: Uuid,
    pattern: &str,
    allowed_to_create: &str,
    allow_update: bool,
    allow_delete: bool,
) {
    sqlx::query(
        "INSERT INTO tag_protection_rules \
         (id, repository_id, pattern, allowed_to_create, allow_update, allow_delete, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(repo_id)
    .bind(pattern)
    .bind(allowed_to_create)
    .bind(allow_update)
    .bind(allow_delete)
    .bind(created_by)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO protection_generations (repository_id, generation) VALUES ($1, 1) \
         ON CONFLICT (repository_id) DO UPDATE SET generation = protection_generations.generation + 1",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Generates an ed25519 key and returns `(private_key_path, public_key_line)`.
fn generate_ssh_key() -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("githearth-signed-key-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("id_ed25519");
    let output = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &path.to_string_lossy(),
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
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    (path, public)
}

async fn register_ssh_key(pool: &PgPool, account_id: Uuid, public_key: &str) {
    // Production stores the key with its trailing comment stripped (ssh_keys.rs
    // sets the comment to ""), so strip it here to match the real stored format.
    let canonical = public_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    sqlx::query(
        "INSERT INTO ssh_keys (id, account_id, name, algorithm, public_key, fingerprint) \
         VALUES ($1, $2, 'test', 'ed25519', $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(canonical)
    .bind(format!("SHA256:{}", Uuid::new_v4().simple()))
    .execute(pool)
    .await
    .unwrap();
}

async fn owner_id(fx: &Fixture) -> Uuid {
    sqlx::query_scalar("SELECT created_by FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn protected_main_refuses_force_push_and_delete() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_protection_rule(&fx.pool, fx.repo_id, owner, "main", false, false, false).await;

    let clone = std::env::temp_dir().join(format!("git-prot-main-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Fast-forward push to main is still allowed (no require_pull_request).
    std::fs::write(clone.join("a"), "a").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "a"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "a",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    // Rewind and force-push: refused.
    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "HEAD~1"])).await;
    let out = git_run(git_cmd(&["-C", cd, "push", "--force", "origin", "main"])).await;
    assert!(
        !out.status.success(),
        "force push to protected main must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected branch: ref.force_push"),
        "force push reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Delete main: refused.
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "--delete", "main"])).await;
    assert!(!out.status.success(), "delete of protected main must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected branch: ref.delete"),
        "delete reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn release_glob_applies_and_unprotected_branches_unchanged() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_protection_rule(
        &fx.pool,
        fx.repo_id,
        owner,
        "release/*",
        false,
        false,
        false,
    )
    .await;

    let clone = std::env::temp_dir().join(format!("git-prot-glob-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // A release/* branch refuses a force push.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "release/v1"])).await;
    std::fs::write(clone.join("r"), "r").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "r"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "r",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "release/v1"])).await;
    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "HEAD~1"])).await;
    let out = git_run(git_cmd(&[
        "-C",
        cd,
        "push",
        "--force",
        "origin",
        "release/v1",
    ]))
    .await;
    assert!(!out.status.success(), "force push to release/* must fail");

    // An ordinary branch is unaffected: force push and delete both work.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "feature"])).await;
    std::fs::write(clone.join("f"), "f").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "f"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "f",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "feature"])).await;
    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "HEAD~1"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "--force", "origin", "feature"])).await;
    git_ok(git_cmd(&[
        "-C", cd, "push", "origin", "--delete", "feature",
    ]))
    .await;

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn protected_tag_create_move_delete_refused_over_http() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    let org_id: Uuid = sqlx::query_scalar("SELECT organization_id FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    // `v*` protected, creation restricted to admins, move/delete denied.
    add_tag_protection_rule(&fx.pool, fx.repo_id, owner, "v*", "admin", false, false).await;

    // A write-role member may not create a matching tag.
    let (_writer, wuser) = member_with_repo_grant(&fx.pool, org_id, fx.repo_id, "write").await;
    let wurl = format!(
        "http://{wuser}:testpass@127.0.0.1:{}/{}/{}.git",
        fx.port, fx.org_slug, fx.repo_name
    );
    let wclone = std::env::temp_dir().join(format!("git-tag-prot-w-{}", Uuid::new_v4()));
    let wcd = wclone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &wurl, wcd])).await;
    git_ok(git_cmd(&["-C", wcd, "tag", "v2.0.0"])).await;
    let out = git_run(git_cmd(&["-C", wcd, "push", "origin", "v2.0.0"])).await;
    assert!(
        !out.status.success(),
        "write role must not create a protected tag"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected tag: ref.create"),
        "create reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The owner (admin) creates v1.0.0, then move and delete are refused.
    let clone = std::env::temp_dir().join(format!("git-tag-prot-o-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    git_ok(git_cmd(&["-C", cd, "tag", "v1.0.0"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "v1.0.0"])).await;

    // Move: point the tag at a new commit and force push.
    std::fs::write(clone.join("move"), "move").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "move"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "move",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "tag", "-f", "v1.0.0"])).await;
    let out = git_run(git_cmd(&["-C", cd, "push", "--force", "origin", "v1.0.0"])).await;
    assert!(!out.status.success(), "moving a protected tag must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected tag: ref.update"),
        "move reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Delete.
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "--delete", "v1.0.0"])).await;
    assert!(!out.status.success(), "deleting a protected tag must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected tag: ref.delete"),
        "delete reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An unprotected tag is unaffected: create, move and delete all succeed.
    git_ok(git_cmd(&["-C", cd, "tag", "other"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "other"])).await;
    git_ok(git_cmd(&["-C", cd, "tag", "-f", "other"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "--force", "origin", "other"])).await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "--delete", "other"])).await;

    let _ = std::fs::remove_dir_all(&wclone);
    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_a_require_pull_request_branch_succeeds_but_the_next_push_is_refused() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_protection_rule(&fx.pool, fx.repo_id, owner, "release/*", false, false, true).await;

    let clone = std::env::temp_dir().join(format!("git-prot-create-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Creating release/2.0 is allowed even though release/* requires a PR:
    // the branch does not exist yet, so the rule only applies from then on.
    git_ok(git_cmd(&["-C", cd, "checkout", "-b", "release/2.0"])).await;
    std::fs::write(clone.join("r"), "r").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "r"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "r",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "release/2.0"])).await;

    // A second fast-forward push to the now-existing branch is refused.
    std::fs::write(clone.join("r2"), "r2").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "r2"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "r2",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "release/2.0"])).await;
    assert!(
        !out.status.success(),
        "fast-forward push to protected branch must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected branch: ref.push"),
        "push reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn require_pull_request_refuses_direct_push() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_protection_rule(&fx.pool, fx.repo_id, owner, "main", false, false, true).await;

    let clone = std::env::temp_dir().join(format!("git-prot-pr-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    std::fs::write(clone.join("a"), "a").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "a"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "a",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(
        !out.status.success(),
        "direct push to require_pull_request main must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("protected branch: ref.push"),
        "push reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn require_signed_commits_refuses_unsigned_and_accepts_signed_push() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_signed_commits_rule(&fx.pool, fx.repo_id, owner, "main").await;

    // Register an SSH key to the owner, then generate the committer email the
    // server maps to the account.
    let (key_path, public_key) = generate_ssh_key();
    register_ssh_key(&fx.pool, owner, &public_key).await;
    let committer_email = format!("{}@users.noreply.githearth.invalid", fx.username);

    // An unsigned push to a require-signed-commits branch is refused.
    let clone = std::env::temp_dir().join(format!("git-signed-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;
    std::fs::write(clone.join("u"), "u").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "u"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        &format!("user.email={committer_email}"),
        "commit",
        "-m",
        "unsigned",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(
        !out.status.success(),
        "unsigned push to a require-signed-commits branch must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not signed by a registered committer"),
        "unsigned push reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Rewind and sign a commit with the registered key: accepted.
    git_ok(git_cmd(&["-C", cd, "reset", "--hard", "origin/main"])).await;
    std::fs::write(clone.join("s"), "s").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "s"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        &format!("user.email={committer_email}"),
        "-c",
        "gpg.format=ssh",
        "-c",
        &format!("user.signingkey={}", key_path.display()),
        "commit",
        "-S",
        "-m",
        "signed",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(key_path.parent().unwrap());
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn require_signed_commits_refuses_commit_signed_by_another_committers_key() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;
    add_signed_commits_rule(&fx.pool, fx.repo_id, owner, "main").await;

    // The pusher (owner) registers a key; a second account registers a
    // different key. The pusher then forges a commit that names the second
    // account as committer but is signed with the pusher's own key.
    let (pusher_key, pusher_public) = generate_ssh_key();
    register_ssh_key(&fx.pool, owner, &pusher_public).await;
    let victim = fixture_account(&fx.pool, &tag("user")).await;
    let (victim_key, victim_public) = generate_ssh_key();
    register_ssh_key(&fx.pool, victim, &victim_public).await;

    let pusher_email = format!("{}@users.noreply.githearth.invalid", fx.username);
    let victim_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(victim)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    let victim_email = format!("{victim_username}@users.noreply.githearth.invalid");

    let clone = std::env::temp_dir().join(format!("git-signed-forge-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Commit 1: the pusher's own signed commit, so the pusher's key legitimately
    // belongs in this push's set of keys.
    std::fs::write(clone.join("a"), "a").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "a"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        &format!("user.email={pusher_email}"),
        "-c",
        "gpg.format=ssh",
        "-c",
        &format!("user.signingkey={}", pusher_key.display()),
        "commit",
        "-S",
        "-m",
        "pusher",
    ]))
    .await;

    // Commit 2: names the victim as committer but is signed with the pusher's
    // key. A shared allowed-signers file would accept it because the pusher's
    // key is present for commit 1.
    std::fs::write(clone.join("b"), "b").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "b"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        &format!("user.email={victim_email}"),
        "-c",
        "gpg.format=ssh",
        "-c",
        &format!("user.signingkey={}", pusher_key.display()),
        "commit",
        "-S",
        "-m",
        "forged",
    ]))
    .await;

    // Both commits are pushed together; the forged one must be refused even
    // though the pusher's own signed commit is in the same push.
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(
        !out.status.success(),
        "a commit naming another account as committer but signed with the pusher's key must be refused"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not signed by a registered committer"),
        "forged signed push reason: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(pusher_key.parent().unwrap());
    let _ = std::fs::remove_dir_all(victim_key.parent().unwrap());
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn replay_skips_merge_record_and_applies_following_push() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;

    // Two child commits on two branches, fetched into the bare repo so the
    // refs and their objects exist: main (merge) and feature (push).
    let work = std::env::temp_dir().join(format!("git-push-merge-replay-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("merged"), "m").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "merged"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "merge",
    ]))
    .await;
    let merge_oid = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    git_ok(git_cmd(&[
        "-C",
        tw,
        "checkout",
        "-q",
        "-b",
        "feature",
        fx.base_commit.as_str(),
    ]))
    .await;
    std::fs::write(work.join("feature"), "f").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feature"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "feature",
    ]))
    .await;
    let push_oid = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    seed_bare(fx.repo_path_str(), tw, "main:refs/heads/main").await;
    seed_bare(fx.repo_path_str(), tw, "feature:refs/heads/feature").await;
    let _ = std::fs::remove_dir_all(&work);

    let mut journal = Journal::open(&fx.repo_path).unwrap();
    let merge_op = Uuid::new_v4();
    journal
        .append_prepared(
            merge_op,
            Some(owner),
            vec![Update {
                old: fx.base_commit.clone(),
                new: merge_oid,
                reference: "refs/heads/main".into(),
            }],
            Some(Origin::Merge),
        )
        .unwrap();
    journal.append_marker(merge_op, Kind::Committed).unwrap();
    let push_op = Uuid::new_v4();
    journal
        .append_prepared(
            push_op,
            Some(owner),
            vec![Update {
                old: "0".repeat(40),
                new: push_oid,
                reference: "refs/heads/feature".into(),
            }],
            None,
        )
        .unwrap();
    journal.append_marker(push_op, Kind::Committed).unwrap();

    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(summary.committed, 2);

    // The merge record is replayed through `record_merge_result`, which writes
    // its own `repository_pushes` row (the `ref_updates` foreign key needs
    // one); it is never recorded as an ordinary push.
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, 2, "one repository_pushes row per applied record");
    let rc: i64 = sqlx::query_scalar("SELECT count(*) FROM ref_updates WHERE repository_id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(rc, 2);
    let feature_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id=$1 AND ref_name='refs/heads/feature'",
    )
    .bind(fx.repo_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(feature_rows, 1);
    let main_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id=$1 AND ref_name='refs/heads/main'",
    )
    .bind(fx.repo_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(main_rows, 1);

    // The high-water mark passes both records.
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(ja, 4);

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn update_ref_expected_journals_merge_origin_and_replay_records_merge_result() {
    let fx = setup(1 << 30).await;
    let owner = owner_id(&fx).await;

    // Build a child commit and bring its object into the bare repo without
    // moving any branch ref (objects only).
    let work = std::env::temp_dir().join(format!("git-push-merge-journal-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(work.join("merged"), "m").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "merged"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "merge",
    ]))
    .await;
    let child = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        tw,
        "HEAD:refs/remotes/tmp/merge",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&work);

    let op = Uuid::new_v4();
    let actor = owner;
    // The pull request and merge intent the journal record is replayed into
    // (the actor must be a real account: `merge_intents.actor_id` references
    // `accounts.id`).
    let pull_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests \
         (id, repository_id, number, title, body, author_id, source_branch, target_branch, source_repository_id) \
         VALUES ($1, $2, 1, 'merge', '', $3, 'feature', 'main', $2)",
    )
    .bind(pull_id)
    .bind(fx.repo_id)
    .bind(owner)
    .execute(&fx.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO merge_intents \
         (operation_id, repository_id, pull_request_id, source_ref, source_oid, target_ref, \
          expected_target_oid, method, policy_generation, checks_approvals_digest, actor_id, state) \
         VALUES ($1, $2, $3, 'feature', $4, 'main', $5, 'merge', 0, $6, $7, 'executing')",
    )
    .bind(op)
    .bind(fx.repo_id)
    .bind(pull_id)
    .bind(&child)
    .bind(&fx.base_commit)
    .bind(format!("digest-{}", Uuid::new_v4().simple()))
    .bind(owner)
    .execute(&fx.pool)
    .await
    .unwrap();

    let repo_path = fx.repo_path.clone();
    let hooks_dir = fx.storage_root.join("hooks");
    let expected_old = fx.base_commit.clone();
    let new_oid = child.clone();
    let guard = fx
        .state
        .storage
        .repository_lock(fx.repo_id)
        .lock_owned()
        .await;
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        githearth_git::merge::update_ref_expected(
            &repo_path,
            &hooks_dir,
            "refs/heads/main",
            &new_oid,
            &expected_old,
            op,
            actor,
            githearth_git::journal::Origin::Merge,
            Duration::from_secs(30),
        )
    })
    .await
    .unwrap();
    result.unwrap();

    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        child
    );

    let records = journal_records(&fx.repo_path);
    let prepared = records
        .iter()
        .find(|r| r.operation == op && r.kind == Kind::Prepared)
        .expect("merge prepared record");
    assert_eq!(prepared.origin, Some(Origin::Merge));
    assert_eq!(prepared.actor, Some(actor));
    assert_eq!(prepared.updates.len(), 1);
    assert_eq!(prepared.updates[0].reference, "refs/heads/main");
    assert_eq!(prepared.updates[0].old, fx.base_commit);
    assert_eq!(prepared.updates[0].new, child);
    assert!(
        records
            .iter()
            .any(|r| r.operation == op && r.kind == Kind::Committed),
        "committed marker follows the prepared record"
    );

    let summary = pushes::replay(&fx.state, fx.repo_id).await.unwrap();
    assert_eq!(summary.committed, 1);

    // The merge record is replayed into the intent, not recorded as a push.
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(
        pc, 1,
        "record_merge_result writes one repository_pushes row for the ref_updates foreign key"
    );

    let intent_state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(intent_state, "merged");
    let result_oid: String =
        sqlx::query_scalar("SELECT result_oid FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(result_oid, child);
    let journal_sequence: Option<i64> =
        sqlx::query_scalar("SELECT journal_sequence FROM merge_intents WHERE operation_id = $1")
            .bind(op)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    let sequence = journal_sequence.expect("merged intent records its journal sequence");

    let pull_state: String = sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
        .bind(pull_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(pull_state, "merged");

    let refs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_updates WHERE repository_id = $1 AND journal_sequence = $2",
    )
    .bind(fx.repo_id)
    .bind(sequence)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(refs, 1, "exactly one ref update for the merge sequence");

    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_and_merge_serialize_on_the_repository_lock() {
    let fx = setup(1 << 30).await;

    // The pusher clones main at the base commit and builds a child commit.
    let clone = std::env::temp_dir().join(format!("git-push-merge-lock-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap().to_owned();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd.as_str()])).await;
    std::fs::write(clone.join("p"), "p").unwrap();
    git_ok(git_cmd(&["-C", cd.as_str(), "add", "p"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd.as_str(),
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "p",
    ]))
    .await;

    // The merge commit is a different child of base, so the push becomes
    // non-fast-forward once the merge moves main.
    let merge_work = std::env::temp_dir().join(format!("git-push-merge-mw-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&merge_work).unwrap();
    let mw = merge_work.to_str().unwrap();
    git_ok(git_cmd(&["-C", mw, "clone", "--", fx.repo_path_str(), "."])).await;
    std::fs::write(merge_work.join("merged"), "m").unwrap();
    git_ok(git_cmd(&["-C", mw, "add", "merged"])).await;
    git_ok(git_cmd(&[
        "-C",
        mw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "merge",
    ]))
    .await;
    let merge_oid = git_output(git_cmd(&["-C", mw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "-c",
        "core.hooksPath=/dev/null",
        "fetch",
        "--",
        mw,
        "HEAD:refs/remotes/tmp/merge",
    ]))
    .await;
    let _ = std::fs::remove_dir_all(&merge_work);

    // Hold the per-repository lock, exactly as execute_intent will.
    let guard = fx
        .state
        .storage
        .repository_lock(fx.repo_id)
        .lock_owned()
        .await;

    // Start the push. Its lock-free advertisement completes against the base
    // commit, then the receive-pack POST holds a git permit while it blocks on
    // the per-repository lock.
    let initial_permits = fx.state.git_slots.available_permits();
    let push_task = tokio::spawn(async move {
        git_run(git_cmd(&["-C", cd.as_str(), "push", "origin", "main"])).await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while fx.state.git_slots.available_permits() >= initial_permits
        && tokio::time::Instant::now() < deadline
    {
        assert!(
            !push_task.is_finished(),
            "push completed instead of blocking on the lock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        fx.state.git_slots.available_permits() < initial_permits,
        "push never reached the server"
    );
    // Let the advertisement finish so the client is now waiting on the lock.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !push_task.is_finished(),
        "push must still be blocked while the merge holds the lock"
    );

    // Move main to the merge commit while holding the lock.
    let repo_path = fx.repo_path.clone();
    let hooks_dir = fx.storage_root.join("hooks");
    let expected_old = fx.base_commit.clone();
    let new_oid = merge_oid.clone();
    let op = Uuid::new_v4();
    let actor = Uuid::new_v4();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        githearth_git::merge::update_ref_expected(
            &repo_path,
            &hooks_dir,
            "refs/heads/main",
            &new_oid,
            &expected_old,
            op,
            actor,
            githearth_git::journal::Origin::Merge,
            Duration::from_secs(30),
        )
    })
    .await
    .unwrap();
    result.unwrap();

    // The lock is released when the closure returned; the push now proceeds
    // and is judged against the moved ref (a compare-and-swap the pushed ref
    // no longer satisfies), so it is rejected and the branch stays put.
    let out = push_task.await.unwrap();
    assert!(!out.status.success(), "stale push must be rejected");

    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        merge_oid
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

async fn setup_with_quota(repository_quota_bytes: u64) -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-push-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        git_push_limit_bytes: 1 << 30,
        git_concurrency: 8,
        repository_quota_bytes,
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
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;
    let repo_name = tag("repo");
    let repo_id = create_active_repo(&pool, org_id, &repo_name, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let repo_path = state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();

    let work = std::env::temp_dir().join(format!("githearth-push-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "# Hello\n").unwrap();
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
    let base_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    seed_bare(repo_path.to_str().unwrap(), tw, "main:refs/heads/main").await;
    let _ = std::fs::remove_dir_all(&work);

    let app = router(state.clone());
    let port = find_port_and_serve(&app).await;

    Fixture {
        app,
        state,
        pool,
        storage_root,
        username,
        org_slug,
        repo_name,
        repo_id,
        repo_path,
        base_commit,
        port,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn storage_quota_refuses_large_push_ref_unchanged_no_journal() {
    let fx = setup_with_quota(1_048_576).await; // 1 MiB quota

    let clone = std::env::temp_dir().join(format!("git-push-quota-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    let baseline: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();

    // Write a ~2 MiB file of non-compressible data and try to push
    let mut blob = vec![0u8; 2 * 1024 * 1024];
    let mut urandom = std::fs::File::open("/dev/urandom").unwrap();
    std::io::Read::read_exact(&mut urandom, &mut blob).unwrap();
    std::fs::write(clone.join("big.bin"), &blob).unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "big.bin"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "big",
    ]))
    .await;
    let out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    assert!(!out.status.success(), "push over quota must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("repository storage quota exceeded"),
        "rejection reason: {stderr}"
    );

    // No journal records written
    let records = journal_records(&fx.repo_path);
    assert!(
        !records.iter().any(|r| r.kind == Kind::Prepared),
        "no prepared records should be written"
    );

    // Ref unchanged
    assert_eq!(
        git_output(git_cmd(&[
            "--git-dir",
            fx.repo_path_str(),
            "rev-parse",
            "main"
        ]))
        .await,
        fx.base_commit
    );

    // No push metadata
    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pc, baseline, "no push metadata should be recorded");

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pushes_without_new_objects_succeed_over_quota() {
    // Seed 2 MiB of incompressible data past the 1 MiB quota (bypassing the hook),
    // then push a new ref at an existing commit and delete a branch.
    let fx = setup_with_quota(1_048_576).await;

    let work = std::env::temp_dir().join(format!("git-push-quota-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "clone", "--", fx.repo_path_str(), "."])).await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-b", "extra"])).await;
    let mut blob = vec![0u8; 2 * 1024 * 1024];
    let mut urandom = std::fs::File::open("/dev/urandom").unwrap();
    std::io::Read::read_exact(&mut urandom, &mut blob).unwrap();
    std::fs::write(work.join("x.bin"), &blob).unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "x.bin"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "extra",
    ]))
    .await;
    seed_bare(fx.repo_path_str(), tw, "extra:refs/heads/extra").await;
    let _ = std::fs::remove_dir_all(&work);

    let clone = std::env::temp_dir().join(format!("git-push-quota-del-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Git still creates a quarantine (with an empty pack/) for this push.
    git_ok(git_cmd(&[
        "-C",
        cd,
        "push",
        "origin",
        "origin/extra:refs/heads/copy",
    ]))
    .await;
    // A delete-only push sends no pack at all.
    git_ok(git_cmd(&["-C", cd, "push", "origin", ":extra"])).await;

    let refs_out = git_output(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads/",
    ]))
    .await;
    assert!(
        !refs_out.lines().any(|l| l == "refs/heads/extra"),
        "extra branch must be deleted: {refs_out}"
    );
    assert!(
        refs_out.lines().any(|l| l == "refs/heads/copy"),
        "copy must be created: {refs_out}"
    );

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn quota_zero_allows_large_push() {
    let fx = setup_with_quota(0).await;

    let clone = std::env::temp_dir().join(format!("git-push-quota-zero-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    // Write a ~2 MiB file of non-compressible data
    let mut blob = vec![0u8; 2 * 1024 * 1024];
    let mut urandom = std::fs::File::open("/dev/urandom").unwrap();
    std::io::Read::read_exact(&mut urandom, &mut blob).unwrap();
    std::fs::write(clone.join("big.bin"), &blob).unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "big.bin"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "big",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    wait_for_journal_applied(&fx.pool, fx.repo_id, 1).await;
    assert_push_metadata(&fx.pool, fx.repo_id, 1).await;

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_credentials_password() {
    let fx = setup(1 << 30).await;

    let clone = std::env::temp_dir().join(format!("git-push-pw-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    std::fs::write(clone.join("pw.txt"), "password auth\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "pw.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "pw",
    ]))
    .await;
    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    wait_for_journal_applied(&fx.pool, fx.repo_id, 1).await;
    assert_push_credentials(&fx.pool, fx.repo_id, "password", None).await;

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_credentials_token() {
    let fx = setup(1 << 30).await;
    let owner: Uuid = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT organization_id FROM repositories WHERE id=$1) AND role='owner'")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2")
        .bind(githearth_server::hash_password("testpass").unwrap())
        .bind(owner)
        .execute(&fx.pool)
        .await
        .unwrap();

    // Generate a 64-char hex session token.
    let mut session_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut session_bytes);
    let session = hex::encode(session_bytes);
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(session.as_bytes())
        .bind(owner)
        .execute(&fx.pool)
        .await
        .unwrap();

    let token_name = format!("tok-{}", Uuid::new_v4().simple());
    let (status, body) = request(
        &fx.app,
        "POST",
        "/api/v1/user/tokens",
        Some(&session),
        serde_json::json!({"name": token_name, "scopes": ["repo:write"]}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let token_plaintext = body["token"].as_str().unwrap().to_owned();
    let token_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // Push using the token. Embed the token in the URL as Basic auth password.
    let clone = std::env::temp_dir().join(format!("git-push-tok-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    let token_url = format!(
        "http://any:{}@127.0.0.1:{}/{}/{}.git",
        token_plaintext, fx.port, fx.org_slug, fx.repo_name
    );

    git_ok(git_cmd(&["clone", "--", &token_url, cd])).await;

    std::fs::write(clone.join("tok.txt"), "token auth\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "tok.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "tok",
    ]))
    .await;

    git_ok(git_cmd(&["-C", cd, "push", "origin", "main"])).await;

    wait_for_journal_applied(&fx.pool, fx.repo_id, 1).await;
    assert_push_credentials(&fx.pool, fx.repo_id, "token", Some(token_id)).await;

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

// ---------------------------------------------------------------------------
// repository.pushed credential fields + retention sweep
// ---------------------------------------------------------------------------

/// A repository + organization + actor, scoped to unique fixture rows so these
/// tests can share the database with the rest of the suite.
struct CredFixture {
    pool: PgPool,
    repo_id: Uuid,
    org_id: Uuid,
    actor: Uuid,
}

async fn cred_fixture() -> CredFixture {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let actor = fixture_account(&pool, &tag("owner")).await;
    let (org_id, _) = fixture_organization(&pool, actor).await;
    let repo_id = create_active_repo(&pool, org_id, &tag("repo"), actor).await;
    CredFixture {
        pool,
        repo_id,
        org_id,
        actor,
    }
}

async fn apply_record(pool: &PgPool, repo_id: Uuid, operation: Uuid, actor: Uuid) {
    let record = Record {
        sequence: 1,
        operation,
        kind: Kind::Prepared,
        actor: Some(actor),
        origin: None,
        updates: vec![Update {
            old: "a".repeat(40),
            new: "b".repeat(40),
            reference: "refs/heads/main".into(),
        }],
    };
    let mut conn = pool.acquire().await.unwrap();
    let (inserted, _) = pushes::apply_committed(&mut conn, repo_id, &record)
        .await
        .unwrap();
    assert!(inserted, "apply_committed must insert the push");
}

async fn pushed_payload(pool: &PgPool, org_id: Uuid) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE kind='repository.pushed' AND organization_id=$1",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pushed_event_payload_includes_credential_kind_and_id() {
    let fx = cred_fixture().await;
    let operation = Uuid::new_v4();
    let token_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO push_credentials (operation_id, credential_kind, credential_id) \
         VALUES ($1, 'token', $2)",
    )
    .bind(operation)
    .bind(token_id)
    .execute(&fx.pool)
    .await
    .unwrap();

    apply_record(&fx.pool, fx.repo_id, operation, fx.actor).await;

    let payload = pushed_payload(&fx.pool, fx.org_id).await;
    assert_eq!(payload["credential_kind"], serde_json::json!("token"));
    assert_eq!(
        payload["credential_id"],
        serde_json::json!(token_id.to_string())
    );

    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pushed_event_payload_credentials_are_null_when_row_absent() {
    let fx = cred_fixture().await;
    let operation = Uuid::new_v4();

    apply_record(&fx.pool, fx.repo_id, operation, fx.actor).await;

    let payload = pushed_payload(&fx.pool, fx.org_id).await;
    assert!(payload.get("credential_kind").is_some());
    assert!(payload.get("credential_id").is_some());
    assert!(payload["credential_kind"].is_null());
    assert!(payload["credential_id"].is_null());

    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sweep_credentials_deletes_only_rows_older_than_a_day() {
    let fx = cred_fixture().await;
    let recent = Uuid::new_v4();
    let old_applied = Uuid::new_v4();
    let old_unapplied = Uuid::new_v4();
    let fresh = Uuid::new_v4();

    // Recent (12h) row: kept by the age filter.
    sqlx::query(
        "INSERT INTO push_credentials (operation_id, credential_kind, created_at) \
         VALUES ($1, 'password', now() - interval '12 hours')",
    )
    .bind(recent)
    .execute(&fx.pool)
    .await
    .unwrap();
    // Aged (2 days) row with an applied push: deleted.
    sqlx::query(
        "INSERT INTO push_credentials (operation_id, credential_kind, created_at) \
         VALUES ($1, 'password', now() - interval '2 days')",
    )
    .bind(old_applied)
    .execute(&fx.pool)
    .await
    .unwrap();
    apply_record(&fx.pool, fx.repo_id, old_applied, fx.actor).await;
    // Aged (2 days) row without an applied push: kept.
    sqlx::query(
        "INSERT INTO push_credentials (operation_id, credential_kind, created_at) \
         VALUES ($1, 'password', now() - interval '2 days')",
    )
    .bind(old_unapplied)
    .execute(&fx.pool)
    .await
    .unwrap();
    // Fresh row: kept.
    sqlx::query(
        "INSERT INTO push_credentials (operation_id, credential_kind) VALUES ($1, 'password')",
    )
    .bind(fresh)
    .execute(&fx.pool)
    .await
    .unwrap();

    let purged = {
        let mut conn = fx.pool.acquire().await.unwrap();
        pushes::sweep_credentials(&mut conn).await.unwrap()
    };
    // The database is shared with the rest of the suite, and sweep_credentials
    // deletes every aged-and-applied row in it, so other tests' rows make the
    // exact count non-deterministic. A >= 1 bound is the strongest check on the
    // return value; the assertions below verify this test's own rows precisely.
    assert!(purged >= 1, "sweep must delete the aged applied row");

    let remaining: Vec<Uuid> = sqlx::query_scalar(
        "SELECT operation_id FROM push_credentials WHERE operation_id = ANY($1)",
    )
    .bind(vec![recent, old_applied, old_unapplied, fresh])
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    assert!(
        !remaining.contains(&old_applied),
        "aged applied row must be deleted"
    );
    assert!(
        remaining.contains(&old_unapplied),
        "aged row without an applied push must survive"
    );
    assert!(remaining.contains(&recent), "12h-old row must survive");
    assert!(remaining.contains(&fresh), "fresh row must survive");

    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fetch_inserts_no_push_credentials_row() {
    let fx = setup(1 << 30).await;

    let clone = std::env::temp_dir().join(format!("git-fetch-nop-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    git_ok(git_cmd(&["clone", "--", &fx.url(), cd])).await;

    let pc: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM push_credentials pc \
         JOIN repository_pushes rp ON rp.operation_id = pc.operation_id \
         WHERE rp.repository_id = $1",
    )
    .bind(fx.repo_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(pc, 0, "fetch must not insert push_credentials");

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_credentials_insert_failure_http_refused() {
    let fx = setup(1 << 30).await;
    let owner: Uuid = sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id=(SELECT organization_id FROM repositories WHERE id=$1) AND role='owner'")
        .bind(fx.repo_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2")
        .bind(githearth_server::hash_password("testpass").unwrap())
        .bind(owner)
        .execute(&fx.pool)
        .await
        .unwrap();

    let mut session_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut session_bytes);
    let session = hex::encode(session_bytes);
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(session.as_bytes())
        .bind(owner)
        .execute(&fx.pool)
        .await
        .unwrap();

    let token_name = format!("tok-{}", Uuid::new_v4().simple());
    let (status, body) = request(
        &fx.app,
        "POST",
        "/api/v1/user/tokens",
        Some(&session),
        serde_json::json!({"name": token_name, "scopes": ["repo:write"]}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{body}");
    let token_plaintext = body["token"].as_str().unwrap().to_owned();
    let token_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let clone = std::env::temp_dir().join(format!("git-push-pcs-fail-{}", Uuid::new_v4()));
    let cd = clone.to_str().unwrap();
    let token_url = format!(
        "http://any:{}@127.0.0.1:{}/{}/{}.git",
        token_plaintext, fx.port, fx.org_slug, fx.repo_name
    );

    // Clean up any leftover trigger from a previous aborted run.
    let drop_sql = format!(
        "DROP TRIGGER IF EXISTS pcrej_http_{} ON push_credentials",
        token_id.simple()
    );
    sqlx::query(&drop_sql).execute(&fx.pool).await.ok();

    sqlx::query(&format!(
        "CREATE OR REPLACE FUNCTION pcrej_http_fn_{}() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'injected push_credentials insert failure for test';
         END $$ LANGUAGE plpgsql",
        token_id.simple()
    ))
    .execute(&fx.pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER pcrej_http_{} BEFORE INSERT ON push_credentials FOR EACH ROW \
         WHEN (NEW.credential_id = '{}') EXECUTE FUNCTION pcrej_http_fn_{}()",
        token_id.simple(),
        token_id,
        token_id.simple()
    ))
    .execute(&fx.pool)
    .await
    .unwrap();

    git_ok(git_cmd(&["clone", "--", &token_url, cd])).await;
    std::fs::write(clone.join("fail.txt"), "insert failure test\n").unwrap();
    git_ok(git_cmd(&["-C", cd, "add", "fail.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        cd,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "fail",
    ]))
    .await;

    let push_out = git_run(git_cmd(&["-C", cd, "push", "origin", "main"])).await;
    let stderr = String::from_utf8_lossy(&push_out.stderr);
    assert!(
        !push_out.status.success(),
        "push must be refused when push_credentials insert fails"
    );
    assert!(
        stderr.contains("HTTP 500") || stderr.contains("internal server error"),
        "expected HTTP 500 error in stderr, got: {stderr}"
    );

    let pushes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(fx.repo_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pushes, 0, "no repository_pushes row must exist");

    let remote_head = git_output(git_cmd(&[
        "--git-dir",
        fx.repo_path_str(),
        "rev-parse",
        "refs/heads/main",
    ]))
    .await;
    assert_eq!(
        remote_head.trim(),
        fx.base_commit.trim(),
        "remote ref must not have changed"
    );

    sqlx::query(&format!(
        "DROP TRIGGER pcrej_http_{} ON push_credentials",
        token_id.simple()
    ))
    .execute(&fx.pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "DROP FUNCTION pcrej_http_fn_{}",
        token_id.simple()
    ))
    .execute(&fx.pool)
    .await
    .unwrap();

    let _ = std::fs::remove_dir_all(&clone);
    cleanup(&fx.storage_root);
    fx.pool.close().await;
}
