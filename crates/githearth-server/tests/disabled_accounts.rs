//! M4.1 exit criterion: a disabled account's existing sessions and tokens stop
//! working on API and UI, and Git HTTP and SSH admission refuse it. Each
//! transport is exercised against the same disabled account.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{
    AppState, MIGRATOR, config::Config, connect, git_admission, router, ssh_keys,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Option<String>, Value) {
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn json_request(
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
    let (status, _, value) = send(app, builder.body(Body::from(body.to_string())).unwrap()).await;
    (status, value)
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, Option<String>) {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let (status, location, _) = send(app, builder.body(Body::empty()).unwrap()).await;
    (status, location)
}

fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {cred}")
}

struct Fixture {
    pool: PgPool,
    app: Router,
    state: AppState,
    _tmp: tempfile::TempDir,
    account: Uuid,
    username: String,
    password: String,
    session: String,
    org_slug: String,
    repo_name: String,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        storage_root: tmp.path().to_path_buf(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state.clone());

    let username = tag("member");
    let password = "member-secret-pass";
    let account = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,password_changed_at) \
         VALUES ($1,$2,$3,clock_timestamp())",
    )
    .bind(account)
    .bind(&username)
    .bind(githearth_server::hash_password(password).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let (status, session) = json_request(
        &app,
        "POST",
        "/api/v1/login",
        None,
        json!({"username": username, "password": password}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session = session["token"].as_str().unwrap().to_owned();

    // An organization and private repository the account owns, so authorization
    // would otherwise admit it on every transport.
    let org_id = Uuid::new_v4();
    let org_slug = tag("org");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Disabled Account Org')")
        .bind(org_id)
        .bind(&org_slug)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(account)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let repo_name = tag("repo");
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(&repo_name)
        .bind(account)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

    Fixture {
        pool,
        app,
        state,
        _tmp: tmp,
        account,
        username,
        password: password.to_owned(),
        session,
        org_slug,
        repo_name,
    }
}

async fn disable(pool: &PgPool, account: Uuid) {
    sqlx::query(
        "UPDATE accounts SET suspended = true, disabled_at = clock_timestamp(), \
         disabled_by = $1 WHERE id = $1",
    )
    .bind(account)
    .execute(pool)
    .await
    .unwrap();
}

async fn admit(
    f: &Fixture,
    username: &str,
    password: &str,
    want: git_admission::Want,
) -> StatusCode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        basic_auth(username, password).parse().unwrap(),
    );
    match git_admission::admit(&f.state, &headers, &f.org_slug, &f.repo_name, want).await {
        Ok(_) => StatusCode::OK,
        Err(e) => e.0,
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disabled_account_sessions_and_tokens_stop_working() {
    let f = fixture().await;
    // A personal access token created while the account is active.
    let (status, token_body) = json_request(
        &f.app,
        "POST",
        "/api/v1/user/tokens",
        Some(&f.session),
        json!({"name": tag("token"), "scopes": ["repo:write"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{token_body}");
    let token = token_body["token"].as_str().unwrap().to_owned();

    // Both credentials work before the disable.
    assert_eq!(
        json_request(&f.app, "GET", "/api/v1/me", Some(&f.session), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        json_request(&f.app, "GET", "/api/v1/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );

    disable(&f.pool, f.account).await;

    // The existing session and token are refused.
    assert_eq!(
        json_request(&f.app, "GET", "/api/v1/me", Some(&f.session), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        json_request(&f.app, "GET", "/api/v1/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // A new sign-in is refused too.
    assert_eq!(
        json_request(
            &f.app,
            "POST",
            "/api/v1/login",
            None,
            json!({"username": f.username, "password": f.password})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // The Git credential path and SSH admission refuse the same account.
    assert_eq!(
        admit(&f, &f.username, &f.password, git_admission::Want::Fetch).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        admit(&f, &f.username, &f.password, git_admission::Want::Push).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        admit(&f, "x-access-token", &token, git_admission::Want::Fetch).await,
        StatusCode::UNAUTHORIZED
    );

    // Re-enabling restores the account; the token (never revoked) works again.
    sqlx::query(
        "UPDATE accounts SET suspended=false, disabled_at=NULL, disabled_by=NULL WHERE id=$1",
    )
    .bind(f.account)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        json_request(&f.app, "GET", "/api/v1/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        admit(&f, &f.username, &f.password, git_admission::Want::Fetch).await,
        StatusCode::OK
    );

    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disabled_account_is_signed_out_of_the_ui() {
    let f = fixture().await;
    let cookie = format!("githearth_session={}", f.session);

    let (status, _) = get(&f.app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);

    disable(&f.pool, f.account).await;

    // The browser session is no longer honoured: pages redirect to sign-in.
    let (status, location) = get(&f.app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/login"));
    let (status, _) = get(&f.app, "/settings", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    f.pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn disabled_account_is_refused_by_ssh_admission() {
    let f = fixture().await;
    // A key row is enough: the SSH admission only looks the fingerprint up.
    let fingerprint = format!("SHA256:{}", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO ssh_keys (id, account_id, name, algorithm, public_key, fingerprint) \
         VALUES ($1,$2,'laptop','ed25519','ssh-ed25519 AAAA',$3)",
    )
    .bind(Uuid::new_v4())
    .bind(f.account)
    .bind(&fingerprint)
    .execute(&f.pool)
    .await
    .unwrap();

    let mut conn = f.pool.acquire().await.unwrap();
    assert!(
        ssh_keys::authenticate(&mut conn, &fingerprint)
            .await
            .unwrap()
            .is_some(),
        "an active account's key authenticates"
    );

    disable(&f.pool, f.account).await;

    assert!(
        ssh_keys::authenticate(&mut conn, &fingerprint)
            .await
            .unwrap()
            .is_none(),
        "a disabled account's key must not authenticate"
    );

    drop(conn);
    f.pool.close().await;
}
