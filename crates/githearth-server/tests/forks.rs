//! Fork and cross-repository pull request contract tests (PostgreSQL-backed).
//!
//! Each fixture builds a private parent repository (with `main` and `feature`),
//! a fork of it in a contributor's own organization, and a fresh commit on the
//! fork's `feature` branch. Tests exercise forking, cross-repository pull
//! requests, merges by each method, stale-approval dismissal on a fork push, the
//! untrusted-contributor policy, and fork deletion closing its open pulls.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::merge::{MergeOutcome, execute_intent};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn git_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(args);
    cmd
}

async fn git_run(mut cmd: Command) -> std::process::Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::task::spawn_blocking(move || cmd.output()),
    )
    .await
    .expect("git command timed out")
    .expect("spawn_blocking join error")
    .expect("git command failed to launch")
}

async fn git_ok(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn git_fail(cmd: Command) {
    let out = git_run(cmd).await;
    assert!(!out.status.success(), "git unexpectedly succeeded");
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

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn request_with_idempotency(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    idem_key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = idem_key {
        builder = builder.header("Idempotency-Key", key);
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.status()
}

async fn get_body(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn find_port_and_serve(app: &Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    port
}

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

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

async fn fixture_session(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
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

async fn grant_read(pool: &PgPool, account: Uuid, organization: Uuid) {
    // A grant's account subject must be a member of the scope organization.
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(organization)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(account)
        .bind(organization)
        .execute(pool)
        .await
        .unwrap();
}

struct Fixture {
    app: Router,
    state: AppState,
    pool: PgPool,
    storage_root: PathBuf,
    parent_org_slug: String,
    parent_org_id: Uuid,
    contrib_org_slug: String,
    owner_token: String,
    contributor_token: String,
    contributor_username: String,
    stranger_token: String,
    stranger_username: String,
    parent_repo_name: String,
    parent_repo_id: Uuid,
    parent_repo_path: PathBuf,
    fork_name: String,
    fork_id: Uuid,
    fork_path: PathBuf,
    main_head: String,
    fork_feature_head: String,
    work: PathBuf,
    number: i64,
}

fn repo_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/pulls")
}

async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-forks-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: storage_root.clone(),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_githearth-server")),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();

    let owner = fixture_account(&pool, &tag("owner")).await;
    set_password(&pool, owner, "testpass").await;
    let contributor = fixture_account(&pool, &tag("contributor")).await;
    set_password(&pool, contributor, "testpass").await;
    let stranger = fixture_account(&pool, &tag("stranger")).await;
    set_password(&pool, stranger, "testpass").await;
    let contributor_username: String =
        sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
            .bind(contributor)
            .fetch_one(&pool)
            .await
            .unwrap();
    let stranger_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(stranger)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (parent_org_id, parent_org_slug) = fixture_organization(&pool, owner).await;
    let (_contrib_org_id, contrib_org_slug) = fixture_organization(&pool, contributor).await;
    // The contributor can read the (private) parent, so they may fork it and
    // open pull requests, but they get no write grant.
    grant_read(&pool, contributor, parent_org_id).await;

    let owner_token = fixture_session(&pool, owner).await;
    let contributor_token = fixture_session(&pool, contributor).await;
    let stranger_token = fixture_session(&pool, stranger).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let parent_repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{parent_org_slug}/repositories"),
        Some(&owner_token),
        json!({"name": parent_repo_name, "visibility": "private"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let parent_repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let parent_repo_path = state_repo_path(&storage_root, parent_repo_id);

    // Seed the parent: root commit on main, a feature commit on a branch.
    git_ok(git_cmd(&[
        "--git-dir",
        parent_repo_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;
    let work = std::env::temp_dir().join(format!("git-forks-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "root",
    ]))
    .await;
    let main_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "-b", "feature"])).await;
    std::fs::write(work.join("feat.txt"), "feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feat.txt"])).await;
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
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        parent_repo_path.to_str().unwrap(),
        "main",
        "feature",
    ]))
    .await;

    // Contributor forks the private parent into their own organization.
    let fork_name = tag("fork");
    let (status, fork) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{parent_org_slug}/repositories/{parent_repo_name}/forks"),
        Some(&contributor_token),
        json!({"organization": contrib_org_slug, "name": fork_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id: Uuid = fork["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        fork["visibility"], "private",
        "private parent yields private fork"
    );
    assert_eq!(
        fork["parent_repository_id"].as_str().unwrap(),
        parent_repo_id.to_string()
    );
    let fork_path = state_repo_path(&storage_root, fork_id);

    // Contributor advances the fork's feature branch with their own commit.
    git_ok(git_cmd(&[
        "--git-dir",
        fork_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(work.join("contribution.txt"), "contributed\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "contribution.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=C",
        "-c",
        "user.email=c@c",
        "commit",
        "-m",
        "contribution",
    ]))
    .await;
    let fork_feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        fork_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    // Contributor opens a cross-repository pull request: fork/feature -> main.
    let source_repository = format!("{contrib_org_slug}/{fork_name}");
    let (status, pull) = request(
        &app,
        "POST",
        &repo_href(&parent_org_slug, &parent_repo_name),
        Some(&contributor_token),
        json!({
            "title": "contribution",
            "source_branch": "feature",
            "target_branch": "main",
            "source_repository": source_repository,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();

    Fixture {
        app,
        state,
        pool,
        storage_root,
        parent_org_slug,
        parent_org_id,
        contrib_org_slug,
        owner_token,
        contributor_token,
        contributor_username,
        stranger_token,
        stranger_username,
        parent_repo_name,
        parent_repo_id,
        parent_repo_path,
        fork_name,
        fork_id,
        fork_path,
        main_head,
        fork_feature_head,
        work,
        number,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

async fn merge_href(f: &Fixture, number: i64) -> String {
    format!(
        "/api/v1/organizations/{}/repositories/{}/pulls/{number}/merge",
        f.parent_org_slug, f.parent_repo_name
    )
}

async fn create_merge_intent(f: &Fixture, method: &str, head: &str) -> (StatusCode, Value) {
    request(
        &f.app,
        "POST",
        &merge_href(f, f.number).await,
        Some(&f.owner_token),
        json!({"method": method, "expected_head_oid": head}),
    )
    .await
}

async fn set_intent_executing(pool: &PgPool, operation_id: Uuid) {
    sqlx::query("UPDATE merge_intents SET state = 'executing' WHERE operation_id = $1")
        .bind(operation_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn main_tip(f: &Fixture) -> String {
    git_output(git_cmd(&[
        "--git-dir",
        f.parent_repo_path.to_str().unwrap(),
        "rev-parse",
        "main",
    ]))
    .await
}

fn expect_merged(outcome: MergeOutcome) -> String {
    match outcome {
        MergeOutcome::Merged { oid } => oid,
        other => panic!("expected Merged, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_pull_requests_merge_by_each_method() {
    for method in ["merge", "squash", "rebase", "ff_only"] {
        let f = build_fixture().await;
        let (status, intent) = create_merge_intent(&f, method, &f.fork_feature_head).await;
        assert_eq!(status, StatusCode::CREATED, "method {method}: {intent}");
        let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
        set_intent_executing(&f.pool, op).await;

        let oid = expect_merged(execute_intent(&f.state, op).await.unwrap());
        assert_eq!(main_tip(&f).await, oid, "method {method} advanced main");

        let pull_id: Uuid =
            sqlx::query_scalar("SELECT pull_request_id FROM merge_intents WHERE operation_id = $1")
                .bind(op)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        let pull_state: String =
            sqlx::query_scalar("SELECT state FROM pull_requests WHERE id = $1")
                .bind(pull_id)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(
            pull_state, "merged",
            "method {method} merged the pull request"
        );

        finish(f).await;
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_author_without_target_permission_cannot_merge() {
    let f = build_fixture().await;
    let (status, _body) = request(
        &f.app,
        "POST",
        &merge_href(&f, f.number).await,
        Some(&f.contributor_token),
        json!({"method": "merge", "expected_head_oid": f.fork_feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The target main must not move.
    assert_eq!(main_tip(&f).await, f.main_head);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn push_to_fork_updates_pull_head_and_dismisses_stale_approvals() {
    let f = build_fixture().await;

    // The parent owner approves at the current fork head.
    let (status, _) = request(
        &f.app,
        "POST",
        &format!(
            "{}/{}/reviews",
            repo_href(&f.parent_org_slug, &f.parent_repo_name),
            f.number
        ),
        Some(&f.owner_token),
        json!({"verdict": "approved", "commit_id": f.fork_feature_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, detail) = request(
        &f.app,
        "GET",
        &format!(
            "{}/{}",
            repo_href(&f.parent_org_slug, &f.parent_repo_name),
            f.number
        ),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["approvals"], 1);

    // Contributor pushes another commit to the fork's feature branch.
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("more.txt"), "more\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "more.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=C",
        "-c",
        "user.email=c@c",
        "commit",
        "-m",
        "more",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        f.fork_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    // The head moved, so the earlier approval is stale.
    let (status, detail) = request(
        &f.app,
        "GET",
        &format!(
            "{}/{}",
            repo_href(&f.parent_org_slug, &f.parent_repo_name),
            f.number
        ),
        Some(&f.owner_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["approvals"], 0);
    assert_ne!(detail["head_id"].as_str().unwrap(), f.fork_feature_head);

    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_author_without_target_permission_cannot_push_to_target() {
    let f = build_fixture().await;
    let port = find_port_and_serve(&f.app).await;
    let tw = f.work.to_str().unwrap();
    // The contributor's work tree still has `feature` checked out; a push of it
    // to the parent's `main` must be refused by the server's ref admission.
    let url = format!(
        "http://{}:testpass@127.0.0.1:{port}/{}/{}.git",
        f.contributor_username, f.parent_org_slug, f.parent_repo_name
    );
    git_fail(git_cmd(&["-C", tw, "push", "--", &url, "feature:main"])).await;
    // The target main must not move.
    assert_eq!(main_tip(&f).await, f.main_head);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn private_fork_is_hidden_from_unrelated_users_over_api_ui_and_git_http() {
    let f = build_fixture().await;

    // API: a stranger cannot read the private fork.
    let fork_read = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.contrib_org_slug, f.fork_name
    );
    let (status, _) = request(
        &f.app,
        "GET",
        &fork_read,
        Some(&f.stranger_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // UI: a stranger's browser session cannot open the fork page.
    let stranger_cookie = format!("githearth_session={}", f.stranger_token);
    let ui = format!(
        "/organizations/{}/repositories/{}",
        f.contrib_org_slug, f.fork_name
    );
    assert_eq!(
        get(&f.app, &ui, Some(&stranger_cookie)).await,
        StatusCode::NOT_FOUND
    );

    // Git HTTP: a stranger's clone/fetch is refused (the repository is not
    // visible to them).
    let port = find_port_and_serve(&f.app).await;
    let url = format!(
        "http://{}:testpass@127.0.0.1:{port}/{}/{}.git",
        f.stranger_username, f.contrib_org_slug, f.fork_name
    );
    let clone_dir = std::env::temp_dir().join(format!("git-forks-clone-{}", Uuid::new_v4()));
    git_fail(git_cmd(&["clone", "--", &url, clone_dir.to_str().unwrap()])).await;
    let _ = std::fs::remove_dir_all(&clone_dir);

    // The fork owner can still read it.
    let (status, body) = request(
        &f.app,
        "GET",
        &fork_read,
        Some(&f.contributor_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleting_the_fork_closes_its_open_pull_requests() {
    let f = build_fixture().await;

    // A queued merge intent for the pull: fork deletion must cancel it.
    let (status, intent) = create_merge_intent(&f, "merge", &f.fork_feature_head).await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let operation_id: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();

    let closed_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE kind = 'pull_request.closed' AND organization_id = $1",
    )
    .bind(f.parent_org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(closed_before, 0);

    // Delete the fork via the API.
    let (status, _) = request(
        &f.app,
        "DELETE",
        &format!(
            "/api/v1/organizations/{}/repositories/{}",
            f.contrib_org_slug, f.fork_name
        ),
        Some(&f.contributor_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let state: String = sqlx::query_scalar(
        "SELECT state FROM pull_requests WHERE repository_id = $1 AND number = $2",
    )
    .bind(f.parent_repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(state, "closed");
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT closed_reason FROM pull_requests WHERE repository_id = $1 AND number = $2",
    )
    .bind(f.parent_repo_id)
    .bind(f.number as i32)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("source repository deleted"));

    // The system close cancels the queued merge intent, like an author close.
    let intent_state: String =
        sqlx::query_scalar("SELECT state FROM merge_intents WHERE operation_id = $1")
            .bind(operation_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(intent_state, "cancelled");

    // And it leaves a per-pull audit trail.
    let closed_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events \
         WHERE kind = 'pull_request.closed' AND organization_id = $1",
    )
    .bind(f.parent_org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(closed_after, 1);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn source_repository_fork_status_is_not_leaked_without_read_access() {
    let f = build_fixture().await;

    // Give the stranger read access to the parent (so they can open a pull
    // request) but not to the contributor's organization.
    let stranger_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&f.stranger_username)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(f.parent_org_id)
    .bind(stranger_id)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by) \
         VALUES ($1, 'read', 'account', $2, 'organization', $3, $2)",
    )
    .bind(Uuid::new_v4())
    .bind(stranger_id)
    .bind(f.parent_org_id)
    .execute(&f.pool)
    .await
    .unwrap();

    // A second, non-fork private repository in the contributor's organization.
    let other_name = tag("other");
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.contrib_org_slug),
        Some(&f.contributor_token),
        json!({"name": other_name, "visibility": "private"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A source that is not a fork (and that the stranger cannot read) must look
    // exactly like a source that does not exist: both are 404, so the fork
    // relationship cannot be enumerated without read access.
    let (status, body) = request(
        &f.app,
        "POST",
        &repo_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.stranger_token),
        json!({
            "title": "probe",
            "source_branch": "feature",
            "target_branch": "main",
            "source_repository": format!("{}/{}", f.contrib_org_slug, other_name),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _) = request(
        &f.app,
        "POST",
        &repo_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.stranger_token),
        json!({
            "title": "probe",
            "source_branch": "feature",
            "target_branch": "main",
            "source_repository": format!("{}/does-not-exist", f.contrib_org_slug),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_form_lists_only_namespaces_the_viewer_can_create_repositories_in() {
    let f = build_fixture().await;

    // The contributor owns contrib_org (so they can create repositories there)
    // but has only read access to parent_org, so the fork form must offer only
    // contrib_org and never parent_org.
    let cookie = format!("githearth_session={}", f.contributor_token);
    let path = format!(
        "/organizations/{}/repositories/{}",
        f.parent_org_slug, f.parent_repo_name
    );
    let (status, body) = get_body(&f.app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("<option value=\"{}\">", f.contrib_org_slug)));
    assert!(!body.contains(&format!("<option value=\"{}\">", f.parent_org_slug)));

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reopening_a_pull_request_clears_its_closed_reason() {
    let f = build_fixture().await;

    // Simulate a system close (as fork deletion would) directly, so the fork
    // still exists and the pull request can be reopened through the API.
    let pull_id: Uuid =
        sqlx::query_scalar("SELECT id FROM pull_requests WHERE repository_id = $1 AND number = $2")
            .bind(f.parent_repo_id)
            .bind(f.number as i32)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE pull_requests SET state = 'closed', closed_at = now(), \
         closed_reason = 'source repository deleted', updated_at = now() WHERE id = $1",
    )
    .bind(pull_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let (status, _) = request(
        &f.app,
        "PATCH",
        &format!(
            "{}/{}",
            repo_href(&f.parent_org_slug, &f.parent_repo_name),
            f.number
        ),
        Some(&f.owner_token),
        json!({"state": "open"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let reason: Option<String> =
        sqlx::query_scalar("SELECT closed_reason FROM pull_requests WHERE id = $1")
            .bind(pull_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(reason, None);

    finish(f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_merge_holds_the_fork_lock() {
    let f = build_fixture().await;
    let (status, intent) = create_merge_intent(&f, "merge", &f.fork_feature_head).await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    // Hold the fork's repository lock, then run the merge: it must block until
    // the lock is released, proving the merge serializes against fork pushes.
    let fork_guard = f
        .state
        .storage
        .repository_lock(f.fork_id)
        .lock_owned()
        .await;
    let state = f.state.clone();
    let merge_op = op;
    let handle = tokio::spawn(async move { execute_intent(&state, merge_op).await });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !handle.is_finished(),
        "merge completed while the fork lock was held"
    );

    drop(fork_guard);
    let outcome = handle.await.unwrap().unwrap();
    let oid = expect_merged(outcome);
    assert_eq!(main_tip(&f).await, oid);

    finish(f).await;
}

async fn insert_abandoned_fork_claim(
    f: &Fixture,
    contributor_id: Uuid,
    contrib_org_id: Uuid,
    key: &str,
    fork_name: &str,
    operation_id: Uuid,
    status: &str,
) -> Uuid {
    let fork_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id, organization_id, name, created_by, operation_id, status, visibility, description, parent_repository_id, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,'private','',$7, now() - interval '1 hour')",
    )
    .bind(fork_id)
    .bind(contrib_org_id)
    .bind(fork_name)
    .bind(contributor_id)
    .bind(operation_id)
    .bind(status)
    .bind(f.parent_repo_id)
    .execute(&f.pool)
    .await
    .unwrap();
    let path = format!(
        "/api/v1/organizations/{}/repositories/{}/forks",
        f.parent_org_slug, f.parent_repo_name
    );
    let body = json!({"organization": f.contrib_org_slug, "name": fork_name});
    let hash =
        githearth_server::idempotency::request_hash("POST", &path, body.to_string().as_bytes());
    sqlx::query(
        "INSERT INTO operations (actor_id, key, request_hash, status, claim_id, operation_id, created_at, expires_at) \
         VALUES ($1,$2,$3,'running',$4,$5, clock_timestamp() - interval '120 seconds', clock_timestamp() + interval '24 hours')",
    )
    .bind(contributor_id)
    .bind(key)
    .bind(hash)
    .bind(Uuid::new_v4())
    .bind(operation_id)
    .execute(&f.pool)
    .await
    .unwrap();
    fork_id
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resumed_fork_create_refuses_to_resurrect_a_deleted_fork() {
    let f = build_fixture().await;

    let contributor_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&f.contributor_username)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let contrib_org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&f.contrib_org_slug)
        .fetch_one(&f.pool)
        .await
        .unwrap();

    let fork_name = tag("resume-deleted");
    let body = json!({"organization": f.contrib_org_slug, "name": fork_name});
    let path = format!(
        "/api/v1/organizations/{}/repositories/{}/forks",
        f.parent_org_slug, f.parent_repo_name
    );
    let operation_id = Uuid::new_v4();
    let key = tag("resume-deleted-key");
    insert_abandoned_fork_claim(
        &f,
        contributor_id,
        contrib_org_id,
        &key,
        &fork_name,
        operation_id,
        "deleted",
    )
    .await;

    // Resuming the abandoned create must refuse to resurrect the deleted fork,
    // mirroring the repository create resume path (404, not 409).
    let (status, _) = request_with_idempotency(
        &f.app,
        "POST",
        &path,
        Some(&f.contributor_token),
        Some(&key),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resumed_fork_create_refreshes_updated_at() {
    let f = build_fixture().await;

    let contributor_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&f.contributor_username)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let contrib_org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&f.contrib_org_slug)
        .fetch_one(&f.pool)
        .await
        .unwrap();

    let fork_name = tag("resume-touch");
    let body = json!({"organization": f.contrib_org_slug, "name": fork_name});
    let path = format!(
        "/api/v1/organizations/{}/repositories/{}/forks",
        f.parent_org_slug, f.parent_repo_name
    );
    let operation_id = Uuid::new_v4();
    let key = tag("resume-touch-key");
    let fork_id = insert_abandoned_fork_claim(
        &f,
        contributor_id,
        contrib_org_id,
        &key,
        &fork_name,
        operation_id,
        "active",
    )
    .await;

    // A resumed create whose row is already active still refreshes updated_at,
    // so the reconciler (which only visits rows older than its grace) does not
    // race a resumed fork copy.
    let (status, _) = request_with_idempotency(
        &f.app,
        "POST",
        &path,
        Some(&f.contributor_token),
        Some(&key),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let age: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (clock_timestamp() - updated_at))::float8 FROM repositories WHERE id = $1",
    )
    .bind(fork_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        age < 60.0,
        "resumed fork create must refresh updated_at (age={age}s)"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resumed_fork_create_refuses_to_resurrect_a_failed_fork() {
    let f = build_fixture().await;

    let contributor_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&f.contributor_username)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let contrib_org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&f.contrib_org_slug)
        .fetch_one(&f.pool)
        .await
        .unwrap();

    let fork_name = tag("resume-failed");
    let body = json!({"organization": f.contrib_org_slug, "name": fork_name});
    let path = format!(
        "/api/v1/organizations/{}/repositories/{}/forks",
        f.parent_org_slug, f.parent_repo_name
    );
    let operation_id = Uuid::new_v4();
    let key = tag("resume-failed-key");
    insert_abandoned_fork_claim(
        &f,
        contributor_id,
        contrib_org_id,
        &key,
        &fork_name,
        operation_id,
        "failed",
    )
    .await;

    // A `failed` fork is neither deletable-resume nor active: resuming its
    // create must refuse with a conflict, never resurrect the storage.
    let (status, _) = request_with_idempotency(
        &f.app,
        "POST",
        &path,
        Some(&f.contributor_token),
        Some(&key),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    finish(f).await;
}

fn fork_href(org: &str, repo: &str) -> String {
    format!("/api/v1/organizations/{org}/repositories/{repo}/forks")
}

fn csrf_for_session(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let token_hash = Sha256::digest(token.as_bytes());
    hex::encode(&Sha256::digest(token_hash)[..16])
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn post_form(
    app: &Router,
    path: &str,
    cookie: &str,
    form: &[(&str, &str)],
) -> (StatusCode, Option<String>, String) {
    let body = form
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn forking_requires_read_on_the_parent() {
    let f = build_fixture().await;

    // A stranger with no read grant on the private parent sees a plain 404,
    // indistinguishable from a repository that does not exist.
    let (status, body) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.stranger_token),
        json!({"organization": f.contrib_org_slug}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A repository that does not exist is also 404.
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, "does-not-exist"),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_rejects_unknown_or_forbidden_target_namespaces_and_bad_names() {
    let f = build_fixture().await;

    // An unknown target organization.
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": "does-not-exist"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A namespace the contributor belongs to but cannot create repositories in
    // (they hold only a read grant on the parent org).
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": f.parent_org_slug}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An invalid fork name is rejected before any work happens.
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug, "name": "bad name!"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A body the handler cannot deserialize is a 400 as well.
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(fork_href(&f.parent_org_slug, &f.parent_repo_name))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", f.contributor_token))
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn forking_onto_an_existing_name_is_a_conflict() {
    let f = build_fixture().await;

    let (status, body) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug, "name": f.fork_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn forking_an_inactive_parent_is_a_conflict() {
    let f = build_fixture().await;

    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(f.parent_repo_id)
        .execute(&f.pool)
        .await
        .unwrap();

    let (status, body) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_form_rejects_bad_csrf_and_bad_idempotency_key() {
    let f = build_fixture().await;
    let cookie = format!("githearth_session={}", f.contributor_token);
    let path = format!(
        "/organizations/{}/repositories/{}/fork",
        f.parent_org_slug, f.parent_repo_name
    );

    // A forged CSRF token is refused with the forbidden page.
    let (status, _, _) = post_form(
        &f.app,
        &path,
        &cookie,
        &[
            ("csrf_token", "deadbeef"),
            ("idempotency_key", &tag("key")),
            ("organization", &f.contrib_org_slug),
            ("name", "renamed"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A valid CSRF token but a malformed idempotency key is a bad request.
    let csrf = csrf_for_session(&f.contributor_token);
    let (status, _, _) = post_form(
        &f.app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", "with space"),
            ("organization", &f.contrib_org_slug),
            ("name", "renamed"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_form_maps_failures_to_friendly_pages() {
    let f = build_fixture().await;
    let path = format!(
        "/organizations/{}/repositories/{}/fork",
        f.parent_org_slug, f.parent_repo_name
    );

    // A stranger who cannot read the parent gets the not-found page.
    let stranger_cookie = format!("githearth_session={}", f.stranger_token);
    let stranger_csrf = csrf_for_session(&f.stranger_token);
    let (status, _, body) = post_form(
        &f.app,
        &path,
        &stranger_cookie,
        &[
            ("csrf_token", &stranger_csrf),
            ("idempotency_key", &tag("key")),
            ("organization", &f.contrib_org_slug),
            ("name", "some-fork"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("The repository no longer exists."));

    // A name collision maps to the conflict message.
    let contributor_cookie = format!("githearth_session={}", f.contributor_token);
    let contributor_csrf = csrf_for_session(&f.contributor_token);
    let (status, _, _) = post_form(
        &f.app,
        &path,
        &contributor_cookie,
        &[
            ("csrf_token", &contributor_csrf),
            ("idempotency_key", &tag("key")),
            ("organization", &f.contrib_org_slug),
            ("name", &f.fork_name),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // An invalid name maps to the bad-request message.
    let (status, _, body) = post_form(
        &f.app,
        &path,
        &contributor_cookie,
        &[
            ("csrf_token", &contributor_csrf),
            ("idempotency_key", &tag("key")),
            ("organization", &f.contrib_org_slug),
            ("name", "bad name!"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_form_submits_a_fork_and_redirects() {
    let f = build_fixture().await;
    let cookie = format!("githearth_session={}", f.contributor_token);
    let csrf = csrf_for_session(&f.contributor_token);
    let new_name = tag("fork-form");
    let path = format!(
        "/organizations/{}/repositories/{}/fork",
        f.parent_org_slug, f.parent_repo_name
    );

    let (status, location, body) = post_form(
        &f.app,
        &path,
        &cookie,
        &[
            ("csrf_token", &csrf),
            ("idempotency_key", &tag("key")),
            ("organization", &f.contrib_org_slug),
            ("name", &new_name),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        location.as_deref(),
        Some(
            format!(
                "/organizations/{}/repositories/{new_name}",
                f.contrib_org_slug
            )
            .as_str()
        )
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repository_page_offers_no_fork_section_without_a_creatable_namespace() {
    let f = build_fixture().await;

    // Give the stranger read access to the parent, but they belong to no
    // organization at all, so there is no namespace to fork into.
    let stranger_id: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(&f.stranger_username)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id, account_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(f.parent_org_id)
    .bind(stranger_id)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO grants (id, role, subject_kind, subject_id, scope_kind, scope_id, granted_by) \
         VALUES ($1, 'read', 'account', $2, 'organization', $3, $2)",
    )
    .bind(Uuid::new_v4())
    .bind(stranger_id)
    .bind(f.parent_org_id)
    .execute(&f.pool)
    .await
    .unwrap();

    let cookie = format!("githearth_session={}", f.stranger_token);
    let path = format!(
        "/organizations/{}/repositories/{}",
        f.parent_org_slug, f.parent_repo_name
    );
    let (status, body) = get_body(&f.app, &path, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<h2>Fork</h2>"));

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_merge_reports_source_moved_when_the_fork_head_changes() {
    let f = build_fixture().await;

    let (status, intent) = create_merge_intent(&f, "merge", &f.fork_feature_head).await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    // Advance the fork's feature branch after the intent was recorded.
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "feature"])).await;
    std::fs::write(f.work.join("moved.txt"), "moved\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "moved.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=C",
        "-c",
        "user.email=c@c",
        "commit",
        "-m",
        "moved",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        f.fork_path.to_str().unwrap(),
        "feature",
    ]))
    .await;

    let outcome = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Stale {
            reason: "source moved".into()
        }
    );
    assert_eq!(main_tip(&f).await, f.main_head);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_merge_reports_source_not_active_when_the_fork_is_deleted() {
    let f = build_fixture().await;

    let (status, intent) = create_merge_intent(&f, "merge", &f.fork_feature_head).await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;

    // The fork row moves to `deleting` without closing the pull request (that
    // only happens on finish_delete), so the merge reaches the cross-repository
    // source check and reports the source is not active.
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(f.fork_id)
        .execute(&f.pool)
        .await
        .unwrap();

    let outcome = execute_intent(&f.state, op).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Stale {
            reason: "source repository not active".into()
        }
    );
    assert_eq!(main_tip(&f).await, f.main_head);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_migration_is_schema_scoped_beside_an_already_migrated_schema() {
    let f = build_fixture().await;

    // `public` is already migrated by the fixture (it holds the fork columns and
    // `pull_requests_distinct_branches`). Migrating a second schema must drop
    // only its own same-name check, never `public`'s, so a shared test database
    // with several migrated schemas does not trip the migration's DO block.
    let base =
        std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
    let schema = format!("fork_migration_{}", Uuid::new_v4().simple());
    let options = sqlx::postgres::PgConnectOptions::from_str(&base).unwrap();
    let schema_clone = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            let s = schema_clone.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}"))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();

    MIGRATOR.run(&pool).await.unwrap();

    // The second schema reached the fork migration: it has the parent column and
    // the schema-scoped distinct-branches constraint.
    let has_parent: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'repositories' \
         AND column_name = 'parent_repository_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        has_parent,
        "second schema is missing the fork parent column"
    );
    let constraint_name: String = sqlx::query_scalar(
        "SELECT con.conname FROM pg_constraint con \
         JOIN pg_class rel ON rel.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = rel.relnamespace \
         WHERE n.nspname = current_schema() AND rel.relname = 'pull_requests' \
         AND con.conname = 'pull_requests_distinct_branches'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(constraint_name, "pull_requests_distinct_branches");

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_rejects_description_too_long_and_blank_organization() {
    let f = build_fixture().await;

    // A description over the length limit is refused before any work happens.
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug, "description": "x".repeat(1001)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A blank target organization is refused too.
    let (status, _) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({"organization": "   "}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_of_a_public_parent_is_public() {
    let f = build_fixture().await;

    let public_name = tag("pub-parent");
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.parent_org_slug),
        Some(&f.owner_token),
        json!({"name": public_name, "visibility": "public"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let public_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let public_path = state_repo_path(&f.storage_root, public_id);

    // Seed a commit so the fork has a head to copy.
    let tw = f.work.to_str().unwrap();
    git_ok(git_cmd(&[
        "--git-dir",
        public_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;
    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        public_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    let fork_name = tag("pub-fork");
    let (status, fork) = request(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &public_name),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug, "name": fork_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    assert_eq!(fork["visibility"], "public");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fork_rejects_an_invalid_idempotency_key_header() {
    let f = build_fixture().await;

    // The JSON handler validates the Idempotency-Key header before delegating.
    let (status, _) = request_with_idempotency(
        &f.app,
        "POST",
        &fork_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        Some("not a valid key"),
        json!({"organization": f.contrib_org_slug}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    finish(f).await;
}

/// A cross-repository pull's `source_branch` becomes a Git refspec in the
/// server's own `git fetch`. A branch name that could escape `refs/heads/` or
/// be read as a fetch argument must be refused before any head is read, and a
/// malformed `source_repository` spec must be refused too.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_pull_rejects_untrusted_source_branch_and_spec() {
    let f = build_fixture().await;
    let href = repo_href(&f.parent_org_slug, &f.parent_repo_name);
    let source = format!("{}/{}", f.contrib_org_slug, f.fork_name);

    for branch in [
        "../main",
        "refs/heads/main",
        "main..x",
        "-x",
        "a b",
        ".hidden",
        "x/",
        "a//b",
    ] {
        let (status, body) = request(
            &f.app,
            "POST",
            &href,
            Some(&f.contributor_token),
            json!({
                "title": "t",
                "source_branch": branch,
                "target_branch": "main",
                "source_repository": source,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "branch {branch}: {body}");
    }

    for spec in ["", "no-slash", "a/b/c", "org/", "/repo"] {
        let (status, body) = request(
            &f.app,
            "POST",
            &href,
            Some(&f.contributor_token),
            json!({
                "title": "t",
                "source_branch": "feature",
                "target_branch": "main",
                "source_repository": spec,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "spec {spec}: {body}");
    }

    finish(f).await;
}

/// A fork of a SHA-256 parent keeps the object format, copies the parent's refs
/// and HEAD, and can be merged back across repositories (ADR 0008's full copy).
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sha256_fork_copies_objects_and_cross_repo_merges() {
    let f = build_fixture().await;

    let parent_name = tag("sha256-parent");
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.parent_org_slug),
        Some(&f.owner_token),
        json!({"name": parent_name, "visibility": "private", "object_format": "sha256"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let parent_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let parent_path = state_repo_path(&f.storage_root, parent_id);
    // The server installs a pre-receive hook; the test pushes directly to the
    // bare repository, so bypass it as the other fixtures do.
    git_ok(git_cmd(&[
        "--git-dir",
        parent_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;

    let work = std::env::temp_dir().join(format!("git-forks-sha256-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&[
        "-C",
        tw,
        "init",
        "--object-format=sha256",
        "-b",
        "main",
    ]))
    .await;
    std::fs::write(work.join("a.txt"), "a\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "a.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "root",
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        parent_path.to_str().unwrap(),
        "main",
    ]))
    .await;
    let parent_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    let fork_name = tag("sha256-fork");
    let (status, fork) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{parent_name}/forks",
            f.parent_org_slug
        ),
        Some(&f.contributor_token),
        json!({"organization": f.contrib_org_slug, "name": fork_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id: Uuid = fork["id"].as_str().unwrap().parse().unwrap();
    let format: String = sqlx::query_scalar("SELECT object_format FROM repositories WHERE id = $1")
        .bind(fork_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        format, "sha256",
        "the fork inherits the parent object format"
    );

    let fork_path = state_repo_path(&f.storage_root, fork_id);
    git_ok(git_cmd(&[
        "--git-dir",
        fork_path.to_str().unwrap(),
        "config",
        "core.hooksPath",
        "/dev/null",
    ]))
    .await;
    let head = std::fs::read_to_string(fork_path.join("HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/main\n");
    let fork_main = git_output(git_cmd(&[
        "--git-dir",
        fork_path.to_str().unwrap(),
        "rev-parse",
        "refs/heads/main",
    ]))
    .await;
    assert_eq!(fork_main, parent_head, "the fork copied the parent's main");

    // Advance the fork, then merge it back across repositories.
    std::fs::write(work.join("b.txt"), "b\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "b.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=C",
        "-c",
        "user.email=c@c",
        "commit",
        "-m",
        "fork change",
    ]))
    .await;
    let fork_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        fork_path.to_str().unwrap(),
        "main",
    ]))
    .await;

    let (status, pull) = request(
        &f.app,
        "POST",
        &repo_href(&f.parent_org_slug, &parent_name),
        Some(&f.contributor_token),
        json!({
            "title": "sha256 contribution",
            "source_branch": "main",
            "target_branch": "main",
            "source_repository": format!("{}/{}", f.contrib_org_slug, fork_name),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{pull}");
    let number = pull["number"].as_i64().unwrap();

    let (status, intent) = request(
        &f.app,
        "POST",
        &format!(
            "/api/v1/organizations/{}/repositories/{parent_name}/pulls/{number}/merge",
            f.parent_org_slug
        ),
        Some(&f.owner_token),
        json!({"method": "merge", "expected_head_oid": fork_head}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{intent}");
    let op: Uuid = intent["operation_id"].as_str().unwrap().parse().unwrap();
    set_intent_executing(&f.pool, op).await;
    let merged = expect_merged(execute_intent(&f.state, op).await.unwrap());
    let new_main = git_output(git_cmd(&[
        "--git-dir",
        parent_path.to_str().unwrap(),
        "rev-parse",
        "refs/heads/main",
    ]))
    .await;
    assert_eq!(new_main, merged, "the sha256 parent's main advanced");

    std::fs::remove_dir_all(&work).ok();
    finish(f).await;
}

/// A fork whose declared object format differs from the target is refused when
/// a cross-repository pull is opened, so the head fetch can never run across
/// object formats.
#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cross_repo_pull_rejects_object_format_mismatch() {
    let f = build_fixture().await;

    sqlx::query("UPDATE repositories SET object_format = 'sha256' WHERE id = $1")
        .bind(f.fork_id)
        .execute(&f.pool)
        .await
        .unwrap();

    let (status, body) = request(
        &f.app,
        "POST",
        &repo_href(&f.parent_org_slug, &f.parent_repo_name),
        Some(&f.contributor_token),
        json!({
            "title": "mismatch",
            "source_branch": "feature",
            "target_branch": "main",
            "source_repository": format!("{}/{}", f.contrib_org_slug, f.fork_name),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    finish(f).await;
}
