//! Repository browsing API contract tests (PostgreSQL-backed).
//!
//! Fixture repositories are created through the repository API, then filled by
//! a plain `git` CLI in a temporary work tree pushing directly to the storage
//! path — never through HTTP.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
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
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
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

fn state_repo_path(storage_root: &Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    token: String,
    repo_name: String,
    root_commit: String,
    rename_commit: String,
    head: String,
}

/// One repository with a rich history: nested dirs, a binary file, a >1 MiB
/// file, a symlink, a rename, a branch, a tag, a merge commit and unicode,
/// space and `<script>` file names.
async fn build_fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-browse-{}", Uuid::new_v4()));
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
    let token = fixture_session(&pool, owner).await;
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);

    let repo_name = tag("repo");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&token),
        json!({"name": repo_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);

    // Build history in a temporary work tree and push directly to storage.
    let work = std::env::temp_dir().join(format!("git-browse-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;

    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    std::fs::create_dir_all(work.join("docs")).unwrap();
    std::fs::write(work.join("docs/guide.md"), "guide\n").unwrap();
    std::fs::write(work.join("bin.dat"), [0u8, 1, 2, 3, 0, 255]).unwrap();
    std::fs::write(work.join("big.bin"), vec![b'x'; 1_100_000]).unwrap();
    std::fs::write(work.join("sp ace.txt"), "space\n").unwrap();
    std::fs::write(work.join("файл.txt"), "unicode\n").unwrap();
    std::fs::write(work.join("<script>alert(1).txt"), "xss\n").unwrap();
    std::fs::write(work.join("latin1.txt"), [0xFF, 0xFE]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.md", work.join("link")).unwrap();

    git_ok(git_cmd(&["-C", tw, "add", "-A"])).await;
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
    let root_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    git_ok(git_cmd(&["-C", tw, "tag", "v1.0", &root_commit])).await;

    // A gitlink (submodule) entry pointing at the just-created commit.
    git_ok(git_cmd(&[
        "-C",
        tw,
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{root_commit},submod"),
    ]))
    .await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "add submodule",
    ]))
    .await;

    git_ok(git_cmd(&["-C", tw, "mv", "README.md", "renamed.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "rename",
    ]))
    .await;
    let rename_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    git_ok(git_cmd(&[
        "-C",
        tw,
        "checkout",
        "-q",
        "-b",
        "feature",
        &root_commit,
    ]))
    .await;
    std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "feature.txt"])).await;
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

    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "merge",
        "--no-ff",
        "feature",
        "-m",
        "merge feature",
    ]))
    .await;
    let head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    // Direct storage pushes bypass GitHearth, so skip its ref-transaction hooks.
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
        "feature",
        "v1.0",
    ]))
    .await;

    std::fs::remove_dir_all(&work).unwrap();

    Fixture {
        app,
        pool,
        storage_root,
        org_slug,
        token,
        repo_name,
        root_commit,
        rename_commit,
        head,
    }
}

impl Fixture {
    fn base(&self) -> String {
        format!(
            "/api/v1/organizations/{}/repositories/{}",
            self.org_slug, self.repo_name
        )
    }
}

async fn finish(fixture: Fixture) {
    let _ = std::fs::remove_dir_all(&fixture.storage_root);
    fixture.pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn refs_lists_branches_tags_and_default_branch() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/refs", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_branch"], "main");
    let branches: Vec<String> = body["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(branches.contains(&"main".to_owned()));
    assert!(branches.contains(&"feature".to_owned()));
    let tags: Vec<String> = body["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(tags, vec!["v1.0".to_owned()]);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tree_lists_directories_first_with_kind_and_size() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/tree/main", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "docs");
    assert_eq!(entries[0]["kind"], "tree");
    let bin = entries.iter().find(|e| e["name"] == "bin.dat").unwrap();
    assert_eq!(bin["kind"], "blob");
    assert_eq!(bin["size"], 6);
    let link = entries.iter().find(|e| e["name"] == "link").unwrap();
    assert_eq!(link["kind"], "symlink");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn nested_tree_and_text_blob() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/tree/main?path=docs", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entries"][0]["name"], "guide.md");

    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=docs/guide.md", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "guide\n");
    assert!(body["base64"].is_null());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn binary_blob_is_base64_and_large_blob_is_truncated() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=bin.dat", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["binary"], true);
    assert!(body["base64"].is_string());

    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=big.bin", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["truncated"], true);
    assert_eq!(body["size"], 1_100_000);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unicode_space_and_script_file_names_survive_round_trip() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=sp%20ace.txt", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "space\n");

    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=%D1%84%D0%B0%D0%B9%D0%BB.txt", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "unicode\n");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn commits_paginate_by_cursor() {
    let f = build_fixture().await;
    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let url = match cursor {
            Some(c) => format!("{}/commits/main?limit=2&cursor={c}", f.base()),
            None => format!("{}/commits/main?limit=2", f.base()),
        };
        let (status, body) = request(&f.app, "GET", &url, Some(&f.token), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        for item in items {
            seen.push(item["id"].as_str().unwrap().to_owned());
        }
        match body["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    // root + submodule + rename + feature + merge = 5 commits reachable from main.
    assert_eq!(seen.len(), 5);
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 5);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn root_commit_diff_is_against_the_empty_tree() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/commit/{}", f.base(), f.root_commit),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["parents"].as_array().unwrap().len(), 0);
    let files = body["files"].as_array().unwrap();
    let readme = files.iter().find(|f| f["new_path"] == "README.md").unwrap();
    assert_eq!(readme["status"], "A");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rename_is_detected_with_both_paths() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/commit/{}", f.base(), f.rename_commit),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let files = body["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0]["status"].as_str().unwrap().starts_with('R'));
    assert_eq!(files[0]["old_path"], "README.md");
    assert_eq!(files[0]["new_path"], "renamed.md");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn merge_commit_diffs_against_first_parent_only() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/commit/{}", f.base(), f.head),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["parents"].as_array().unwrap().len(), 2);
    let files = body["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["new_path"], "feature.txt");
    assert_eq!(files[0]["status"], "A");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn bad_rev_and_path_syntax_400() {
    let f = build_fixture().await;
    for path in [
        format!("{}/tree/a..b", f.base()),
        format!("{}/tree/-foo", f.base()),
        format!("{}/tree/main?path=..", f.base()),
        format!("{}/tree/main?path=a/../b", f.base()),
        format!("{}/tree/main?path=a%00b", f.base()),
        format!("{}/commit/xyz", f.base()),
    ] {
        let (status, _) = request(&f.app, "GET", &path, Some(&f.token), Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unknown_rev_404_and_unknown_path_404() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/tree/nope", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "revision not found");

    let (status, _) = request(
        &f.app,
        "GET",
        &format!("{}/tree/main?path=nope", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_user_and_unauthenticated_are_denied() {
    let f = build_fixture().await;
    let stranger = fixture_account(&f.pool, &tag("stranger")).await;
    let stranger_token = fixture_session(&f.pool, stranger).await;
    let (status, _) = request(
        &f.app,
        "GET",
        &format!("{}/refs", f.base()),
        Some(&stranger_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = request(
        &f.app,
        "GET",
        &format!("{}/refs", f.base()),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn empty_repository_returns_empty_refs_and_unknown_rev() {
    let f = build_fixture().await;
    let empty_name = tag("empty");
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.token),
        json!({"name": empty_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let base = format!(
        "/api/v1/organizations/{}/repositories/{}",
        f.org_slug, empty_name
    );
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{base}/refs"),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["branches"].as_array().unwrap().len(), 0);
    assert_eq!(body["tags"].as_array().unwrap().len(), 0);
    assert_eq!(body["default_branch"], "main");

    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{base}/tree/main"),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "revision not found");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sha256_repository_browses() {
    // The create API hard-codes SHA-1, so build the SHA-256 repository on disk
    // directly against an `active` row.
    let storage_root =
        std::env::temp_dir().join(format!("githearth-browse-256-{}", Uuid::new_v4()));
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
    let owner = fixture_account(&pool, &tag("sha-owner")).await;
    let token = fixture_session(&pool, owner).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;
    let repo_name = tag("sha256");
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind(&repo_name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let repo_path = state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha256,
        )
        .unwrap();

    let work = std::env::temp_dir().join(format!("git-browse-256-work-{}", Uuid::new_v4()));
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
    std::fs::write(work.join("f.txt"), "sha256\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "f.txt"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "sha256",
    ]))
    .await;
    // Direct storage pushes bypass GitHearth, so skip its ref-transaction hooks.
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--",
        repo_path.to_str().unwrap(),
        "main",
    ]))
    .await;
    let head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
    assert_eq!(head.len(), 64);
    std::fs::remove_dir_all(&work).unwrap();

    let app = router(state);
    let base = format!("/api/v1/organizations/{org_slug}/repositories/{repo_name}");
    let (status, body) = request(
        &app,
        "GET",
        &format!("{base}/blob/main?path=f.txt"),
        Some(&token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], "sha256\n");

    let _ = std::fs::remove_dir_all(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn submodule_entry_reports_kind_commit() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/tree/main", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let submod = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "submod")
        .unwrap();
    assert_eq!(submod["kind"], "commit");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_utf8_blob_is_base64_not_binary() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/blob/main?path=latin1.txt", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["binary"], false);
    assert!(body["text"].is_null());
    assert!(body["base64"].is_string());
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn blob_of_directory_or_missing_path_is_404() {
    let f = build_fixture().await;
    for path in ["docs", "nope"] {
        let (status, _) = request(
            &f.app,
            "GET",
            &format!("{}/blob/main?path={path}", f.base()),
            Some(&f.token),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "path={path}");
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn blob_and_log_bad_syntax_400() {
    let f = build_fixture().await;
    for path in [
        format!("{}/blob/a..b?path=x", f.base()),
        format!("{}/blob/main?path=..", f.base()),
        format!("{}/blob/main?path=a/../b", f.base()),
        format!("{}/commits/a..b", f.base()),
        format!("{}/commits/main?path=..", f.base()),
        format!("{}/commits/main?limit=0", f.base()),
        format!("{}/commits/main?limit=101", f.base()),
    ] {
        let (status, _) = request(&f.app, "GET", &path, Some(&f.token), Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn log_filters_by_path() {
    let f = build_fixture().await;
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/commits/main?path=docs", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], f.root_commit);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unknown_commit_id_is_404() {
    let f = build_fixture().await;
    let missing = "0".repeat(40);
    let (status, body) = request(
        &f.app,
        "GET",
        &format!("{}/commit/{missing}", f.base()),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "revision not found");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_active_repository_is_404() {
    let f = build_fixture().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&f.org_slug)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let owner: Uuid =
        sqlx::query_scalar("SELECT account_id FROM memberships WHERE organization_id = $1 LIMIT 1")
            .bind(org_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let name = tag("creating");
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'creating','private')")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(&name)
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(&f.pool)
        .await
        .unwrap();
    let (status, _) = request(
        &f.app,
        "GET",
        &format!(
            "/api/v1/organizations/{}/repositories/{name}/refs",
            f.org_slug
        ),
        Some(&f.token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}
