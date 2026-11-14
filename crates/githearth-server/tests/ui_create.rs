//! Browser organization and repository creation (PostgreSQL-backed).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, hash_password, router};
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::path::PathBuf;
use tower::ServiceExt;
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn fixture() -> (Router, PgPool, PathBuf, String) {
    let storage_root = std::env::temp_dir().join(format!("githearth-ui-create-{}", Uuid::new_v4()));
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
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(tag("creator"))
        .bind(hash_password("password123456").unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sessions (token_hash,account_id,expires_at) VALUES (sha256($1::bytea),$2,clock_timestamp() + interval '1 hour')")
        .bind(&token)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let app = router(AppState::new(pool.clone(), config).await.unwrap());
    (
        app,
        pool,
        storage_root,
        format!("githearth_session={token}"),
    )
}

fn hidden(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let rest = &html[html.find(&marker).unwrap() + marker.len()..];
    rest[..rest.find('"').unwrap()].to_owned()
}

async fn get(app: &Router, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
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
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        location,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

async fn read_member(pool: &PgPool, org_id: Uuid) -> String {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(tag("reader"))
        .bind(hash_password("password123456").unwrap())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(org_id)
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
    format!("githearth_session={token}")
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

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn create_organization_and_repository_through_the_browser() {
    let (app, pool, storage, cookie) = fixture().await;
    let org_slug = tag("ui-org");
    let repo_name = tag("ui-repo");

    // Signed out redirects to login.
    let (status, _) = get(&app, "/organizations/new", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = get(&app, "/organizations/any/repositories/new", None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (status, form) = get(&app, "/organizations/new", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("<label for=\"slug\">"), "{form}");
    let (status, location, body) = post_form(
        &app,
        "/organizations/new",
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("slug", &org_slug),
            ("name", "UI Created Org"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(location.unwrap(), format!("/organizations/{org_slug}"));
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(exists, 1);

    // The organization page offers repository creation.
    let (status, page) = get(&app, &format!("/organizations/{org_slug}"), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&format!("/organizations/{org_slug}/repositories/new")));

    let (status, form) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(form.contains("<label for=\"visibility\">"), "{form}");
    let (status, location, body) = post_form(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("idempotency_key", &hidden(&form, "idempotency_key")),
            ("name", &repo_name),
            ("description", "made in the browser"),
            ("visibility", "private"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(
        location.unwrap(),
        format!("/organizations/{org_slug}/repositories/{repo_name}")
    );
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE name = $1")
        .bind(&repo_name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(exists, 1);
    let (status, page) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/{repo_name}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&repo_name));

    // Missing CSRF is refused.
    let (status, _, _) = post_form(
        &app,
        "/organizations/new",
        &cookie,
        &[
            ("csrf_token", "wrong"),
            ("slug", "csrf-org"),
            ("name", "CSRF Org"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A read-only member sees the organization but not the creation link.
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(&org_slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let reader = read_member(&pool, org_id).await;
    let (status, page) = get(&app, &format!("/organizations/{org_slug}"), Some(&reader)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("New repository"), "{page}");

    // Duplicate names surface the API's 409 as a form page.
    let (_, form) = get(&app, "/organizations/new", Some(&cookie)).await;
    let (status, _, body) = post_form(
        &app,
        "/organizations/new",
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("slug", &org_slug),
            ("name", "Duplicate"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("Create organization"), "{body}");

    let (_, form) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        Some(&cookie),
    )
    .await;
    let (status, _, body) = post_form(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("idempotency_key", &hidden(&form, "idempotency_key")),
            ("name", &repo_name),
            ("description", ""),
            ("visibility", "private"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("Create repository"), "{body}");

    // NUL in a browser path is a styled 404, not a 503.
    let (status, _) = get(&app, "/organizations/%00", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/%00"),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Validation failures re-render the form with a 400, not a bare error.
    let (_, form) = get(&app, "/organizations/new", Some(&cookie)).await;
    let (status, _, body) = post_form(
        &app,
        "/organizations/new",
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("slug", "UPPERCASE"),
            ("name", "Bad Slug"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Create organization"), "{body}");

    // Repository validation and NUL paths.
    let (status, form) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = post_form(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("idempotency_key", &hidden(&form, "idempotency_key")),
            ("name", "bad name!"),
            ("description", ""),
            ("visibility", "private"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("Create repository"), "{body}");

    let (status, _, _) = post_form(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("idempotency_key", "bad key"),
            ("name", &tag("x-repo")),
            ("visibility", "private"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = get(&app, "/organizations/%00/repositories/new", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // `new` is reserved for these routes, so it cannot be an org slug or repo
    // name.
    let (_, form) = get(&app, "/organizations/new", Some(&cookie)).await;
    let (status, _, _) = post_form(
        &app,
        "/organizations/new",
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("slug", "new"),
            ("name", "Reserved"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, form) = get(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        Some(&cookie),
    )
    .await;
    let (status, _, _) = post_form(
        &app,
        &format!("/organizations/{org_slug}/repositories/new"),
        &cookie,
        &[
            ("csrf_token", &hidden(&form, "csrf_token")),
            ("idempotency_key", &hidden(&form, "idempotency_key")),
            ("name", "new"),
            ("visibility", "private"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown API paths and malformed query strings keep the JSON envelope.
    let (status, body) = get(&app, "/api/does-not-exist", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("\"error\""), "{body}");
    let (status, body) = get(&app, "/api/v1/organizations?after=abc", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("\"error\""), "{body}");

    let _ = std::fs::remove_dir_all(storage);
    pool.close().await;
}
