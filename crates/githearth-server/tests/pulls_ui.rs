//! Pull request, review and comment page contract tests (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use serde_json::json;
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
    base: String,
    api_base: String,
    token: String,
    cookie: String,
    org_id: Uuid,
    repo_id: Uuid,
    work: PathBuf,
    feature_head: String,
    head: String,
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, _, body) = send(app, builder.body(Body::empty()).unwrap()).await;
    (status, body)
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
    send(
        app,
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
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

async fn api(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let (status, _, text) = send(
        app,
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    (status, serde_json::from_str(&text).unwrap_or_default())
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

fn csrf(html: &str) -> String {
    hidden(html, "csrf_token")
}

fn key(html: &str) -> String {
    hidden(html, "idempotency_key")
}

async fn account(pool: &PgPool) -> (Uuid, String, String) {
    let id = Uuid::new_v4();
    let username = tag("user");
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(&username)
        .bind(hash_password("password123456").unwrap())
        .execute(pool)
        .await
        .unwrap();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    (id, username, token)
}

async fn fixture() -> Fixture {
    let storage_root = std::env::temp_dir().join(format!("githearth-pulls-ui-{}", Uuid::new_v4()));
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
    let (owner, _, token) = account(&pool).await;
    let org_id = Uuid::new_v4();
    let org = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Org')")
        .bind(org_id)
        .bind(&org)
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
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    let repo = tag("repo");
    let (status, body) = api(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org}/repositories"),
        &token,
        json!({"name": repo}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repo_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let repo_path = state_repo_path(&storage_root, repo_id);

    // Build main + feature branches directly in storage.
    let work = std::env::temp_dir().join(format!("git-pulls-ui-work-{}", Uuid::new_v4()));
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
    let head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
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
    let feature_head = git_output(git_cmd(&["-C", tw, "rev-parse", "HEAD"])).await;
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
    ]))
    .await;

    Fixture {
        app,
        pool,
        storage_root,
        base: format!("/organizations/{org}/repositories/{repo}"),
        api_base: format!("/api/v1/organizations/{org}/repositories/{repo}"),
        cookie: format!("githearth_session={token}"),
        token,
        org_id,
        repo_id,
        work,
        feature_head,
        head,
    }
}

async fn finish(f: Fixture) {
    let _ = std::fs::remove_dir_all(&f.storage_root);
    let _ = std::fs::remove_dir_all(&f.work);
    f.pool.close().await;
}

/// Adds a browser-session account with the given organization role, returning its cookie.
async fn member_cookie(f: &Fixture, role: &str) -> String {
    let (id, _, token) = account(&f.pool).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(f.org_id)
    .bind(id)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,$2,'account',$3,'organization',$4,$3)")
        .bind(Uuid::new_v4())
        .bind(role)
        .bind(id)
        .bind(f.org_id)
        .execute(&f.pool)
        .await
        .unwrap();
    format!("githearth_session={token}")
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_view_comment_and_review_through_forms() {
    let f = fixture().await;
    let (status, repo_page) = get(&f.app, &f.base, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(repo_page.contains(&format!("{}/pulls\"", f.base)));

    let (status, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("<label for=\"source_branch\">"));
    assert!(form.contains("<label for=\"target_branch\">"));
    let token = csrf(&form);
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &key(&form)),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "<script>x</script> broken"),
            ("body", "Steps & details"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(location, format!("{}/pulls/1", f.base));

    let (status, page) = get(&f.app, &location, Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("&lt;script&gt;x&lt;/script&gt; broken"));
    assert!(!page.contains("<script>x"));
    assert!(page.contains("Steps &amp; details"));
    assert!(page.contains("No comments yet."));
    assert!(page.contains("No reviews yet."));
    assert!(page.contains(&f.feature_head));
    assert!(page.contains(&f.head));
    assert!(page.contains("<dt>Approvals</dt>"));

    // Add a comment.
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/pulls/1/comments", f.base),
        &f.cookie,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("body", "first <b>comment</b>"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, page) = get(&f.app, &location.unwrap(), Some(&f.cookie)).await;
    assert!(page.contains("first &lt;b&gt;comment&lt;/b&gt;"));

    // Submit a review at the head (commit_id is a hidden field on the page).
    // The author cannot approve their own PR, so a write member reviews.
    let commit_id = hidden(&page, "commit_id");
    assert_eq!(commit_id, f.feature_head);
    let reviewer = member_cookie(&f, "write").await;
    let (status, page) = get(&f.app, &format!("{}/pulls/1", f.base), Some(&reviewer)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, location, _) = post_form(
        &f.app,
        &format!("{}/pulls/1/reviews", f.base),
        &reviewer,
        &[
            ("csrf_token", &csrf(&page)),
            ("idempotency_key", &key(&page)),
            ("verdict", "approved"),
            ("body", "looks good"),
            ("commit_id", &commit_id),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_, page) = get(&f.app, &location.unwrap(), Some(&f.cookie)).await;
    assert!(page.contains("approved"));

    // The API sees exactly what the forms created, with the same audit events.
    let (_, detail) = api(
        &f.app,
        "GET",
        &format!("{}/pulls/1", f.api_base),
        &f.token,
        json!(null),
    )
    .await;
    assert_eq!(detail["approvals"], 1);

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind IN ('pull_request.created','pull_request.commented')",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(events, 2);

    let (_, list) = get(&f.app, &format!("{}/pulls", f.base), Some(&f.cookie)).await;
    assert!(list.contains(&format!("{}/pulls/1\"", f.base)));
    assert!(list.contains("<th scope=\"col\">Title</th>"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pull_and_comment_lists_paginate() {
    let f = fixture().await;
    // Create 31 pull requests each on its own branch, and 31 comments on the first.
    for n in 1..=31 {
        let branch = format!("b{n}");
        git_ok(git_cmd(&[
            "-C",
            f.work.to_str().unwrap(),
            "checkout",
            "-q",
            "-b",
            &branch,
        ]))
        .await;
        git_ok(git_cmd(&[
            "-C",
            f.work.to_str().unwrap(),
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            &format!("commit {n}"),
        ]))
        .await;
        let repo_path = state_repo_path(&f.storage_root, f.repo_id);
        git_ok(git_cmd(&[
            "-C",
            f.work.to_str().unwrap(),
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "--",
            repo_path.to_str().unwrap(),
            &branch,
        ]))
        .await;
        let (status, _) = api(
            &f.app,
            "POST",
            &format!("{}/pulls", f.api_base),
            &f.token,
            json!({"title": format!("pull {n}"), "source_branch": branch, "target_branch": "main"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    for i in 1..=31 {
        let (status, _) = api(
            &f.app,
            "POST",
            &format!("{}/pulls/1/comments", f.api_base),
            &f.token,
            json!({"body": format!("comment {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (_, first) = get(&f.app, &format!("{}/pulls", f.base), Some(&f.cookie)).await;
    assert!(first.contains("pull 31<"));
    assert!(!first.contains("pull 1<"));
    let next_url = format!("{}/pulls?state=open&amp;after=2", f.base);
    assert!(first.contains(&next_url), "{first}");
    let (_, second) = get(&f.app, &next_url.replace("&amp;", "&"), Some(&f.cookie)).await;
    assert!(second.contains("pull 1<"));
    assert!(second.contains("First page"));
    assert!(!second.contains("rel=\"next\""));

    let (_, closed) = get(
        &f.app,
        &format!("{}/pulls?state=closed", f.base),
        Some(&f.cookie),
    )
    .await;
    assert!(closed.contains("No pull requests."));

    let (_, pull) = get(&f.app, &format!("{}/pulls/1", f.base), Some(&f.cookie)).await;
    assert!(pull.contains("comment 30<"));
    assert!(!pull.contains("comment 31<"));
    let (_, more) = get(
        &f.app,
        &format!("{}/pulls/1?after=30", f.base),
        Some(&f.cookie),
    )
    .await;
    assert!(more.contains("comment 31<"));
    assert!(more.contains("First comments"));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn forms_require_session_csrf_and_valid_input() {
    let f = fixture().await;
    let (status, _) = get(&f.app, &format!("{}/pulls", f.base), None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[("csrf_token", "wrong"), ("title", "t")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[("title", "t")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    let token = csrf(&form);
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "no key"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let k = key(&form);
    let (status, _, page) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "   "),
            ("body", "kept text"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(page.contains("role=\"alert\""));
    assert!(page.contains("kept text"));
    assert_eq!(
        key(&page),
        k,
        "the re-rendered form keeps its idempotency key"
    );

    // A missing branch (valid names, but source does not exist) re-renders too
    // via the API conflict-to-page mapping, not a 500.
    let (_, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &csrf(&form)),
            ("idempotency_key", &key(&form)),
            ("source_branch", "nope"),
            ("target_branch", "main"),
            ("title", "x"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = get(&f.app, &format!("{}/pulls/abc", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resubmitted_forms_do_not_duplicate() {
    let f = fixture().await;
    let (_, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    let (token, k) = (csrf(&form), key(&form));
    let fields = [
        ("csrf_token", token.as_str()),
        ("idempotency_key", k.as_str()),
        ("source_branch", "feature"),
        ("target_branch", "main"),
        ("title", "double click"),
    ];
    let (first, first_location, _) =
        post_form(&f.app, &format!("{}/pulls", f.base), &f.cookie, &fields).await;
    let (second, second_location, _) =
        post_form(&f.app, &format!("{}/pulls", f.base), &f.cookie, &fields).await;
    assert_eq!(
        (first, second),
        (StatusCode::SEE_OTHER, StatusCode::SEE_OTHER)
    );
    assert_eq!(first_location, second_location);

    let (_, page) = get(&f.app, &first_location.unwrap(), Some(&f.cookie)).await;
    let (token, k) = (csrf(&page), key(&page));
    let fields = [
        ("csrf_token", token.as_str()),
        ("idempotency_key", k.as_str()),
        ("body", "once"),
    ];
    for _ in 0..2 {
        let (status, _, _) = post_form(
            &f.app,
            &format!("{}/pulls/1/comments", f.base),
            &f.cookie,
            &fields,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM pull_requests p JOIN repositories r ON r.id = p.repository_id WHERE r.organization_id = $1), \
                (SELECT count(*) FROM pull_request_comments c JOIN pull_requests p ON p.id = c.pull_request_id JOIN repositories r ON r.id = p.repository_id WHERE r.organization_id = $1), \
                (SELECT count(*) FROM audit_events WHERE organization_id = $1 AND kind IN ('pull_request.created','pull_request.commented'))",
    )
    .bind(f.org_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 2));
    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn edited_resubmit_with_the_same_key_shows_the_422_notice_and_keeps_the_text() {
    let f = fixture().await;
    let (_, form) = get(&f.app, &format!("{}/pulls/new", f.base), Some(&f.cookie)).await;
    let (token, k) = (csrf(&form), key(&form));

    // The first submit succeeds.
    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "first title"),
            ("body", "kept text"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // Resubmitting the same key with an edited title re-renders the form with
    // the friendly notice and the user's text.
    let (status, _, page) = post_form(
        &f.app,
        &format!("{}/pulls", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("source_branch", "feature"),
            ("target_branch", "main"),
            ("title", "second title"),
            ("body", "kept text"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        page.contains("This form was already submitted. Reload the page to start a new one."),
        "{page}"
    );
    assert!(page.contains("kept text"), "{page}");
    assert_eq!(key(&page), k, "the re-rendered form keeps its key");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn comment_resubmit_with_the_same_key_shows_the_422_notice() {
    let f = fixture().await;
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/pulls", f.api_base),
        &f.token,
        json!({"title": "p", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, page) = get(&f.app, &format!("{}/pulls/1", f.base), Some(&f.cookie)).await;
    let (token, k) = (csrf(&page), key(&page));

    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls/1/comments", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("body", "first comment"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, page) = post_form(
        &f.app,
        &format!("{}/pulls/1/comments", f.base),
        &f.cookie,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("body", "edited comment"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        page.contains("This form was already submitted. Reload the page to start a new one."),
        "{page}"
    );
    assert!(page.contains("edited comment"), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn review_resubmit_with_the_same_key_shows_the_422_notice() {
    let f = fixture().await;
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/pulls", f.api_base),
        &f.token,
        json!({"title": "p", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The author cannot approve their own pull request, so a write member reviews.
    let reviewer = member_cookie(&f, "write").await;
    let (_, page) = get(&f.app, &format!("{}/pulls/1", f.base), Some(&reviewer)).await;
    let (token, k, commit_id) = (csrf(&page), key(&page), hidden(&page, "commit_id"));

    let (status, _, _) = post_form(
        &f.app,
        &format!("{}/pulls/1/reviews", f.base),
        &reviewer,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("verdict", "approved"),
            ("body", ""),
            ("commit_id", &commit_id),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, _, page) = post_form(
        &f.app,
        &format!("{}/pulls/1/reviews", f.base),
        &reviewer,
        &[
            ("csrf_token", &token),
            ("idempotency_key", &k),
            ("verdict", "approved"),
            ("body", "changed"),
            ("commit_id", &commit_id),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        page.contains("This form was already submitted. Reload the page to start a new one."),
        "{page}"
    );

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn merged_pull_header_shows_the_badge_once_and_not_the_raw_state() {
    let f = fixture().await;
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/pulls", f.api_base),
        &f.token,
        json!({"title": "merged pull", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    sqlx::query("UPDATE pull_requests SET state = 'merged' WHERE number = 1")
        .execute(&f.pool)
        .await
        .unwrap();

    let (status, page) = get(&f.app, &format!("{}/pulls/1", f.base), Some(&f.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("<span class=\"badge\">Merged</span>"),
        "{page}"
    );
    assert!(!page.contains("Merged merged"), "{page}");

    finish(f).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn unrelated_users_get_404() {
    let f = fixture().await;
    let (status, _) = api(
        &f.app,
        "POST",
        &format!("{}/pulls", f.api_base),
        &f.token,
        json!({"title": "private", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, _, stranger) = account(&f.pool).await;
    let cookie = format!("githearth_session={stranger}");
    for path in ["/pulls", "/pulls/new", "/pulls/1"] {
        let (status, _) = get(&f.app, &format!("{}{path}", f.base), Some(&cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    finish(f).await;
}
