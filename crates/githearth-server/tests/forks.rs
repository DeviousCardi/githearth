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

    finish(f).await;
}
