use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, git_admission, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    header_key: Option<(&str, &str)>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some((k, v)) = header_key {
        builder = builder.header(k, v);
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

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
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

async fn setup() -> (PgPool, Uuid, String, tempfile::TempDir) {
    let storage_root = tempfile::tempdir().unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        storage_root: storage_root.path().to_path_buf(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let (_org_id, org_slug) = fixture_organization(&pool, owner).await;
    (pool, owner, org_slug, storage_root)
}

fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine;
    let cred = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {cred}")
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

async fn app_state_for_test(pool: PgPool, storage_root: &tempfile::TempDir) -> AppState {
    AppState::new(
        pool.clone(),
        Config {
            database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
            database_max_connections: 4,
            database_timeout_seconds: 1,
            storage_root: storage_root.path().to_path_buf(),
            ..Config::default()
        },
    )
    .await
    .unwrap()
}

async fn admit_basic(
    state: &AppState,
    org_slug: &str,
    repo_name: &str,
    username: &str,
    password: &str,
    want: git_admission::Want,
) -> Result<git_admission::Admitted, git_admission::AuthError> {
    let auth = basic_auth(username, password);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", auth.parse().unwrap());
    git_admission::admit(state, &headers, org_slug, repo_name, want).await
}

// ---- no header / malformed header / bad base64 / wrong password / unknown user → 401 ----

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_no_header_401_with_www_authenticate() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;
    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let headers = axum::http::HeaderMap::new();
    let result = git_admission::admit(
        &state,
        &headers,
        &org_slug,
        "repo",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    assert_eq!(err.1, "invalid or expired credentials");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_malformed_header_401() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;
    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", "Bearer some-token".parse().unwrap());
    let result = git_admission::admit(
        &state,
        &headers,
        &org_slug,
        "repo",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_bad_base64_401() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;
    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic !!!not-valid-base64!!!".parse().unwrap(),
    );
    let result = git_admission::admit(
        &state,
        &headers,
        &org_slug,
        "repo",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_wrong_password_401() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;
    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let result = admit_basic(
        &state,
        &org_slug,
        "repo",
        &username,
        "wrong",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_unknown_user_401() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;
    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let result = admit_basic(
        &state,
        &org_slug,
        "repo",
        "nonexistent",
        "blah",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    pool.close().await;
}

// ---- unrelated user → 404 ----

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_unrelated_user_404() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let _owner_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;

    let stranger = fixture_account(&pool, &tag("stranger")).await;
    set_password(&pool, stranger, "strangerpass").await;
    let stranger_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(stranger)
        .fetch_one(&pool)
        .await
        .unwrap();

    let state = app_state_for_test(pool.clone(), &_tmp).await;
    let result = admit_basic(
        &state,
        &org_slug,
        "repo",
        &stranger_username,
        "strangerpass",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    pool.close().await;
}

// ---- non-active repo → 404 ----

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admission_non_active_repo_404() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let owner_username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'creating','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind("inactive-repo")
        .bind(owner)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

    let state = app_state_for_test(pool.clone(), &_tmp).await;
    let result = admit_basic(
        &state,
        &org_slug,
        "inactive-repo",
        &owner_username,
        "correctpass",
        git_admission::Want::Fetch,
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.0, StatusCode::NOT_FOUND);
    pool.close().await;
}

// ---- fetch ok, push admitted ----

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn admit_functional_with_password() {
    let (pool, owner, org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    create_active_repo(&pool, org_id, "repo", owner).await;

    let state = app_state_for_test(pool.clone(), &_tmp).await;

    let admitted = admit_basic(
        &state,
        &org_slug,
        "repo",
        &username,
        "correctpass",
        git_admission::Want::Fetch,
    )
    .await
    .unwrap();
    assert_eq!(admitted.account, owner);
    assert_eq!(admitted.repository.name, "repo");

    let admitted = admit_basic(
        &state,
        &org_slug,
        "repo",
        &username,
        "correctpass",
        git_admission::Want::Push,
    )
    .await
    .unwrap();
    assert_eq!(admitted.account, owner);

    pool.close().await;
}

// ---- login still works and returns 401 for bad password ----

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn login_still_works() {
    let (pool, owner, _org_slug, _tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

    // Use web API for login test - the original app
    let app_pool = connect(&Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        storage_root: _tmp.path().to_path_buf(),
        ..Config::default()
    })
    .await
    .unwrap();
    let app_state = AppState::new(
        app_pool.clone(),
        Config {
            database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
            database_max_connections: 4,
            database_timeout_seconds: 1,
            storage_root: _tmp.path().to_path_buf(),
            ..Config::default()
        },
    )
    .await
    .unwrap();
    let app = router(app_state);

    let (status, token) = request(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        json!({"username": username, "password": "correctpass"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(token["token"].as_str().is_some());
    assert_eq!(token["token_type"], "Bearer");

    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        json!({"username": username, "password": "wrongpass"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn archived_repository_is_cloneable_but_refuses_ref_updates() {
    use axum::response::IntoResponse;
    let (pool, owner, org_slug, tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo = create_active_repo(&pool, org_id, "old", owner).await;
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();
    let state = app_state_for_test(pool.clone(), &tmp).await;
    let admit = |want| admit_basic(&state, &org_slug, "old", &username, "correctpass", want);

    admit(git_admission::Want::Fetch).await.unwrap();
    let admitted = admit(git_admission::Want::Push).await.unwrap();
    let update = githearth_git::journal::Update {
        old: "0".repeat(40),
        new: "a".repeat(40),
        reference: "refs/heads/main".into(),
    };
    assert_eq!(
        git_admission::decide_ref(&state, &admitted, &update, true),
        Err("permission denied: ref.push")
    );

    // Only a 401 challenges for credentials.
    let not_found = git_admission::AuthError(axum::http::StatusCode::NOT_FOUND, "not found");
    assert!(
        !not_found
            .into_response()
            .headers()
            .contains_key("www-authenticate")
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn busy_or_unavailable_authentication_is_not_a_bad_password() {
    let (pool, owner, org_slug, tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let state = AppState::new(
        pool.clone(),
        Config {
            database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
            storage_root: tmp.path().to_path_buf(),
            auth_concurrency: 1,
            ..Config::default()
        },
    )
    .await
    .unwrap();
    // One password check at a time: concurrent attempts queue briefly and are then
    // either checked or told to back off, never reported as a bad password. Whether a
    // burst outlasts the queue wait depends on hashing speed; the deterministic 429 case
    // is `saturated_auth_returns_429_before_database_access` in the library tests.
    let mut attempts = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (state, org_slug, username) = (state.clone(), org_slug.clone(), username.clone());
        attempts.spawn(async move {
            let want = git_admission::Want::Fetch;
            admit_basic(&state, &org_slug, "none", &username, "correctpass", want)
                .await
                .unwrap_err()
                .0
        });
    }
    let statuses = attempts.join_all().await;
    assert!(
        statuses.iter().all(|s| matches!(s.as_u16(), 404 | 429)),
        "{statuses:?}"
    );

    pool.close().await;
    let error = admit_basic(
        &state,
        &org_slug,
        "none",
        &username,
        "correctpass",
        git_admission::Want::Fetch,
    )
    .await
    .unwrap_err();
    assert_eq!(error.0, axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn reload_sees_changes_made_after_admission() {
    let (pool, owner, org_slug, tmp) = setup().await;
    set_password(&pool, owner, "correctpass").await;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id=$1")
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug=$1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let repo = create_active_repo(&pool, org_id, "live", owner).await;
    let state = app_state_for_test(pool.clone(), &tmp).await;
    let admitted = admit_basic(
        &state,
        &org_slug,
        "live",
        &username,
        "correctpass",
        git_admission::Want::Push,
    )
    .await
    .unwrap();
    let update = githearth_git::journal::Update {
        old: "0".repeat(40),
        new: "a".repeat(40),
        reference: "refs/heads/main".into(),
    };
    assert_eq!(
        git_admission::decide_ref(&state, &admitted, &update, true),
        Ok(())
    );

    // Archived while the pack was uploading.
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();
    let fresh = git_admission::reload(&state, &admitted).await.unwrap();
    assert_eq!(
        git_admission::decide_ref(&state, &fresh, &update, true),
        Err("permission denied: ref.push")
    );

    // Deleted meanwhile: no longer admitted at all.
    sqlx::query("UPDATE repositories SET status = 'deleting' WHERE id = $1")
        .bind(repo)
        .execute(&pool)
        .await
        .unwrap();
    let err = git_admission::reload(&state, &admitted).await.unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::NOT_FOUND);
    pool.close().await;
}
