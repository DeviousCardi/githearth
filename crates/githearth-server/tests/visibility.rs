//! Repository visibility across the HTTP git transport and the web UI.
//!
//! Verifies that public repositories are readable anonymously, internal
//! repositories are readable by members of the owning organization only, and
//! private repositories are denied to everyone without an explicit grant.
//! SSH authorization is exercised by the M4 harness (scripts/verify-m4.py).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
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

struct Fixture {
    app: Router,
    pool: PgPool,
    storage_root: PathBuf,
    org_slug: String,
    owner: Uuid,
    member: Uuid,
    non_member: Uuid,
}

impl Fixture {
    async fn teardown(self) {
        let _ = std::fs::remove_dir_all(&self.storage_root);
        self.pool.close().await;
    }
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
    let hash = hash_password(password).unwrap();
    sqlx::query("UPDATE accounts SET password_hash=$1 WHERE id=$2")
        .bind(hash)
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
}

async fn username_of(pool: &PgPool, account: Uuid) -> String {
    sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(account)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Creates a repository row (active) with the given visibility and seeds one
/// commit into its on-disk bare repository.
async fn seed_repo(
    pool: &PgPool,
    state: &AppState,
    org_id: Uuid,
    owner: Uuid,
    name: &str,
    visibility: &str,
) -> Uuid {
    let repo_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) \
         VALUES ($1,$2,$3,$4,$5,'active',$6)",
    )
    .bind(repo_id)
    .bind(org_id)
    .bind(name)
    .bind(owner)
    .bind(Uuid::new_v4())
    .bind(visibility)
    .execute(pool)
    .await
    .unwrap();

    let repo_path = state
        .storage
        .create(
            repo_id,
            Uuid::new_v4(),
            githearth_git::layout::ObjectFormat::Sha1,
        )
        .unwrap();

    let work = std::env::temp_dir().join(format!("visibility-work-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work).unwrap();
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "init",
        "-b",
        "main",
    ]))
    .await;
    std::fs::write(work.join("README.md"), format!("# {name}\n")).unwrap();
    git_ok(git_cmd(&["-C", work.to_str().unwrap(), "add", "README.md"])).await;
    git_ok(git_cmd(&[
        "-C",
        work.to_str().unwrap(),
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]))
    .await;
    seed_bare(repo_path.to_str().unwrap(), work.to_str().unwrap()).await;
    let _ = std::fs::remove_dir_all(&work);
    repo_id
}

async fn setup() -> Fixture {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-visibility-{}", Uuid::new_v4()));
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
    set_password(&pool, owner, "ownerpass").await;
    let member = fixture_account(&pool, &tag("member")).await;
    set_password(&pool, member, "memberpass").await;
    let non_member = fixture_account(&pool, &tag("outsider")).await;
    set_password(&pool, non_member, "outsiderpass").await;

    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Org')")
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
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) \
                 VALUES ($1,'owner','account',$2,'organization',$3,$2)",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();

    seed_repo(&pool, &state, org_id, owner, "public-repo", "public").await;
    seed_repo(&pool, &state, org_id, owner, "internal-repo", "internal").await;
    seed_repo(&pool, &state, org_id, owner, "private-repo", "private").await;

    let app = router(state);
    Fixture {
        app,
        pool,
        storage_root,
        org_slug,
        owner,
        member,
        non_member,
    }
}

async fn raw_get(app: &Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method("GET").uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
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

async fn clone_result(url: &str, dir: &str) -> bool {
    git_run(git_cmd(&["clone", "--", url, dir]))
        .await
        .status
        .success()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn http_visibility_matrix() {
    let fx = setup().await;
    let port = find_port_and_serve(&fx.app).await;

    let owner_user = username_of(&fx.pool, fx.owner).await;
    let member_user = username_of(&fx.pool, fx.member).await;
    let outsider_user = username_of(&fx.pool, fx.non_member).await;

    let urls = |user: &str, pass: &str, repo: &str| {
        format!(
            "http://{user}:{pass}@127.0.0.1:{port}/{}/{repo}.git",
            fx.org_slug
        )
    };
    let anon_url = |repo: &str| format!("http://127.0.0.1:{port}/{}/{repo}.git", fx.org_slug);

    let attempt = |url: String| async move {
        let dir = std::env::temp_dir().join(format!("vis-clone-{}", Uuid::new_v4()));
        let ok = clone_result(&url, dir.to_str().unwrap()).await;
        let _ = std::fs::remove_dir_all(&dir);
        ok
    };

    // Anonymous: public only.
    assert!(
        attempt(anon_url("public-repo")).await,
        "anonymous public clone"
    );
    assert!(
        !attempt(anon_url("internal-repo")).await,
        "anonymous internal clone"
    );
    assert!(
        !attempt(anon_url("private-repo")).await,
        "anonymous private clone"
    );

    // Member (org membership, no repo grant): public + internal.
    assert!(
        attempt(urls(&member_user, "memberpass", "public-repo")).await,
        "member public clone"
    );
    assert!(
        attempt(urls(&member_user, "memberpass", "internal-repo")).await,
        "member internal clone"
    );
    assert!(
        !attempt(urls(&member_user, "memberpass", "private-repo")).await,
        "member private clone"
    );

    // Non-member: public only.
    assert!(
        attempt(urls(&outsider_user, "outsiderpass", "public-repo")).await,
        "outsider public clone"
    );
    assert!(
        !attempt(urls(&outsider_user, "outsiderpass", "internal-repo")).await,
        "outsider internal clone"
    );
    assert!(
        !attempt(urls(&outsider_user, "outsiderpass", "private-repo")).await,
        "outsider private clone"
    );

    // Owner reads everything.
    assert!(attempt(urls(&owner_user, "ownerpass", "public-repo")).await);
    assert!(attempt(urls(&owner_user, "ownerpass", "internal-repo")).await);
    assert!(attempt(urls(&owner_user, "ownerpass", "private-repo")).await);

    fx.teardown().await;
}

fn browser_cookie(token: &str) -> String {
    format!("githearth_session={token}")
}

async fn browser_session(pool: &PgPool, account: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO sessions (token_hash,account_id,expires_at) \
         VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')",
    )
    .bind(&token)
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
    token
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn web_ui_visibility() {
    let fx = setup().await;
    let repo_path = |name: &str| format!("/organizations/{}/repositories/{name}", fx.org_slug);

    // Anonymous: public page renders, internal/private redirect to sign-in.
    let (status, body) = raw_get(&fx.app, &repo_path("public-repo"), &[]).await;
    assert_eq!(status, StatusCode::OK, "anonymous public page");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("public"),
        "public page shows visibility: {text}"
    );
    assert!(text.contains("public-repo"));

    for name in ["internal-repo", "private-repo"] {
        let (status, _) = raw_get(&fx.app, &repo_path(name), &[]).await;
        assert!(status.is_redirection(), "anonymous {name} should redirect");
    }

    // Member: public + internal render, private is not found.
    let token = browser_session(&fx.pool, fx.member).await;
    let cookie = browser_cookie(&token);
    for name in ["public-repo", "internal-repo"] {
        let (status, body) = raw_get(&fx.app, &repo_path(name), &[("cookie", &cookie)]).await;
        assert_eq!(status, StatusCode::OK, "member {name} page");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(name), "member {name} page names repo");
    }
    let (status, _) = raw_get(&fx.app, &repo_path("private-repo"), &[("cookie", &cookie)]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "member private page");

    // Non-member: only public renders; internal and private are not found.
    let outsider_token = browser_session(&fx.pool, fx.non_member).await;
    let outsider_cookie = browser_cookie(&outsider_token);
    let (status, _) = raw_get(
        &fx.app,
        &repo_path("public-repo"),
        &[("cookie", &outsider_cookie)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "outsider public page");
    for name in ["internal-repo", "private-repo"] {
        let (status, _) = raw_get(&fx.app, &repo_path(name), &[("cookie", &outsider_cookie)]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "outsider {name} page");
    }

    fx.teardown().await;
}
