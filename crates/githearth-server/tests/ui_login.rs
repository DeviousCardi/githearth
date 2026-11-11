//! Browser login/navigation UI contract tests (PostgreSQL-backed).
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn fresh_db(name: &str) -> String {
    let url = std::env::var("GITHEARTH_TEST_DATABASE_URL")
        .expect("set GITHEARTH_TEST_DATABASE_URL to a disposable database");
    let (origin, _) = url.rsplit_once('/').expect("URL must end in database name");
    let maint = sqlx::PgPool::connect(&format!("{origin}/postgres"))
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&maint)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&maint)
        .await
        .unwrap();
    format!("{origin}/{name}")
}

async fn setup_with_secure(secure: bool) -> (Router, PgPool, String) {
    let db_name = format!("githearth_ui_{}", Uuid::new_v4().simple());
    let url = fresh_db(&db_name).await;
    let config = Config {
        database_url: url.clone(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        secure_cookies: secure,
        storage_root: std::env::temp_dir().join(format!(
            "githearth-ui-login-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        )),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let hash = hash_password("password123456").unwrap();
    let account_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,$3,false)",
    )
    .bind(account_id)
    .bind("alice")
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Acme Org')")
        .bind(org_id)
        .bind("acme")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(account_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(account_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,description,visibility,archived,status,operation_id,created_by) VALUES ($1,$2,$3,$4,'private',false,'active',$5,$6)")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind("repo1")
        .bind("<script>alert(1)</script>")
        .bind(Uuid::new_v4())
        .bind(account_id)
        .execute(&pool)
        .await
        .unwrap();
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, db_name)
}

async fn setup() -> (Router, PgPool, String) {
    setup_with_secure(true).await
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
    let body = format!(
        "username={}&password={}&csrf_token={}",
        username, password, csrf_token
    );
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

async fn get_page(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post_form(
    app: &Router,
    path: &str,
    cookie: Option<&str>,
    body: &str,
) -> (StatusCode, String, Option<String>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        set_cookie,
    )
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_sets_cookie_and_redirects() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
    );
    let (status, _, set_cookie) = post_form(&app, "/login", Some(&csrf_cookie), &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cookie = set_cookie.unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("githearth_session="));
    assert!(cookie.contains("Secure"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_csrf_missing_rejected() {
    let (app, pool, _db) = setup().await;
    let (status, body, _) = post_form(
        &app,
        "/login",
        None,
        "username=alice&password=password123456",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("Invalid CSRF token."));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_cookie_opt_out_not_secure() {
    let (app, pool, _db) = setup_with_secure(false).await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
    );
    let (_, _, set_cookie) = post_form(&app, "/login", Some(&csrf_cookie), &body).await;
    assert!(!set_cookie.unwrap().contains("Secure"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_failure_does_not_leak() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=wrongpassword&csrf_token={}",
        csrf_token
    );
    let (status, _, _) = post_form(&app, "/login", Some(&csrf_cookie), &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, body) = get_page(&app, "/login?error=1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Invalid username or password."));
    assert!(!body.contains("alice"));
    let (status, body, _) = post_form(
        &app,
        "/login",
        None,
        "username=alice&password=password123456&csrf_token=bad",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("Invalid CSRF token."));
    let (status, body) = get_page(&app, "/login?error=csrf", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Invalid CSRF token."));
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=nobody&password=password123456&csrf_token={}",
        csrf_token
    );
    let (status, _, _) = post_form(&app, "/login", Some(&csrf_cookie), &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn home_lists_orgs_with_cookie() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acme"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn suspended_account_loses_browser_session_access() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    assert_eq!(get_page(&app, "/", Some(&cookie)).await.0, StatusCode::OK);
    sqlx::query("UPDATE accounts SET suspended=true WHERE username='alice'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get_page(&app, "/", Some(&cookie)).await.0,
        StatusCode::SEE_OTHER
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn org_page_lists_readable_repo_and_escapes() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/organizations/acme", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("repo1"));
    assert!(!body.contains("script"));
    let (status, body) = get_page(
        &app,
        "/organizations/acme/repositories/repo1",
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!body.contains("<script>alert(1)</script>"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn authenticated_repository_page_renders() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(
        &app,
        "/organizations/acme/repositories/repo1",
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("repo1"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn unauthorized_repository_access_returns_not_found() {
    let (app, pool, _db) = setup().await;
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind("bob")
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = login(&app, "bob", "password123456").await;
    let (status, _) = get_page(
        &app,
        "/organizations/acme/repositories/repo1",
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn database_failure_returns_unavailable() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, _) = get_page(&app, "/login", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    pool.close().await;
    let (status, _) = get_page(&app, "/login", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // pool already closed
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn suspended_organization_hidden_from_navigation() {
    let (app, pool, _db) = setup().await;
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id,slug,name,suspended) VALUES ($1,$2,'Hidden Org',true)",
    )
    .bind(org_id)
    .bind("hiddenorg")
    .execute(&pool)
    .await
    .unwrap();
    let alice: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username='alice'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(alice)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Hidden Org"));
    assert!(!body.contains("/organizations/hiddenorg"));
    let (status, _) = get_page(&app, "/organizations/hiddenorg", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_page_displays_error_states() {
    let (app, pool, _db) = setup().await;
    for q in ["/login?error=1", "/login?error=csrf"] {
        let (status, _) = get_page(&app, q, None).await;
        assert_eq!(status, StatusCode::OK);
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn empty_navigation_states_render() {
    let (app, pool, _db) = setup().await;
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind("bob")
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = login(&app, "bob", "password123456").await;
    let (status, body) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No organizations yet."));
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Empty Org')")
        .bind(org_id)
        .bind("emptyorg")
        .execute(&pool)
        .await
        .unwrap();
    let alice: Uuid = sqlx::query_scalar("SELECT id FROM accounts WHERE username='alice'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(org_id)
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(alice)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/organizations/emptyorg", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No repositories you can read."));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn access_isolation() {
    let (app, pool, _db) = setup().await;
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind("bob")
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = login(&app, "bob", "password123456").await;
    let (status, body) = get_page(&app, "/organizations/acme", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("repo1"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn logout_requires_csrf() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, _, _) = post_form(&app, "/logout", Some(&cookie), "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cookie_cannot_authenticate_json_mutation() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/organizations")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slug":"x","name":"X"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn logout_success_clears_cookie() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let marker = "name=\"csrf_token\" value=\"";
    let pos = body.find(marker).unwrap() + marker.len();
    let rest = &body[pos..];
    let end = rest.find('"').unwrap();
    let csrf = &rest[..end];
    let (status, _, set_cookie) = post_form(
        &app,
        "/logout",
        Some(&cookie),
        &format!("csrf_token={}", csrf),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let cleared = set_cookie.unwrap();
    assert!(cleared.contains("Max-Age=0"));
    assert!(cleared.contains("Secure"));
    let (status, _) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_during_outage_is_unavailable_not_bad_credentials() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    pool.close().await;
    let (status, _, set_cookie) = post_form(
        &app,
        "/login",
        Some(&csrf_cookie),
        &format!("username=alice&password=password123456&csrf_token={csrf_token}"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(set_cookie.is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_page_reuses_csrf_cookie_and_sets_page_headers() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .header("cookie", &csrf_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(resp.headers()[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(resp.headers()[header::REFERRER_POLICY], "same-origin");
    let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_owned();
    for directive in [
        "default-src 'none'",
        "frame-ancestors 'none'",
        "form-action 'self'",
    ] {
        assert!(csp.contains(directive), "CSP {csp:?} lacks {directive}");
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&bytes).contains(&format!("value=\"{csrf_token}\"")));

    // Non-page responses get nosniff and Referrer-Policy too, without the page CSP.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(resp.headers()[header::REFERRER_POLICY], "same-origin");
    assert!(
        resp.headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_none()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn logout_with_expired_session_clears_cookie() {
    let (app, pool, _db) = setup().await;
    let cookie = login(&app, "alice", "password123456").await;
    let (_, body) = get_page(&app, "/", Some(&cookie)).await;
    let marker = "name=\"csrf_token\" value=\"";
    let rest = &body[body.find(marker).unwrap() + marker.len()..];
    let csrf = &rest[..rest.find('"').unwrap()];
    sqlx::query("DELETE FROM sessions")
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, set_cookie) = post_form(
        &app,
        "/logout",
        Some(&cookie),
        &format!("csrf_token={csrf}"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(set_cookie.unwrap().contains("githearth_session=; "));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn pages_declare_lang_labels_and_skip_link() {
    let (app, pool, _db) = setup().await;
    let (status, body) = get_page(&app, "/login", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<html lang=\"en\">"));
    assert!(body.contains("<a class=\"skip-link\" href=\"#main\">Skip to content</a>"));
    assert!(body.contains("<label for=\"username\">Username</label>"));
    assert!(body.contains("<label for=\"password\">Password</label>"));

    let cookie = login(&app, "alice", "password123456").await;
    let (status, body) = get_page(&app, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<html lang=\"en\">"));
    assert!(body.contains("<a class=\"skip-link\" href=\"#main\">Skip to content</a>"));
    assert!(body.contains("<main id=\"main\" tabindex=\"-1\">"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_error_is_tied_to_the_credential_fields() {
    let (app, pool, _db) = setup().await;
    let (status, body) = get_page(&app, "/login?error=1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("id=\"login-error\""));
    assert!(body.contains("aria-invalid=\"true\" aria-describedby=\"login-error\""));
    pool.close().await;
}
