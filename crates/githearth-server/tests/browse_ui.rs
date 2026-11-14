//! Repository browsing UI contract tests (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::PgPool;
use std::path::PathBuf;
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

async fn git_ok(mut cmd: Command) {
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn git_output(mut cmd: Command) -> String {
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn state_repo_path(storage_root: &std::path::Path, repo_id: Uuid) -> PathBuf {
    let id_simple = repo_id.simple().to_string();
    storage_root
        .join("repositories")
        .join(&id_simple[..2])
        .join(format!("{id_simple}.git"))
}

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
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
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri(path);
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

async fn fetch_csrf(app: &Router) -> (String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    let marker = "name=\"csrf_token\" value=\"";
    let pos = html.find(marker).unwrap() + marker.len();
    let rest = &html[pos..];
    let end = rest.find('"').unwrap();
    (cookie, rest[..end].to_string())
}

async fn login(app: &Router, username: &str, password: &str) -> String {
    let (csrf_cookie, csrf_token) = fetch_csrf(app).await;
    let body = format!("username={username}&password={password}&csrf_token={csrf_token}");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", &csrf_cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

struct UiFixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    repo_name: String,
    cookie: String,
    token: String,
    root_commit: String,
    rename_commit: String,
    many_commit: String,
}

async fn build_fixture() -> UiFixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-browse-ui-{}", Uuid::new_v4()));
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

    let owner = Uuid::new_v4();
    let username = tag("alice");
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(owner)
        .bind(&username)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();

    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);

    let repo_name = tag("repo");
    let (status, body) = api(
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

    let work = std::env::temp_dir().join(format!("git-browse-ui-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    std::fs::create_dir_all(work.join("docs/nested")).unwrap();
    std::fs::write(work.join("docs/guide.md"), "guide\n").unwrap();
    std::fs::write(work.join("docs/nested/inner.txt"), "inner\n").unwrap();
    std::fs::write(work.join("bin.dat"), [0u8, 1, 2, 0]).unwrap();
    std::fs::write(work.join("big.bin"), vec![b'x'; 1_100_000]).unwrap();
    std::fs::write(work.join("xss.txt"), "<script>alert(1)</script>\n").unwrap();
    std::fs::write(work.join("<script>alert(1).txt"), "x\n").unwrap();
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

    // A commit touching more than MAX_DIFF_FILES files, so the diff is truncated.
    std::fs::create_dir_all(work.join("many")).unwrap();
    for i in 0..301 {
        std::fs::write(work.join(format!("many/f{i:03}.txt")), "x\n").unwrap();
    }
    git_ok(git_cmd(&["-C", tw, "add", "many"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-m",
        "many",
    ]))
    .await;
    let many_commit = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

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
    std::fs::remove_dir_all(&work).unwrap();

    let cookie = login(&app, &username, "password123456").await;

    UiFixture {
        app,
        pool,
        storage_root,
        org_slug,
        repo_name,
        cookie,
        token,
        root_commit,
        rename_commit,
        many_commit,
    }
}

async fn finish(f: UiFixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

fn ui_base(f: &UiFixture) -> String {
    format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tree_and_compare_pages_carry_u_i_headers() {
    let f = build_fixture().await;

    let resp = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("{}/tree/main", ui_base(&f)))
                .header("cookie", &f.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(resp.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn compare_page_carries_u_i_headers() {
    let f = build_compare_ui_fixture().await;
    let resp = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("{}/compare/main...feature", cmp_ui_base(&f)))
                .header("cookie", &f.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(resp.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    finish_cmp_ui(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn browse_pages_allow_anonymous_public_and_require_login_for_private() {
    let f = build_fixture().await;
    // The fixture repository is private: anonymous browsing redirects to login.
    let (status, _) = get_page(&f.app, &format!("{}/tree/main", ui_base(&f)), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // A public repository is browsable anonymously, like its landing page and
    // its Git transport.
    sqlx::query(
        "UPDATE repositories SET visibility = 'public' \
         WHERE organization_id = (SELECT id FROM organizations WHERE slug = $1) AND name = $2",
    )
    .bind(&f.org_slug)
    .bind(&f.repo_name)
    .execute(&f.pool)
    .await
    .unwrap();
    let (status, body) = get_page(&f.app, &format!("{}/tree/main", ui_base(&f)), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("renamed.md"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tree_page_renders_headings_table_scope_and_breadcrumbs() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/tree/main", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<table>"));
    assert!(body.contains("<th scope=\"col\">Name</th>"));
    assert!(body.contains("aria-label=\"Breadcrumb\""));
    assert!(
        body.contains("<caption class=\"muted\">Files at main</caption>"),
        "{body}"
    );
    assert!(
        body.contains(&format!("{}/commits/main", ui_base(&f))),
        "{body}"
    );
    assert!(body.contains("docs"));
    assert!(body.contains("renamed.md"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tree_page_escapes_script_file_name() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/tree/main", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("&lt;script&gt;alert(1).txt"));
    assert!(!body.contains("<script>alert(1).txt"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn blob_page_escapes_content_and_binary_shows_notice() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/blob/main/xss.txt", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!body.contains("<script>alert(1)</script>"));
    assert!(body.contains("aria-label=\"Breadcrumb\""), "{body}");

    let (status, body) = get_page(
        &f.app,
        &format!("{}/blob/main/bin.dat", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Binary or truncated file"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repository_page_links_to_default_branch_tree() {
    let f = build_fixture().await;
    let (status, body) = get_page(&f.app, &ui_base(&f), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("{}/tree/main", ui_base(&f))));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn empty_repository_page_shows_clone_instructions() {
    let f = build_fixture().await;
    let empty_name = tag("empty");
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("/api/v1/organizations/{}/repositories", f.org_slug),
        Some(&f.token),
        json!({"name": empty_name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = get_page(
        &f.app,
        &format!("/organizations/{}/repositories/{}", f.org_slug, empty_name),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("git clone http"), "{body}");
    assert!(
        body.contains(&format!("/{}/{}.git", f.org_slug, empty_name)),
        "{body}"
    );
    assert!(!body.contains("git clone /"), "{body}");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_browser_user_gets_404() {
    let f = build_fixture().await;
    let other = Uuid::new_v4();
    let other_name = tag("bob");
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(other)
        .bind(&other_name)
        .bind(&hash)
        .execute(&f.pool)
        .await
        .unwrap();
    let cookie = login(&f.app, &other_name, "password123456").await;
    let (status, _) = get_page(&f.app, &format!("{}/tree/main", ui_base(&f)), Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn tree_subdir_renders_breadcrumbs_and_kinds() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/tree/main/docs/nested", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("aria-label=\"Breadcrumb\""));
    assert!(body.contains("docs"));
    assert!(body.contains("nested"));
    assert!(body.contains("inner.txt"));

    let (status, body) = get_page(
        &f.app,
        &format!("{}/tree/main", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("symlink"));
    assert!(body.contains("commit"));
    // The submodule is rendered as plain text, not as a link.
    assert!(body.contains("<td>submod</td>"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn blob_page_shows_truncated_notice() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/blob/main/big.bin", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Binary or truncated file"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn commits_page_lists_commits_with_links() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/commits/main", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("root"));
    assert!(body.contains("rename"));
    assert!(body.contains(&format!("{}/commit/", ui_base(&f))));
    assert!(body.contains("aria-label=\"Breadcrumb\""), "{body}");
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn commit_page_renders_rename_and_binary_notices() {
    let f = build_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/commit/{}", ui_base(&f), f.rename_commit),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("renamed.md"));
    assert!(body.contains("aria-label=\"Breadcrumb\""), "{body}");

    let (status, body) = get_page(
        &f.app,
        &format!("{}/commit/{}", ui_base(&f), f.root_commit),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Binary file: bin.dat"));

    let (status, body) = get_page(
        &f.app,
        &format!("{}/commit/{}", ui_base(&f), f.many_commit),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Diff output truncated."));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn invalid_rev_and_path_render_not_found() {
    let f = build_fixture().await;
    for path in [
        format!("{}/tree/a..b", ui_base(&f)),
        format!("{}/tree/main/docs//guide", ui_base(&f)),
        format!("{}/blob/a..b/x", ui_base(&f)),
        format!("{}/commits/a..b", ui_base(&f)),
        format!("{}/commit/xyz", ui_base(&f)),
    ] {
        let (status, _) = get_page(&f.app, &path, Some(&f.cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn missing_and_non_active_repositories_are_404() {
    let f = build_fixture().await;
    let (status, _) = get_page(
        &f.app,
        &format!(
            "/organizations/{}/repositories/missing/tree/main",
            f.org_slug
        ),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

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
    let (status, _) = get_page(
        &f.app,
        &format!(
            "/organizations/{}/repositories/{name}/tree/main",
            f.org_slug
        ),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn database_failure_renders_unavailable() {
    let f = build_fixture().await;
    f.pool.close().await;
    let (status, _) = get_page(
        &f.app,
        &format!("{}/tree/main", ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let _ = std::fs::remove_dir_all(&f.storage_root);
}

// ---- comparison pages ------------------------------------------------------

struct CompareUiFixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    repo_name: String,
    cookie: String,
}

async fn build_compare_ui_fixture() -> CompareUiFixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-cmp-ui-{}", Uuid::new_v4()));
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

    let owner = Uuid::new_v4();
    let username = tag("cmp-alice");
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(owner)
        .bind(&username)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();

    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

    let org_id = Uuid::new_v4();
    let org_slug = tag("cmp-org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Organization')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);

    let repo_name = tag("cmp-repo");
    let (status, body) = api(
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

    let work = std::env::temp_dir().join(format!("git-cmp-ui-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    let tw = work.to_str().unwrap();
    git_ok(git_cmd(&["-C", tw, "init", "-b", "main"])).await;
    std::fs::write(work.join("shared.txt"), "v1\n").unwrap();
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "-A"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "root",
    ]))
    .await;
    let root = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;

    git_ok(git_cmd(&[
        "-C", tw, "checkout", "-q", "-b", "feature", &root,
    ]))
    .await;
    std::fs::write(work.join("shared.txt"), "v2f\n").unwrap();
    std::fs::write(work.join("feature.txt"), "feature\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "-A"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "F1",
    ]))
    .await;

    git_ok(git_cmd(&["-C", tw, "checkout", "-q", "main"])).await;
    std::fs::write(work.join("shared.txt"), "v2m\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "-A"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "M2",
    ]))
    .await;

    let xss_branch = "<script>alert(1)</script>";
    git_ok(git_cmd(&[
        "-C", tw, "checkout", "-q", "-b", xss_branch, &root,
    ]))
    .await;
    std::fs::write(work.join("<script>alert(1).txt"), "x\n").unwrap();
    git_ok(git_cmd(&["-C", tw, "add", "-A"])).await;
    git_ok(git_cmd(&[
        "-C",
        tw,
        "-c",
        "user.name=T",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "X1",
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
        repo_path.to_str().unwrap(),
        "main",
        "feature",
        xss_branch,
    ]))
    .await;
    std::fs::remove_dir_all(&work).unwrap();

    let cookie = login(&app, &username, "password123456").await;

    CompareUiFixture {
        app,
        pool,
        storage_root,
        org_slug,
        repo_name,
        cookie,
    }
}

fn cmp_ui_base(f: &CompareUiFixture) -> String {
    format!("/organizations/{}/repositories/{}", f.org_slug, f.repo_name)
}

async fn finish_cmp_ui(f: CompareUiFixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn compare_page_renders_mergeability_conflicts_commits_and_diff() {
    let f = build_compare_ui_fixture().await;
    let (status, body) = get_page(
        &f.app,
        &format!("{}/compare/main...feature", cmp_ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Compare main...feature"));
    assert!(body.contains("Conflicts"));
    assert!(body.contains("<li>shared.txt</li>"));
    assert!(body.contains("1 ahead, 1 behind"));
    assert!(body.contains("Files changed"));
    assert!(body.contains("<table>"));
    assert!(
        body.contains("<caption class=\"muted\">Commits in this comparison</caption>"),
        "{body}"
    );
    assert!(body.contains("aria-label=\"Breadcrumb\""), "{body}");
    assert!(body.contains("feature.txt"));
    assert!(body.contains(&format!("{}/commit/", cmp_ui_base(&f))));
    finish_cmp_ui(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn compare_page_escapes_branch_and_file_names() {
    let f = build_compare_ui_fixture().await;
    let spec = "main...%3Cscript%3Ealert(1)%3C%2Fscript%3E";
    let (status, body) = get_page(
        &f.app,
        &format!("{}/compare/{spec}", cmp_ui_base(&f)),
        Some(&f.cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(body.contains("&lt;script&gt;alert(1).txt"));
    assert!(!body.contains("<script>alert(1)"));
    finish_cmp_ui(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repository_page_links_to_compare_and_compare_form_redirects() {
    let f = build_compare_ui_fixture().await;

    let (status, body) = get_page(&f.app, &cmp_ui_base(&f), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("{}/compare/main...main", cmp_ui_base(&f))));

    // The small GET form redirects to the canonical compare/<base>...<head> path.
    let resp = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "{}/compare?base=main&head=feature",
                    cmp_ui_base(&f)
                ))
                .header("cookie", &f.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(
        location,
        format!("{}/compare/main...feature", cmp_ui_base(&f))
    );
    finish_cmp_ui(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn compare_page_requires_login() {
    let f = build_compare_ui_fixture().await;
    let (status, _) = get_page(
        &f.app,
        &format!("{}/compare/main...feature", cmp_ui_base(&f)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    finish_cmp_ui(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unmatched_paths_render_the_styled_404_with_page_headers() {
    let f = build_fixture().await;
    for path in [
        "/no-such-page".to_owned(),
        format!("{}/tree/", ui_base(&f)),
        format!("{}/tree", ui_base(&f)),
    ] {
        let response = f
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(&path)
                    .header("cookie", &f.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "{path}"
        );
        assert_eq!(
            response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
            "DENY",
            "{path}"
        );
        assert!(
            response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_some(),
            "{path}"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("<h1>Not found</h1>"), "{path}: {body}");
    }
    finish(f).await;
}
