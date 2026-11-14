//! Repository housekeeping integration tests (ADR 0013, U11).
//!
//! One disposable database per test; a real HTTP Git server on port 0 for the
//! push and clone cases. Grace is controlled by configuration and by setting
//! object file times, never by sleeping.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use githearth_git::layout::ObjectFormat;
use githearth_git::maintenance::FaultPoint;
use githearth_server::{
    AppState, MIGRATOR,
    config::Config,
    connect,
    housekeeping::{self, Outcome},
    router,
};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Database and storage fixtures
// ---------------------------------------------------------------------------

async fn fresh_database(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("GITHEARTH_TEST_DATABASE_URL");
    let (origin, _) = url.rsplit_once('/').unwrap();
    let maintenance = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maintenance)
        .await;
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maintenance)
        .await
        .unwrap();
    maintenance.close().await;
    format!("{origin}/{name}")
}

fn database_name(prefix: &str) -> String {
    format!(
        "githearth_hk_{}_{}",
        prefix.replace('-', "_"),
        Uuid::new_v4().to_string().replace('-', "_")
    )
}

/// A short storage root: the hook socket path is `<root>/run/hook-XXXXXXXX.sock`
/// and must fit `sun_path` (108 bytes on Linux), so a long temporary prefix
/// cannot be used.
fn temp_storage_root() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/gthk-{}",
        &Uuid::new_v4().simple().to_string()[..12]
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn setup(prefix: &str, tweak: impl FnOnce(&mut Config)) -> Fixture {
    let database_url = fresh_database(&database_name(prefix)).await;
    let storage_root = temp_storage_root();
    let mut config = Config {
        database_url,
        storage_root: storage_root.clone(),
        database_max_connections: 6,
        database_timeout_seconds: 5,
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        // The job runner polls quickly so the durable-job test is bounded by a
        // condition, not a sleep.
        job_poll_interval_ms: 50,
        job_timeout_seconds: 30,
        ..Config::default()
    };
    tweak(&mut config);
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool).await;
    set_password(&pool, owner, "testpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;

    let state = AppState::new(pool.clone(), config.clone()).await.unwrap();
    let token = session_token(&pool, owner).await;
    let app = router(state.clone());
    // Create the main repository through the API, exactly as production does:
    // the authorization facts the API tests rely on are the ones the pull
    // request route reads too.
    let repo_name = format!("repo-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/organizations/{org_slug}/repositories"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({ "name": repo_name }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create repository: {}",
        String::from_utf8_lossy(&body)
    );
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let repo_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    let repo_path = state
        .storage
        .create(repo_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    let base_commit = seed_commit(&repo_path);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Fixture {
        state,
        pool,
        storage_root,
        database_url: config.database_url.clone(),
        owner,
        org_id,
        org_slug,
        repo_name,
        repo_id,
        repo_path,
        base_commit,
        username,
        token,
        port,
        config,
    }
}

struct Fixture {
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    database_url: String,
    owner: Uuid,
    org_id: Uuid,
    org_slug: String,
    repo_name: String,
    repo_id: Uuid,
    repo_path: PathBuf,
    base_commit: String,
    username: String,
    token: String,
    port: u16,
    config: Config,
}

impl Fixture {
    fn url(&self) -> String {
        format!(
            "http://{}:testpass@127.0.0.1:{}/{}/{}.git",
            self.username, self.port, self.org_slug, self.repo_name
        )
    }

    fn objects_size(&self) -> u64 {
        githearth_git::layout::dir_size(&self.repo_path.join("objects"), None).unwrap()
    }

    fn has_object(&self, oid: &str) -> bool {
        Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .arg("--git-dir")
            .arg(&self.repo_path)
            .args(["cat-file", "-e", oid])
            .status()
            .unwrap()
            .success()
    }

    fn fsck(&self) {
        let output = git(
            &["--git-dir", self.repo_path.to_str().unwrap()],
            &["fsck", "--full", "--no-dangling"],
        );
        assert!(
            output.status.success(),
            "fsck failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn run(&self) -> Outcome {
        match housekeeping::run_repository(&self.state, self.repo_id).await {
            Ok(outcome) => outcome,
            Err(error) => panic!("housekeeping failed: {error}"),
        }
    }

    async fn run_expecting_error(&self) -> String {
        match housekeeping::run_repository(&self.state, self.repo_id).await {
            Ok(outcome) => panic!("expected a failed run, got {outcome:?}"),
            Err(error) => error.to_string(),
        }
    }

    async fn housekeeping_row(&self) -> (i64, i64, i32, String) {
        sqlx::query_as(
            "SELECT bytes_before, bytes_after, expired_objects, last_outcome \
             FROM repository_housekeeping WHERE repository_id = $1",
        )
        .bind(self.repo_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn audit_kinds(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT kind FROM audit_events WHERE organization_id = $1 ORDER BY created_at",
        )
        .bind(self.org_id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Age every file under `objects/` so it is older than any grace period.
    fn age_objects(&self) {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(2 * 3600))
            .unwrap();
        age_tree(&self.repo_path.join("objects"), cutoff);
    }

    /// Clone the repository over HTTP and return the clone directory.
    async fn clone(&self) -> PathBuf {
        let into = std::env::temp_dir().join(format!("githearth-hk-clone-{}", Uuid::new_v4()));
        let url = self.url();
        let dir = into.clone();
        let output = tokio::task::spawn_blocking(move || {
            git(
                &[],
                &["clone", "--quiet", "--", &url, dir.to_str().unwrap()],
            )
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        into
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage_root);
    }
}

async fn teardown(fx: Fixture) {
    fx.pool.close().await;
    drop_database(&fx.database_url).await;
}

/// Drops the disposable database so repeated runs do not accumulate one per
/// test on the shared cluster.
async fn drop_database(url: &str) {
    let (origin, name) = url.rsplit_once('/').expect("database url");
    let Ok(maintenance) = PgPool::connect(&format!("{origin}/postgres")).await else {
        return;
    };
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maintenance)
        .await;
    maintenance.close().await;
}

fn age_tree(dir: &Path, cutoff: std::time::SystemTime) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            age_tree(&path, cutoff);
        } else {
            std::fs::File::open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(cutoff))
                .unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Database fixtures
// ---------------------------------------------------------------------------

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..10])
}

async fn fixture_account(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(tag("user"))
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

/// A bearer token for `account`, inserted exactly as the login route stores
/// one, so API calls in a test authenticate without a second HTTP hop.
async fn session_token(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,now() + interval '1 hour')",
    )
    .bind(&token)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    token
}

async fn fixture_organization(pool: &PgPool, owner: Uuid) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Housekeeping Org')")
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
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
         VALUES ($1,$2,$3,$4,$5,'active','private')",
    )
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
// Git helpers
// ---------------------------------------------------------------------------

fn git(global: &[&str], args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(global)
        .args(args)
        .output()
        .unwrap()
}

fn git_ok(global: &[&str], args: &[&str]) -> String {
    let output = git(global, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// One authenticated API call against the fixture's state, returning the
/// status and the JSON body.
async fn call(
    fx: &Fixture,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let app = router(fx.state.clone());
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {}", fx.token));
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// One commit on `main` in the bare repository, without running the hooks.
fn seed_commit(repo: &Path) -> String {
    let dir = std::env::temp_dir().join(format!("githearth-hk-seed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let work = dir.to_str().unwrap();
    git_ok(&[], &["-C", work, "init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "# Hello\n").unwrap();
    git_ok(&[], &["-C", work, "add", "README.md"]);
    git_ok(
        &[],
        &[
            "-C",
            work,
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ],
    );
    let head = git_ok(&[], &["-C", work, "rev-parse", "HEAD"]);
    git_ok(
        &["--git-dir", repo.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--",
            work,
            "main:refs/heads/main",
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    head
}

/// Commit `content` as `name` on the checked-out branch of `clone` and push.
async fn commit_and_push(clone: &Path, name: &str, content: &str) -> std::process::Output {
    let work = clone.to_path_buf();
    let name = name.to_owned();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || {
        let dir = work.to_str().unwrap();
        std::fs::write(work.join(&name), &content).unwrap();
        git_ok(&[], &["-C", dir, "add", &name]);
        git_ok(
            &[],
            &[
                "-C",
                dir,
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                &format!("add {name}"),
            ],
        );
        git(&[], &["-C", dir, "push", "origin", "HEAD"])
    })
    .await
    .unwrap()
}

/// Commit `content` on a fresh branch `branch` and push it.
async fn commit_branch_and_push(
    clone: &Path,
    branch: &str,
    name: &str,
    content: &[u8],
) -> std::process::Output {
    let work = clone.to_path_buf();
    let branch = branch.to_owned();
    let name = name.to_owned();
    let content = content.to_vec();
    tokio::task::spawn_blocking(move || {
        let dir = work.to_str().unwrap();
        git_ok(&[], &["-C", dir, "checkout", "-b", &branch]);
        std::fs::write(work.join(&name), &content).unwrap();
        git_ok(&[], &["-C", dir, "add", &name]);
        git_ok(
            &[],
            &[
                "-C",
                dir,
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                &format!("add {name}"),
            ],
        );
        git(&[], &["-C", dir, "push", "origin", &branch])
    })
    .await
    .unwrap()
}

async fn push_delete(clone: &Path, branch: &str) -> std::process::Output {
    let work = clone.to_path_buf();
    let branch = branch.to_owned();
    tokio::task::spawn_blocking(move || {
        git(
            &[],
            &[
                "-C",
                work.to_str().unwrap(),
                "push",
                "origin",
                "--delete",
                &branch,
            ],
        )
    })
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Tests: what is kept and what is freed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn deleted_branch_is_kept_before_the_grace_period_and_removed_after() {
    let fx = setup("grace", |config| {
        config.housekeeping_grace_seconds = 3600;
    })
    .await;

    let clone = fx.clone().await;
    let output = commit_branch_and_push(&clone, "feature", "feature.txt", b"feature\n").await;
    assert!(
        output.status.success(),
        "push: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let feature_tip = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", "refs/heads/feature"],
    );
    let feature_objects = objects_only_in(&fx.repo_path, &feature_tip, &fx.base_commit);
    assert_eq!(feature_objects.len(), 3, "commit, tree and blob");

    // A deleted branch's objects are unreachable, but young.
    let output = push_delete(&clone, "feature").await;
    assert!(
        output.status.success(),
        "delete: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // First run packs what is reachable; the branch's objects are younger than
    // the grace period and stay.
    let first = fx.run().await;
    let Outcome::Completed(report) = first else {
        panic!("expected a completed run, got {first:?}");
    };
    assert!(report.repacked && report.pruned);
    for oid in &feature_objects {
        assert!(fx.has_object(oid), "kept before the grace period: {oid}");
    }
    let packed = fx.objects_size();

    // Past the grace period the same run removes them and frees space.
    fx.age_objects();
    let second = fx.run().await;
    let Outcome::Completed(report) = second else {
        panic!("expected a completed run, got {second:?}");
    };
    for oid in &feature_objects {
        assert!(!fx.has_object(oid), "removed after the grace period: {oid}");
    }
    assert!(
        fx.objects_size() < packed,
        "maintenance freed space: {packed} -> {}",
        fx.objects_size()
    );
    assert!(report.bytes_before >= report.bytes_after);
    assert!(fx.has_object(&fx.base_commit));

    let (before, after, expired, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    assert_eq!(before as u64, report.bytes_before);
    assert_eq!(after as u64, report.bytes_after);
    assert_eq!(expired, 3);

    fx.fsck();
    let again = fx.clone().await;
    assert_eq!(
        git_ok(&["-C", again.to_str().unwrap()], &["rev-parse", "HEAD"]),
        fx.base_commit,
        "the clone has the reachable history"
    );
    let _ = std::fs::remove_dir_all(&again);
    let _ = std::fs::remove_dir_all(&clone);
    assert!(
        fx.audit_kinds()
            .await
            .iter()
            .any(|k| k == housekeeping::JOB_KIND)
    );
    teardown(fx).await;
}

/// Deterministic, incompressible text: a push of it adds its length to the
/// quarantine, which a repeated character would not (a pack compresses it).
fn incompressible(bytes: usize) -> Vec<u8> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(bytes);
    for _ in 0..bytes {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 33) as u8);
    }
    out
}

/// Objects reachable from `tip` but not from `base`.
fn objects_only_in(repo: &Path, tip: &str, base: &str) -> Vec<String> {
    let reachable = |rev: &str| -> std::collections::HashSet<String> {
        let output = git(
            &["--git-dir", repo.to_str().unwrap()],
            &["rev-list", "--objects", rev],
        );
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split(' ').next().map(str::to_owned))
            .filter(|oid| oid.len() == 40 || oid.len() == 64)
            .collect()
    };
    let base = reachable(base);
    reachable(tip)
        .into_iter()
        .filter(|oid| !base.contains(oid))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn quota_accepts_a_push_after_maintenance_frees_space() {
    let fx = setup("quota", |config| {
        config.repository_quota_bytes = 1_048_576;
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_interval_seconds = 86_400;
    })
    .await;
    let clone = fx.clone().await;

    // A branch larger than the quota allows to keep on top of the base.
    let big = incompressible(900_000);
    let output = commit_branch_and_push(&clone, "big", "big.bin", &big).await;
    assert!(
        output.status.success(),
        "first push: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = push_delete(&clone, "big").await;
    assert!(
        output.status.success(),
        "delete: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The unreachable blob still fills `objects/`, so a push that adds more
    // than the quota is refused by the pre-receive check.
    let used = fx.objects_size();
    assert!(used > 500_000, "the deleted branch still counts: {used}");
    let output =
        commit_branch_and_push(&clone, "again", "again.bin", &incompressible(600_000)).await;
    assert!(
        !output.status.success(),
        "quota must refuse while the deleted branch still counts"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("quota exceeded"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Maintenance with a zero grace period reclaims the space (the test ages
    // the files only if the filesystem clock is too coarse to make them old).
    fx.age_objects();
    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(
        fx.objects_size() < used,
        "housekeeping freed space: {used} -> {}",
        fx.objects_size()
    );

    // A fresh clone: the refused attempt left its local branch behind.
    let _ = std::fs::remove_dir_all(&clone);
    let clone = fx.clone().await;
    let output =
        commit_branch_and_push(&clone, "again", "again.bin", &incompressible(600_000)).await;
    assert!(
        output.status.success(),
        "the same push must succeed after maintenance: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fx.fsck();
    let _ = std::fs::remove_dir_all(&clone);
    teardown(fx).await;
}

// ---------------------------------------------------------------------------
// Tests: concurrent writers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_push_racing_maintenance_completes_and_keeps_its_objects() {
    let fx = setup("push-race", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let clone = fx.clone().await;
    let repo_id = fx.repo_id;
    let state = fx.state.clone();

    // Start the push and the run together; the per-repository lock decides who
    // goes first, and either order is correct. The barrier makes both sides
    // actually meet instead of relying on a sleep.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let push_clone = clone.clone();
    let push_barrier = barrier.clone();
    let push = tokio::spawn(async move {
        push_barrier.wait().await;
        let mut results = Vec::new();
        for i in 0..3 {
            let output =
                commit_and_push(&push_clone, &format!("race-{i}.txt"), &format!("{i}\n")).await;
            results.push(output);
        }
        results
    });
    let run_barrier = barrier.clone();
    let run = tokio::spawn(async move {
        run_barrier.wait().await;
        let mut outcomes = Vec::new();
        for _ in 0..3 {
            outcomes.push(housekeeping::run_repository(&state, repo_id).await);
        }
        outcomes
    });

    for output in push.await.unwrap() {
        assert!(
            output.status.success(),
            "a racing push must complete or be refused cleanly, never corrupt: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for outcome in run.await.unwrap() {
        assert!(outcome.is_ok(), "racing run: {:?}", outcome.err());
    }

    let head = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", "refs/heads/main"],
    );
    let clone_back = fx.clone().await;
    assert_eq!(
        git_ok(
            &["-C", clone_back.to_str().unwrap()],
            &["rev-parse", "HEAD"]
        ),
        head,
        "the clone sees exactly the pushed commit"
    );
    let worktree = clone_back.join("race-2.txt");
    assert!(worktree.exists(), "every pushed file is present");
    fx.fsck();
    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(&clone_back);
    teardown(fx).await;
}

/// A side branch with its own commit, fetched into the bare repository so the
/// merge can resolve it. Returns the commit id.
async fn side_branch(fx: &Fixture) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("githearth-hk-merge-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let work = dir.to_str().unwrap();
    git_ok(
        &[],
        &[
            "clone",
            "--quiet",
            "--",
            fx.repo_path.to_str().unwrap(),
            work,
        ],
    );
    std::fs::write(dir.join("side.txt"), "side\n").unwrap();
    git_ok(&[], &["-C", work, "add", "side.txt"]);
    git_ok(
        &[],
        &[
            "-C",
            work,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "side",
        ],
    );
    let side = git_ok(&[], &["-C", work, "rev-parse", "HEAD"]);
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--",
            work,
            "HEAD:refs/heads/side",
        ],
    );
    (dir, side)
}

/// A pull request and a merge intent already in `executing`, exactly the state
/// A's worker hands to `execute_intent`. The digest is the one the API would
/// compute with no approvals and no required checks.
async fn insert_executing_intent(fx: &Fixture, source: &str) -> Uuid {
    let pull = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch,source_repository_id) \
         VALUES ($1,$2,1,'race','',$3,'side','main',$2)",
    )
    .bind(pull)
    .bind(fx.repo_id)
    .bind(fx.owner)
    .execute(&fx.pool)
    .await
    .unwrap();
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        hex::encode(hasher.finalize())
    };
    let operation = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO merge_intents (operation_id, repository_id, pull_request_id, source_ref, \
         source_oid, target_ref, expected_target_oid, method, policy_generation, \
         checks_approvals_digest, actor_id, state) \
         VALUES ($1,$2,$3,'side',$4,'main',$5,'merge',0,$6,$7,'executing')",
    )
    .bind(operation)
    .bind(fx.repo_id)
    .bind(pull)
    .bind(source)
    .bind(&fx.base_commit)
    .bind(&digest)
    .bind(fx.owner)
    .execute(&fx.pool)
    .await
    .unwrap();
    operation
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL test database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_real_merge_racing_maintenance_persists_and_keeps_the_candidate() {
    let fx = setup("merge-race", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let (dir, side) = side_branch(&fx).await;
    let operation = insert_executing_intent(&fx, &side).await;

    // A real `execute_intent` and a real maintenance run race. Both take the
    // same per-repository lock, so one runs entirely before the other; either
    // order must keep the merge's objects.
    let run = tokio::spawn({
        let state = fx.state.clone();
        let repo_id = fx.repo_id;
        async move { housekeeping::run_repository(&state, repo_id).await }
    });
    let merged = githearth_server::merge::execute_intent(&fx.state, operation)
        .await
        .unwrap();
    let oid = match merged {
        githearth_server::merge::MergeOutcome::Merged { oid } => oid,
        other => panic!("expected the merge to land, got {other:?}"),
    };
    assert!(run.await.unwrap().is_ok(), "maintenance completes");

    // The ref moved and the candidate is reachable.
    assert_eq!(
        git_ok(
            &["--git-dir", fx.repo_path.to_str().unwrap()],
            &["rev-parse", "refs/heads/main"],
        ),
        oid
    );
    assert!(fx.has_object(&oid));
    assert!(fx.has_object(&side));

    // The production path persisted the candidate while it held the lock, so a
    // crash in the window would leave a keep point rather than an orphan.
    let candidate: Option<String> =
        sqlx::query_scalar("SELECT candidate_oid FROM merge_intents WHERE operation_id = $1")
            .bind(operation)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(candidate.as_deref(), Some(oid.as_str()));

    fx.fsck();
    let clone = fx.clone().await;
    assert_eq!(
        git_ok(&["-C", clone.to_str().unwrap()], &["rev-parse", "HEAD"]),
        oid
    );
    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL test database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_merge_that_crashed_before_its_ref_update_leaves_a_pinned_candidate() {
    let fx = setup("merge-crash", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let (dir, side) = side_branch(&fx).await;
    let operation = insert_executing_intent(&fx, &side).await;

    // Crash after the candidate is persisted and before the ref moves.
    githearth_server::merge::fail_after_candidate(operation);
    let error = githearth_server::merge::execute_intent(&fx.state, operation)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("after_candidate"), "{error}");
    let candidate: String =
        sqlx::query_scalar("SELECT candidate_oid FROM merge_intents WHERE operation_id = $1")
            .bind(operation)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(fx.has_object(&candidate));

    // Maintenance with a zero grace period must keep the candidate: the intent
    // is still executing, and its `candidate_oid` is the keep point.
    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(
        fx.has_object(&candidate),
        "the persisted candidate survives the prune"
    );
    assert!(fx.has_object(&side));

    // The retry reuses that same candidate and lands it.
    let merged = githearth_server::merge::execute_intent(&fx.state, operation)
        .await
        .unwrap();
    match merged {
        githearth_server::merge::MergeOutcome::Merged { oid } => assert_eq!(oid, candidate),
        other => panic!("expected the retry to merge, got {other:?}"),
    }
    assert_eq!(
        git_ok(
            &["--git-dir", fx.repo_path.to_str().unwrap()],
            &["rev-parse", "refs/heads/main"],
        ),
        candidate
    );
    fx.fsck();
    let clone = fx.clone().await;
    assert_eq!(
        git_ok(&["-C", clone.to_str().unwrap()], &["rev-parse", "HEAD"]),
        candidate
    );
    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_fork_and_its_parent_both_clone_after_maintenance() {
    let fx = setup("fork", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;

    // A real fork, created through the storage layer exactly as the fork API
    // does: a full copy of refs and objects, independent object store.
    let fork_id = create_active_repo(&fx.pool, fx.org_id, &tag("fork"), fx.owner).await;
    sqlx::query("UPDATE repositories SET parent_repository_id = $1 WHERE id = $2")
        .bind(fx.repo_id)
        .bind(fork_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let fork_path = fx
        .state
        .storage
        .create(fork_id, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    fx.state.storage.fetch_refs(fork_id, fx.repo_id).unwrap();
    assert_ne!(fork_path, fx.repo_path);

    // Extra history only in the parent, then both repositories are maintained.
    let clone = fx.clone().await;
    let output = commit_and_push(&clone, "parent-only.txt", "parent\n").await;
    assert!(output.status.success());

    let parent_outcome = fx.run().await;
    assert!(matches!(parent_outcome, Outcome::Completed(_)));
    let fork_outcome = housekeeping::run_repository(&fx.state, fork_id)
        .await
        .unwrap();
    assert!(
        matches!(fork_outcome, Outcome::Completed(_)),
        "{fork_outcome:?}"
    );

    fx.fsck();
    let parent_clone = fx.clone().await;
    let fork_url = format!(
        "http://{}:testpass@127.0.0.1:{}/{}/{}.git",
        fx.username,
        fx.port,
        fx.org_slug,
        sqlx::query_scalar::<_, String>("SELECT name FROM repositories WHERE id = $1")
            .bind(fork_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap()
    );
    let fork_clone = std::env::temp_dir().join(format!("githearth-hk-fork-{}", Uuid::new_v4()));
    let dir = fork_clone.clone();
    let output = tokio::task::spawn_blocking(move || {
        git(
            &[],
            &["clone", "--quiet", "--", &fork_url, dir.to_str().unwrap()],
        )
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "fork clone: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git_ok(
            &["-C", parent_clone.to_str().unwrap()],
            &["rev-parse", "HEAD"]
        ),
        git_ok(
            &["--git-dir", fx.repo_path.to_str().unwrap()],
            &["rev-parse", "refs/heads/main"]
        )
    );
    assert_eq!(
        git_ok(
            &["-C", fork_clone.to_str().unwrap()],
            &["rev-parse", "HEAD"]
        ),
        fx.base_commit,
        "the fork still has the history it was copied with"
    );
    let _ = std::fs::remove_dir_all(&parent_clone);
    let _ = std::fs::remove_dir_all(&fork_clone);
    let _ = std::fs::remove_dir_all(&clone);
    teardown(fx).await;
}

// ---------------------------------------------------------------------------
// Tests: pull-request head pins
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_closed_pull_request_still_renders_its_diff_after_its_branch_is_pruned() {
    let fx = setup("pin", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;

    // A branch and a pull request row. The production creation path writes the
    // pin in the same transaction; `tests/pulls.rs` asserts that, and this test
    // uses the same helper so it can focus on what maintenance does with it.
    let (dir, head) = side_branch(&fx).await;
    let pull_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pull_requests (id,repository_id,number,title,body,author_id,source_branch,target_branch,source_repository_id) \
         VALUES ($1,$2,1,'pin','',$3,'side','main',$2)",
    )
    .bind(pull_id)
    .bind(fx.repo_id)
    .bind(fx.owner)
    .execute(&fx.pool)
    .await
    .unwrap();
    let mut conn = fx.pool.acquire().await.unwrap();
    githearth_server::pins::pin_head(&mut conn, fx.repo_id, pull_id, &head, 30)
        .await
        .unwrap();
    drop(conn);

    let pinned: (String, String) =
        sqlx::query_as("SELECT commit_oid, reason FROM pinned_commits WHERE pull_request_id = $1")
            .bind(pull_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(pinned.0, head, "the head is pinned");
    assert_eq!(pinned.1, "pull_request_head");

    // The branch disappears, the pull request closes, and an unrelated
    // unreachable object is old enough to prune.
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "-d",
            "refs/heads/side",
        ],
    );
    sqlx::query("UPDATE pull_requests SET state = 'closed', closed_at = now() WHERE id = $1")
        .bind(pull_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();

    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(
        fx.has_object(&head),
        "the closed pull request's pinned head survives the prune"
    );
    assert!(!fx.has_object(&doomed), "the prune really ran");
    // Age the freshly loosened objects and run again: the pin, not the grace
    // period, is what keeps the head this time.
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    assert!(
        fx.has_object(&head),
        "the pin keeps the head once its grace period is over"
    );

    // The diff still renders: Git can read the pinned commit and its tree even
    // though no branch points at it. Pins are not refs and are never
    // advertised.
    let diff = git(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["diff", "--stat", "main", &head],
    );
    assert!(
        diff.status.success(),
        "the pinned head still renders a diff: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    assert!(
        String::from_utf8_lossy(&diff.stdout).contains("side.txt"),
        "the diff names the changed file: {}",
        String::from_utf8_lossy(&diff.stdout)
    );
    assert!(fx.has_object(&format!("{head}:side.txt")));

    // The pinned objects are inside `objects/`, so the quota counts them; they
    // are not exempt from the limit.
    let blob = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", &format!("{head}:side.txt")],
    );
    let blob_path = fx
        .repo_path
        .join("objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    assert!(blob_path.is_file(), "the pinned blob is in objects/");
    assert!(
        fx.objects_size() >= std::fs::metadata(&blob_path).unwrap().len(),
        "pinned bytes are part of the measured quota"
    );

    // Once retention elapses, the closed request keeps nothing.
    sqlx::query("UPDATE pinned_commits SET retain_until = now() - interval '1 day' WHERE pull_request_id = $1")
        .bind(pull_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    fx.age_objects();
    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(
        !fx.has_object(&head),
        "an expired pin no longer keeps a closed request's head"
    );

    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

/// The product-level half of the pin: after the branch is deleted, the pull
/// request is closed and maintenance prunes, a GET still resolves the head to
/// the pinned commit and the diff endpoint renders the changed file. The
/// storage test above proves the objects survive; this proves the read path
/// actually uses the pin (review 2, M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_closed_pull_requests_pinned_head_renders_after_its_branch_is_pruned() {
    let fx = setup("pin-api", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let (dir, head) = side_branch(&fx).await;
    let href = format!(
        "/api/v1/organizations/{}/repositories/{}/pulls",
        fx.org_slug, fx.repo_name
    );

    // The production create path writes the pin in the same transaction.
    let (status, created) = call(
        &fx,
        "POST",
        &href,
        Some(serde_json::json!({
            "title": "pin read path",
            "source_branch": "side",
            "target_branch": "main",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["head_id"].as_str(), Some(head.as_str()));
    let number = created["number"].as_i64().unwrap();
    let pull_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();

    // A request that stayed open longer than the retention window: without
    // the close-time extension its pin would already be expired.
    sqlx::query("UPDATE pinned_commits SET retain_until = now() - interval '1 day' WHERE pull_request_id = $1")
        .bind(pull_id)
        .execute(&fx.pool)
        .await
        .unwrap();

    // The branch goes, the request closes.
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "-d",
            "refs/heads/side",
        ],
    );
    let (status, closed) = call(
        &fx,
        "PATCH",
        &format!("{href}/{number}"),
        Some(serde_json::json!({ "state": "closed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["state"], "closed");
    assert_eq!(
        closed["head_id"].as_str(),
        Some(head.as_str()),
        "the close response already resolves the pin: {closed}"
    );

    // Closing extended the pin from the close, not from its last refresh.
    let extended: bool = sqlx::query_scalar(
        "SELECT bool_and(retain_until > now() + interval '29 days') FROM pinned_commits \
         WHERE pull_request_id = $1",
    )
    .bind(pull_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(extended, "the close path extends the retention window");

    // A zero-grace prune really deletes unreachable history, while the pin
    // keeps the head.
    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();
    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(!fx.has_object(&doomed), "the prune really ran");
    assert!(fx.has_object(&head), "the pinned head survives");

    // GET resolves the missing branch to the pin, not to null.
    let (status, body) = call(&fx, "GET", &format!("{href}/{number}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["head_id"].as_str(),
        Some(head.as_str()),
        "a closed pull request's head is its live pin: {body}"
    );
    assert_eq!(body["base_id"].as_str(), Some(fx.base_commit.as_str()));

    // The diff endpoint renders the pinned commit: the UI's "Files changed"
    // path uses the same base and head.
    let (status, diff) = call(
        &fx,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/compare/main...{head}",
            fx.org_slug, fx.repo_name
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert!(
        diff["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["new_path"] == "side.txt"),
        "the diff names the changed file: {diff}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

// ---------------------------------------------------------------------------
// Tests: the keep-set kinds U11 names (0046)
// ---------------------------------------------------------------------------

/// Push an anchored review comment through the API, which writes the pin in
/// the same transaction as the comment. Returns the comment's id.
async fn comment_on_head(fx: &Fixture, number: i64, head: &str) -> Uuid {
    let (status, body) = call(
        fx,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/pulls/{number}/comments",
            fx.org_slug, fx.repo_name
        ),
        Some(serde_json::json!({
            "body": "anchored",
            "commit_id": head,
            "path": "side.txt",
            "line": 1,
            "side": "new",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

/// Post a commit status through the API, which writes its pin.
async fn post_status(fx: &Fixture, commit: &str, context: &str, state: &str) {
    let (status, body) = call(
        fx,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{}/commits/{commit}/statuses",
            fx.org_slug, fx.repo_name
        ),
        Some(serde_json::json!({"context": context, "state": state})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// B1: a pull request's **base** is pinned in the target when it is created.
/// Delete the target branch, close the request and prune with zero grace: the
/// base commit and its tree survive, so `compare/base...head` still resolves
/// both sides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_closed_pull_requests_base_survives_a_prune_after_its_target_branch_is_deleted() {
    let fx = setup("keep-base", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let (dir, head) = side_branch(&fx).await;
    let href = format!(
        "/api/v1/organizations/{}/repositories/{}/pulls",
        fx.org_slug, fx.repo_name
    );
    // An unrelated base commit: history the head does not descend from and
    // that no other branch reaches, so neither the head pin nor a ref can keep
    // it. `refs/heads/trunk` is then its only ref, and deleting it leaves the
    // commit reachable only through the base pin.
    let trunk = chain_without_ref(&fx, "trunk-only.txt", "trunk\n");
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "refs/heads/trunk",
            &trunk,
        ],
    );
    let (status, created) = call(
        &fx,
        "POST",
        &href,
        Some(serde_json::json!({
            "title": "keep the base",
            "source_branch": "side",
            "target_branch": "trunk",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let number = created["number"].as_i64().unwrap();
    let pull_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let base_pin: (String, Option<String>) = sqlx::query_as(
        "SELECT commit_oid, retain_until::text FROM pinned_commits \
         WHERE base_of_pull_request_id = $1 AND reason = 'pull_request_base'",
    )
    .bind(pull_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(
        base_pin.0, trunk,
        "the base is pinned when the request opens"
    );
    assert!(
        base_pin.1.is_none(),
        "an open request's base lives with the request"
    );

    // Target branch gone, request closed, head branch gone too.
    for reference in ["refs/heads/trunk", "refs/heads/side"] {
        git_ok(
            &["--git-dir", fx.repo_path.to_str().unwrap()],
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "-d",
                reference,
            ],
        );
    }
    let (status, closed) = call(
        &fx,
        "PATCH",
        &format!("{href}/{number}"),
        Some(serde_json::json!({"state": "closed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let extended: bool = sqlx::query_scalar(
        "SELECT retain_until > now() + interval '29 days' FROM pinned_commits \
         WHERE base_of_pull_request_id = $1",
    )
    .bind(pull_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(extended, "closing starts the retention window for the base");

    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();
    let outcome = fx.run().await;
    assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
    assert!(!fx.has_object(&doomed), "the prune really ran");
    assert!(
        fx.has_object(&trunk),
        "the base commit survives with no branch pointing at it"
    );
    let base_tree = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", &format!("{trunk}^{{tree}}")],
    );
    assert!(
        fx.has_object(&base_tree),
        "the pinned base's tree survives: the diff needs it"
    );
    assert!(fx.has_object(&head), "the head pin still works as before");

    // The diff renders from the base pin once the target branch is gone.
    let (status, body) = call(&fx, "GET", &format!("{href}/{number}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["base_id"].as_str(), Some(trunk.as_str()));
    let diff = git(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["diff", "--stat", &trunk, &head],
    );
    assert!(
        diff.status.success(),
        "the pinned base still diffs against the pinned head: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    fx.fsck();

    // Retention is the bound, on the base pin alone: the head does not reach
    // the base, so the base's survival above was the base pin's doing.
    sqlx::query(
        "UPDATE pinned_commits SET retain_until = now() - interval '1 day' \
                 WHERE base_of_pull_request_id = $1",
    )
    .bind(pull_id)
    .execute(&fx.pool)
    .await
    .unwrap();
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    // Retention is the bound: once the base pin has elapsed, housekeeping no
    // longer counts the base as live. The assertion is on the pin (the
    // keep-set's input), not on Git's collector: a loose object a pack also
    // contains survives a repack at grace zero, so asserting the commit file
    // disappears would test `git repack`, not the pin.
    sqlx::query(
        "UPDATE pinned_commits SET retain_until = now() - interval '1 day' \
                 WHERE base_of_pull_request_id = $1",
    )
    .bind(pull_id)
    .execute(&fx.pool)
    .await
    .unwrap();
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    let still_pinned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pinned_commits WHERE base_of_pull_request_id = $1 \
                        AND (retain_until IS NULL OR retain_until > now()))",
    )
    .bind(pull_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(!still_pinned, "the retention bound released the base pin");

    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

/// B2: a review comment's commit is pinned while the comment exists. The
/// comment names a commit that a force push makes unreachable; with zero grace
/// and no ref to it, the commit and its tree survive a prune, and deleting the
/// comment releases the pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_review_comments_commit_survives_a_force_push_and_a_prune() {
    let fx = setup("keep-comment", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let (dir, reviewed) = side_branch(&fx).await;
    let href = format!(
        "/api/v1/organizations/{}/repositories/{}/pulls",
        fx.org_slug, fx.repo_name
    );
    let (status, created) = call(
        &fx,
        "POST",
        &href,
        Some(serde_json::json!({
            "title": "reviewed commit",
            "source_branch": "side",
            "target_branch": "main",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let number = created["number"].as_i64().unwrap();
    let comment_id = comment_on_head(&fx, number, &reviewed).await;
    let pinned: (String, String) = sqlx::query_as(
        "SELECT commit_oid, reason FROM pinned_commits WHERE review_comment_id = $1",
    )
    .bind(comment_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(pinned.0, reviewed, "the comment's commit is pinned");
    assert_eq!(pinned.1, "review_comment");

    // A force push moves the branch; the reviewed commit is now unreachable.
    // The clone checks out the default branch, so check `side` out first.
    let clone = fx.clone().await;
    git_ok(
        &[],
        &["-C", clone.to_str().unwrap(), "checkout", "--quiet", "side"],
    );
    git_ok(
        &[],
        &["-C", clone.to_str().unwrap(), "reset", "--hard", "HEAD~1"],
    );
    std::fs::write(clone.join("other.txt"), "other\n").unwrap();
    git_ok(&[], &["-C", clone.to_str().unwrap(), "add", "other.txt"]);
    git_ok(
        &[],
        &[
            "-C",
            clone.to_str().unwrap(),
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "rewritten",
        ],
    );
    let forced = tokio::task::spawn_blocking({
        let clone = clone.clone();
        move || {
            git(
                &[],
                &[
                    "-C",
                    clone.to_str().unwrap(),
                    "push",
                    "--force",
                    "origin",
                    "HEAD:side",
                ],
            )
        }
    })
    .await
    .unwrap();
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let tip = branch_tip_of(&fx, "side");
    assert_ne!(tip, reviewed, "the branch no longer reaches the review");

    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    assert!(!fx.has_object(&doomed), "the prune really ran");
    assert!(
        fx.has_object(&reviewed),
        "the commented commit survives an unreachable force push"
    );
    let tree = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", &format!("{reviewed}^{{tree}}")],
    );
    assert!(fx.has_object(&tree), "its tree survives too");

    // The pin lives with the comment: removing the comment releases it.
    sqlx::query("DELETE FROM pull_request_comments WHERE id = $1")
        .bind(comment_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pinned_commits WHERE review_comment_id = $1")
            .bind(comment_id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "the pin cascades with its comment");
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    assert!(
        !fx.has_object(&reviewed),
        "without the comment nothing pins the commit"
    );

    let _ = std::fs::remove_dir_all(&clone);
    let _ = std::fs::remove_dir_all(&dir);
    teardown(fx).await;
}

/// B3: a status pins its commit while it is the latest result for its context.
/// A status on a commit no ref reaches survives a zero-grace prune; when a
/// later status of the same context supersedes it, its pin gains the retention
/// timestamp and stops keeping the ancestry of an unreachable commit forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_statuses_commit_survives_a_prune_until_it_is_superseded_and_retention_elapses() {
    let fx = setup("keep-status", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let orphan = commit_without_ref(&fx, "ci.txt", "ci\n");
    post_status(&fx, &orphan, "ci/build", "success").await;
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT p.retain_until::text FROM pinned_commits p \
         JOIN commit_statuses s ON s.id = p.commit_status_id \
         WHERE s.commit_id = $1 AND s.context = 'ci/build' \
         ORDER BY s.seq DESC LIMIT 1",
    )
    .bind(&orphan)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(
        latest.is_none(),
        "the latest status of a context lives with its row"
    );

    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    assert!(!fx.has_object(&doomed), "the prune really ran");
    assert!(
        fx.has_object(&orphan),
        "the reported commit survives with no ref reaching it"
    );
    let tree = git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", &format!("{orphan}^{{tree}}")],
    );
    assert!(fx.has_object(&tree));

    // A newer result for the same context supersedes it: the old pin gets the
    // retention window instead of living forever.
    post_status(&fx, &orphan, "ci/build", "failure").await;
    let superseded: Option<String> = sqlx::query_scalar(
        "SELECT p.retain_until::text FROM pinned_commits p \
         JOIN commit_statuses s ON s.id = p.commit_status_id \
         WHERE s.commit_id = $1 AND s.context = 'ci/build' \
         ORDER BY s.seq ASC LIMIT 1",
    )
    .bind(&orphan)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(
        superseded.is_some(),
        "a superseded status's pin expires: {superseded:?}"
    );
    let newest_live: bool = sqlx::query_scalar(
        "SELECT p.retain_until IS NULL FROM pinned_commits p \
         JOIN commit_statuses s ON s.id = p.commit_status_id \
         WHERE s.commit_id = $1 AND s.context = 'ci/build' \
         ORDER BY s.seq DESC LIMIT 1",
    )
    .bind(&orphan)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert!(newest_live, "the new latest status is live");

    // Elapse the superseded window and release both (the new latest lives with
    // its row; elapsing it here proves the bound is a timestamp, not a ref).
    sqlx::query(
        "UPDATE pinned_commits p SET retain_until = now() - interval '1 day' \
                 FROM commit_statuses s \
                 WHERE s.id = p.commit_status_id AND s.commit_id = $1",
    )
    .bind(&orphan)
    .execute(&fx.pool)
    .await
    .unwrap();
    fx.age_objects();
    assert!(matches!(fx.run().await, Outcome::Completed(_)));
    assert!(
        !fx.has_object(&orphan),
        "past retention a status no longer pins its commit"
    );

    teardown(fx).await;
}

fn branch_tip_of(fx: &Fixture, branch: &str) -> String {
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &["rev-parse", &format!("refs/heads/{branch}")],
    )
}

// ---------------------------------------------------------------------------
// Tests: crash safety
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn an_interrupted_run_at_every_step_is_finished_by_the_next_one() {
    for point in [
        FaultPoint::BeforeRepack,
        FaultPoint::AfterRepack,
        FaultPoint::DuringPrune,
        FaultPoint::BeforeVerification,
    ] {
        let fx = setup("fault", |config| {
            config.housekeeping_grace_seconds = 0;
        })
        .await;

        // Unreachable history to delete, plus reachable history to preserve.
        let clone = fx.clone().await;
        let output = commit_and_push(&clone, "reachable.txt", "kept\n").await;
        assert!(output.status.success());
        let reachable = git_ok(
            &["--git-dir", fx.repo_path.to_str().unwrap()],
            &["rev-parse", "refs/heads/main"],
        );
        let dangling = commit_without_ref(&fx, "dangling.txt", "gone\n");

        housekeeping::set_fault(fx.repo_id, point);
        let error = fx.run_expecting_error().await;
        assert!(error.contains(point.as_str()), "{point:?}: {error}");

        // Whatever the step, reachable history is intact and Git is happy.
        assert!(fx.has_object(&reachable), "{point:?}");
        assert!(fx.has_object(&fx.base_commit), "{point:?}");
        fx.fsck();
        let intact = fx.clone().await;
        let _ = std::fs::remove_dir_all(&intact);

        // The injected fault was one-shot: the restart finishes the work.
        let outcome = fx.run().await;
        let Outcome::Completed(report) = outcome else {
            panic!("{point:?}: expected completion, got {outcome:?}");
        };
        assert!(report.repacked && report.pruned, "{point:?}");
        assert!(fx.has_object(&reachable), "{point:?}");
        assert!(
            !fx.has_object(&dangling),
            "{point:?}: the deletion completes"
        );
        fx.fsck();
        let clone_after = fx.clone().await;
        assert_eq!(
            git_ok(
                &["-C", clone_after.to_str().unwrap()],
                &["rev-parse", "HEAD"]
            ),
            reachable,
            "{point:?}"
        );
        let _ = std::fs::remove_dir_all(&clone_after);
        let _ = std::fs::remove_dir_all(&clone);
        let (_, _, _, outcome) = fx.housekeeping_row().await;
        assert_eq!(outcome, "completed", "{point:?}");
        teardown(fx).await;
    }
}

/// Create a commit in the bare repository that no ref reaches.
fn commit_without_ref(fx: &Fixture, name: &str, content: &str) -> String {
    let dir = std::env::temp_dir().join(format!("githearth-hk-dangling-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let work = dir.to_str().unwrap();
    git_ok(
        &[],
        &[
            "clone",
            "--quiet",
            "--",
            fx.repo_path.to_str().unwrap(),
            work,
        ],
    );
    std::fs::write(dir.join(name), content).unwrap();
    git_ok(&[], &["-C", work, "add", name]);
    git_ok(
        &[],
        &[
            "-C",
            work,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "dangling",
        ],
    );
    let oid = git_ok(&[], &["-C", work, "rev-parse", "HEAD"]);
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--",
            work,
            "HEAD:refs/heads/dangling",
        ],
    );
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "-d",
            "refs/heads/dangling",
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    oid
}

/// One commit in the bare repository, a child of `parent`, reachable from no
/// ref. Used as a base commit that the head does not descend from.
fn chain_without_ref(fx: &Fixture, name: &str, content: &str) -> String {
    let dir = std::env::temp_dir().join(format!("githearth-hk-chain-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let work = dir.to_str().unwrap();
    git_ok(
        &[],
        &[
            "clone",
            "--quiet",
            "--",
            fx.repo_path.to_str().unwrap(),
            work,
        ],
    );
    // An orphan branch: the commit shares no history with `main` or `side`.
    git_ok(
        &[],
        &["-C", work, "checkout", "--quiet", "--orphan", "trunk-only"],
    );
    // Start from nothing: an orphan branch keeps the index of the branch it
    // left, so clear it before the first commit or this commit is `main`'s
    // with another name.
    git_ok(&[], &["-C", work, "rm", "-r", "--quiet", "--cached", "."]);
    std::fs::write(dir.join(name), content).unwrap();
    git_ok(&[], &["-C", work, "add", name]);
    git_ok(
        &[],
        &[
            "-C",
            work,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "orphan",
        ],
    );
    let oid = git_ok(&[], &["-C", work, "rev-parse", "HEAD"]);
    // Copy the object without a ref in the bare repository: `fetch` would
    // otherwise name it after `HEAD`'s branch.
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--quiet",
            "--",
            work,
            "trunk-only",
        ],
    );
    git_ok(
        &["--git-dir", fx.repo_path.to_str().unwrap()],
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "-d",
            "refs/heads/trunk-only",
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    oid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn repeated_and_concurrent_runs_are_harmless() {
    let fx = setup("repeat", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let clone = fx.clone().await;
    let output = commit_and_push(&clone, "one.txt", "one\n").await;
    assert!(output.status.success());

    // Two runs at once on one repository: the lock serializes them and both
    // complete, changing nothing after the first.
    let first = tokio::spawn({
        let state = fx.state.clone();
        let repo_id = fx.repo_id;
        async move { housekeeping::run_repository(&state, repo_id).await }
    });
    let second = tokio::spawn({
        let state = fx.state.clone();
        let repo_id = fx.repo_id;
        async move { housekeeping::run_repository(&state, repo_id).await }
    });
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    let size = fx.objects_size();
    for _ in 0..3 {
        let outcome = fx.run().await;
        assert!(matches!(outcome, Outcome::Completed(_)), "{outcome:?}");
        assert_eq!(fx.objects_size(), size, "a repeated run changes nothing");
    }
    fx.fsck();
    let _ = std::fs::remove_dir_all(&clone);
    teardown(fx).await;
}

// ---------------------------------------------------------------------------
// Tests: eligibility, the durable job and configuration
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn archived_inactive_and_disabled_repositories_are_skipped() {
    let fx = setup("skip", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;

    // Archived: no writes can happen, so it is skipped.
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1")
        .bind(fx.repo_id)
        .execute(&fx.pool)
        .await
        .unwrap();
    assert!(matches!(fx.run().await, Outcome::Skipped(reason) if reason.contains("archived")));
    sqlx::query("UPDATE repositories SET archived = false WHERE id = $1")
        .bind(fx.repo_id)
        .execute(&fx.pool)
        .await
        .unwrap();

    // A repository that does not exist is skipped, not an error.
    let missing = housekeeping::run_repository(&fx.state, Uuid::new_v4())
        .await
        .unwrap();
    assert!(matches!(missing, Outcome::Skipped(_)));

    // A repository that is being deleted is skipped.
    let doomed = create_active_repo(&fx.pool, fx.org_id, &tag("doomed"), fx.owner).await;
    fx.state
        .storage
        .create(doomed, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(doomed)
        .execute(&fx.pool)
        .await
        .unwrap();
    let outcome = housekeeping::run_repository(&fx.state, doomed)
        .await
        .unwrap();
    assert!(
        matches!(outcome, Outcome::Skipped(reason) if reason.contains("not active")),
        "{outcome:?}"
    );

    // Disabled by configuration: nothing runs, so a job left over is a no-op.
    let mut config = fx.config.clone();
    config.housekeeping_enabled = false;
    let disabled = AppState::new(fx.pool.clone(), config).await.unwrap();
    let outcome = housekeeping::run_repository(&disabled, fx.repo_id)
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Skipped(reason) if reason.contains("disabled")));
    assert_eq!(
        housekeeping::enqueue_due(&disabled).await.unwrap(),
        0,
        "a disabled scheduler enqueues nothing"
    );

    fx.fsck();
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn the_durable_job_runs_once_per_repository_and_records_its_result() {
    let fx = setup("job", |config| {
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_concurrency = 2;
        config.housekeeping_interval_seconds = 60;
    })
    .await;

    // Two enqueue passes for the same repository produce one job: the dedupe
    // key is the repository id.
    assert_eq!(housekeeping::enqueue_due(&fx.state).await.unwrap(), 1);
    assert_eq!(housekeeping::enqueue_due(&fx.state).await.unwrap(), 0);
    let jobs: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, kind FROM jobs WHERE dedupe_key = $1")
            .bind(fx.repo_id.to_string())
            .fetch_all(&fx.pool)
            .await
            .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].1, housekeeping::JOB_KIND);

    // The real runner claims it and the handler maintains the repository.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    wait_for_job_state(&fx.pool, jobs[0].0, "done").await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    let (_, _, _, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    // The scheduler does not enqueue it again within the interval.
    assert_eq!(housekeeping::enqueue_due(&fx.state).await.unwrap(), 0);
    assert!(
        fx.audit_kinds()
            .await
            .iter()
            .any(|k| k == housekeeping::JOB_KIND)
    );
    fx.fsck();
    teardown(fx).await;
}

/// B1 regression: the generic `job_timeout_seconds` (1 s here) is shorter than
/// one housekeeping step. The kind's own budget (four steps plus slack) must
/// keep the handler alive, so a run longer than the generic timeout still
/// records a completed row instead of timing out, leaving `last_run_at` NULL
/// and re-enqueueing forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_run_longer_than_the_generic_job_timeout_still_records_completed() {
    let fx = setup("slow-job", |config| {
        config.housekeeping_grace_seconds = 0;
        // The blocker: shorter than the deliberately slow step below.
        config.job_timeout_seconds = 1;
    })
    .await;
    housekeeping::set_slow(fx.repo_id, Duration::from_secs(2));

    let mut conn = fx.pool.acquire().await.unwrap();
    githearth_server::jobs::enqueue(
        &mut conn,
        Some(fx.org_id),
        housekeeping::JOB_KIND,
        serde_json::json!({ "repository_id": fx.repo_id }),
        &fx.repo_id.to_string(),
        None,
    )
    .await
    .unwrap();
    drop(conn);
    let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE dedupe_key = $1")
        .bind(fx.repo_id.to_string())
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let started = std::time::Instant::now();
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    wait_for_job_state(&fx.pool, job, "done").await;
    let elapsed = started.elapsed();
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    assert!(
        elapsed > Duration::from_secs(1),
        "the run must outlast the generic job timeout: {elapsed:?}"
    );
    let (attempt, has_result): (i32, bool) = sqlx::query_as(
        "SELECT j.attempt, h.last_run_at IS NOT NULL FROM jobs j \
         LEFT JOIN repository_housekeeping h ON h.repository_id = $2 WHERE j.id = $1",
    )
    .bind(job)
    .bind(fx.repo_id)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    assert_eq!(attempt, 1, "the run was not timed out and retried");
    assert!(has_result);
    let (_, _, _, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    fx.fsck();
    teardown(fx).await;
}

async fn wait_for_job_state(pool: &PgPool, job: Uuid, expected: &str) {
    wait_for_job_state_for(pool, job, expected, Duration::from_secs(30)).await
}

async fn wait_for_job_state_for(pool: &PgPool, job: Uuid, expected: &str, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(pool)
            .await
            .unwrap();
        if state == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let dump: Vec<(Uuid, String, i32, Option<String>, String, String)> =
                sqlx::query_as("SELECT id, kind, attempt, last_error, state, dedupe_key FROM jobs")
                    .fetch_all(pool)
                    .await
                    .unwrap();
            panic!("job {job} stayed {state}, never reached {expected}; rows={dump:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_failed_run_is_recorded_and_retried_by_the_job() {
    let fx = setup("retry", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    housekeeping::set_fault(fx.repo_id, FaultPoint::DuringPrune);
    // Enqueue the job directly: this simulates the scheduler's enqueue, then
    // the fault makes the first attempt fail.
    let mut conn = fx.pool.acquire().await.unwrap();
    githearth_server::jobs::enqueue(
        &mut conn,
        Some(fx.org_id),
        housekeeping::JOB_KIND,
        serde_json::json!({ "repository_id": fx.repo_id }),
        &fx.repo_id.to_string(),
        None,
    )
    .await
    .unwrap();
    drop(conn);
    let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE dedupe_key = $1")
        .bind(fx.repo_id.to_string())
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    wait_for_job_state(&fx.pool, job, "done").await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    // The first attempt failed, was recorded, and the retry completed.
    let (attempts, last_error): (i32, Option<String>) =
        sqlx::query_as("SELECT attempt, last_error FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(attempts >= 2, "the job retried: {attempts}");
    assert!(last_error.is_none() || last_error.as_deref() != Some("lease expired"));
    let (_, _, _, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    fx.fsck();
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_starved_scan_is_truncated_and_the_next_run_finishes() {
    let fx = setup("starved-scan", |config| {
        config.housekeeping_grace_seconds = 0;
    })
    .await;
    let clone = fx.clone().await;
    let output = commit_and_push(&clone, "kept.txt", "kept\n").await;
    assert!(output.status.success());
    let doomed = commit_without_ref(&fx, "doomed.txt", "gone\n");
    fx.age_objects();

    // The scan has no time left: it must not report a prune.
    housekeeping::set_scan_budget(fx.repo_id, Duration::ZERO);
    let outcome = fx.run().await;
    let Outcome::Completed(report) = outcome else {
        panic!("expected a completed run, got {outcome:?}");
    };
    assert!(report.repacked, "{report:?}");
    assert!(
        report.truncated,
        "the run says it stopped early: {report:?}"
    );
    assert!(!report.pruned, "a starved scan must not claim a prune");
    assert!(fx.has_object(&doomed), "nothing was deleted");
    assert!(fx.has_object(&fx.base_commit));

    // The next run has its own deadline and finishes the work.
    let outcome = fx.run().await;
    let Outcome::Completed(report) = outcome else {
        panic!("expected a completed run, got {outcome:?}");
    };
    assert!(report.pruned && !report.truncated, "{report:?}");
    assert!(!fx.has_object(&doomed));
    fx.fsck();
    let _ = std::fs::remove_dir_all(&clone);
    teardown(fx).await;
}

/// The budget covers every phase: a run longer than the old four-timeout
/// figure still records `completed` under the kind's own budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn a_run_longer_than_four_step_timeouts_still_records_completed() {
    let fx = setup("long-run", |config| {
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_timeout_seconds = 10;
    })
    .await;
    // 41 s > 4 * 10 s, and also longer than the generic 30 s job timeout.
    housekeeping::set_slow(fx.repo_id, Duration::from_secs(41));

    let mut conn = fx.pool.acquire().await.unwrap();
    githearth_server::jobs::enqueue(
        &mut conn,
        Some(fx.org_id),
        housekeeping::JOB_KIND,
        serde_json::json!({ "repository_id": fx.repo_id }),
        &fx.repo_id.to_string(),
        None,
    )
    .await
    .unwrap();
    drop(conn);
    let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE dedupe_key = $1")
        .bind(fx.repo_id.to_string())
        .fetch_one(&fx.pool)
        .await
        .unwrap();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let started = std::time::Instant::now();
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    wait_for_job_state_for(&fx.pool, job, "done", Duration::from_secs(90)).await;
    let elapsed = started.elapsed();
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    assert!(
        elapsed > Duration::from_secs(40),
        "the run must outlast four step timeouts: {elapsed:?}"
    );
    let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(attempt, 1, "the kind budget kept the run alive");
    let (_, _, _, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    fx.fsck();
    teardown(fx).await;
}

/// Direct enqueues (an unarchive, an operator follow-up) bypass the
/// scheduler's allowance; the run-side semaphore still bounds how many Git
/// runs overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn direct_enqueues_do_not_exceed_housekeeping_concurrency() {
    let fx = setup("kind-limit", |config| {
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_concurrency = 1;
        config.job_concurrency = 4;
    })
    .await;
    let other = create_active_repo(&fx.pool, fx.org_id, &tag("other"), fx.owner).await;
    fx.state
        .storage
        .create(other, Uuid::new_v4(), ObjectFormat::Sha1)
        .unwrap();
    housekeeping::set_slow(fx.repo_id, Duration::from_millis(800));
    housekeeping::set_slow(other, Duration::from_millis(800));

    let mut conn = fx.pool.acquire().await.unwrap();
    let mut jobs = Vec::new();
    for repository_id in [fx.repo_id, other] {
        githearth_server::jobs::enqueue(
            &mut conn,
            Some(fx.org_id),
            housekeeping::JOB_KIND,
            serde_json::json!({ "repository_id": repository_id }),
            &repository_id.to_string(),
            None,
        )
        .await
        .unwrap();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE dedupe_key = $1")
            .bind(repository_id.to_string())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        jobs.push(id);
    }
    drop(conn);

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let started = std::time::Instant::now();
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    for job in jobs {
        wait_for_job_state(&fx.pool, job, "done").await;
    }
    let elapsed = started.elapsed();
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    assert!(
        elapsed >= Duration::from_millis(1400),
        "two 800 ms runs at concurrency 1 must serialize: {elapsed:?}"
    );
    for repository_id in [fx.repo_id, other] {
        let outcome: String = sqlx::query_scalar(
            "SELECT last_outcome FROM repository_housekeeping WHERE repository_id = $1",
        )
        .bind(repository_id)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!(outcome, "completed");
    }
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn the_scheduler_enqueues_due_repositories_and_a_bad_payload_is_terminal() {
    let fx = setup("scheduler", |config| {
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_interval_seconds = 60;
    })
    .await;

    // The scheduler task enqueues the repository once and then leaves it
    // alone while the job is pending.
    let scheduler = housekeeping::spawn(fx.state.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let job: Uuid = loop {
        let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE dedupe_key = $1")
            .bind(fx.repo_id.to_string())
            .fetch_optional(&fx.pool)
            .await
            .unwrap();
        if let Some(id) = found {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scheduler never enqueued the due repository"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    scheduler.abort();

    // A payload without a repository id is a permanent failure, not a retry
    // loop: the enqueuer's data is wrong.
    let bad = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, dedupe_key) VALUES ($1, $2, '{}'::jsonb, 'bad')",
    )
    .bind(bad)
    .bind(housekeeping::JOB_KIND)
    .execute(&fx.pool)
    .await
    .unwrap();

    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut registry = githearth_server::jobs::Registry::new();
    housekeeping::register(&mut registry, &fx.config);
    let runner = githearth_server::jobs::spawn(fx.state.clone(), registry, shutdown.clone());
    wait_for_job_state(&fx.pool, bad, "dead").await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;

    let last_error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM jobs WHERE id = $1")
            .bind(bad)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(
        last_error.unwrap_or_default().contains("repository_id"),
        "a bad payload names the missing field"
    );
    // The job the scheduler enqueued is independent of the malformed one and
    // completed normally.
    let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(state, "done");
    let (_, _, _, outcome) = fx.housekeeping_row().await;
    assert_eq!(outcome, "completed");
    teardown(fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL"]
async fn concurrency_bounds_how_many_repositories_are_queued() {
    let fx = setup("bound", |config| {
        config.housekeeping_grace_seconds = 0;
        config.housekeeping_interval_seconds = 60;
        config.housekeeping_concurrency = 1;
    })
    .await;
    // Three more eligible repositories exist; only one run may be queued.
    for _ in 0..3 {
        let id = create_active_repo(&fx.pool, fx.org_id, &tag("extra"), fx.owner).await;
        fx.state
            .storage
            .create(id, Uuid::new_v4(), ObjectFormat::Sha1)
            .unwrap();
    }
    assert_eq!(housekeeping::enqueue_due(&fx.state).await.unwrap(), 1);
    assert_eq!(
        housekeeping::enqueue_due(&fx.state).await.unwrap(),
        0,
        "the in-flight run fills the allowance"
    );
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(housekeeping::JOB_KIND)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
    assert_eq!(queued, 1);
    teardown(fx).await;
}
