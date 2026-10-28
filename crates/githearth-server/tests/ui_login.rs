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

async fn setup() -> (Router, PgPool, String) {
    setup_with_secure(true).await
}

async fn setup_with_secure(secure: bool) -> (Router, PgPool, String) {
    let db_name = format!("githearth_ui_{}", Uuid::new_v4().simple());
    let url = fresh_db(&db_name).await;
    let config = Config {
        database_url: url.clone(),
        database_max_connections: 4,
        database_timeout_seconds: 3,
        secure_cookies: secure,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    // create admin
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
    // organization + membership + grant
    let org_id = Uuid::new_v4();
    let org_slug = "acme";
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Acme Org')")
        .bind(org_id)
        .bind(org_slug)
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
        .bind(Uuid::new_v4()).bind(account_id).bind(org_id).execute(&pool).await.unwrap();
    // repository
    sqlx::query("INSERT INTO repositories (id,organization_id,name,description,visibility,archived,status,operation_id,created_by) VALUES ($1,$2,$3,$4,'private',false,'active',$5,$6)")
        .bind(Uuid::new_v4()).bind(org_id).bind("repo1").bind("<script>alert(1)</script>").bind(Uuid::new_v4()).bind(account_id).execute(&pool).await.unwrap();

    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, db_name)
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
    let token = rest[..end].to_string();
    (cookie, token)
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("githearth_session="));
    // safe default: Secure present
    assert!(cookie.contains("Secure"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_csrf_missing_rejected() {
    let (app, pool, _db) = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=alice&password=password123456"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!cookie.contains("Secure"));
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(loc.contains("error=1"));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(loc)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Invalid username or password."));
    assert!(!html.contains("alice"));
    // rejected CSRF attempt renders its own error
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "username=alice&password=password123456&csrf_token=bad",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Invalid CSRF token."));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login?error=csrf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Invalid CSRF token."));
    // unknown user
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=nobody&password=password123456&csrf_token={}",
        csrf_token
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn home_lists_orgs_with_cookie() {
    let (app, pool, _db) = setup().await;
    // login
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("acme"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn org_page_lists_readable_repo_and_escapes() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/acme")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("repo1"));
    // description is not rendered on the organization page.
    assert!(!html.contains("script"));
    // Repository page renders the escaped description.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/acme/repositories/repo1")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn authenticated_repository_page_renders() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/acme/repositories/repo1")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("repo1"));
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
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=bob&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/acme/repositories/repo1")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn database_failure_returns_unavailable() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    pool.close().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/login")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
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
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(!html.contains("Hidden Org"));
    assert!(!html.contains("/organizations/hiddenorg"));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/hiddenorg")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn login_page_displays_error_states() {
    let (app, pool, _db) = setup().await;
    for q in ["/login?error=1", "/login?error=csrf"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(q)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn empty_navigation_states_render() {
    let (app, pool, _db) = setup().await;
    // empty organization list for a user without memberships
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind("bob")
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=bob&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("No organizations yet."));

    // empty repository list for an organization without readable repos
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
    // login as alice again (new session)
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/emptyorg")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("No repositories you can read."));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn access_isolation() {
    let (app, pool, _db) = setup().await;
    // create second user bob without org access
    let hash = hash_password("password123456").unwrap();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(Uuid::new_v4())
        .bind("bob")
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    // bob login
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=bob&password=password123456&csrf_token={}",
        csrf_token
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
    // bob cannot see acme org
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/organizations/acme")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(!html.contains("repo1"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn logout_requires_csrf() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    // logout without csrf
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(""))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // with csrf from derived token? Need compute. We'll fetch home? Instead test logout success via form with token extracted? Skip exact; test invalid enough.
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn cookie_cannot_authenticate_json_mutation() {
    let (app, pool, _db) = setup().await;
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    // attempt to create org via JSON API using only cookie (no Bearer)
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
    let (csrf_cookie, csrf_token) = fetch_csrf(&app).await;
    let body = format!(
        "username=alice&password=password123456&csrf_token={}",
        csrf_token
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
    // Extract CSRF token from rendered home page.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    let marker = "name=\"csrf_token\" value=\"";
    let pos = html
        .find(marker)
        .expect("csrf field present")
        .saturating_add(marker.len());
    let rest = &html[pos..];
    let end = rest.find('"').unwrap();
    let csrf = &rest[..end];
    let body = format!("csrf_token={}", csrf);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/logout")
                .header("cookie", &cookie)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cleared = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cleared.contains("Max-Age=0"));
    assert!(cleared.contains("Secure"));
    // after logout, home redirects
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    pool.close().await;
}
