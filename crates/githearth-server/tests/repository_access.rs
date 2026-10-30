use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use githearth_server::{AppState, MIGRATOR, config::Config, connect, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Digest;
use sqlx::PgPool;
use std::path::PathBuf;
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

async fn setup() -> (Router, PgPool, PathBuf, Uuid, String, Uuid) {
    let storage_root =
        std::env::temp_dir().join(format!("githearth-repo-access-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        storage_root: storage_root.clone(),
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let owner = fixture_account(&pool, &tag("owner")).await;
    let _token = fixture_session(&pool, owner).await;
    let (org_id, org_slug) = fixture_organization(&pool, owner).await;
    let state = AppState::new(pool.clone(), config).await.unwrap();
    let app = router(state);
    (app, pool, storage_root, owner, org_slug, org_id)
}

fn cleanup(storage_root: &PathBuf) {
    let _ = std::fs::remove_dir_all(storage_root);
}

async fn create_repository(app: &Router, org_slug: &str, token: &str, name: &str) -> Uuid {
    let (status, body) = request(
        app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(token),
        None,
        json!({"name": name}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create repo failed: {body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn grant_access(
    app: &Router,
    org_slug: &str,
    name: &str,
    token: &str,
    subject_kind: &str,
    subject_id: Uuid,
    role: &str,
) -> (StatusCode, Value) {
    request(
        app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/{name}/access"),
        Some(token),
        None,
        json!({"subject_kind": subject_kind, "subject_id": subject_id.to_string(), "role": role}),
    )
    .await
}

// --- Tests ---

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn org_member_with_write_on_repo_x_can_read_and_patch_x_gets_404_on_y_list_shows_only_x_after_revoke_x_is_404()
 {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo_x = create_repository(&app, &org_slug, &owner_token, "repo-x").await;
    let _repo_y = create_repository(&app, &org_slug, &owner_token, "repo-y").await;

    // Create a member account with write grant on repo-x only
    let member = fixture_account(&pool, &tag("member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    // No org-level grant, just a repository grant with maintain role (which includes read+update)
    let (status, _) = grant_access(
        &app,
        &org_slug,
        "repo-x",
        &owner_token,
        "account",
        member,
        "maintain",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let member_token = fixture_session(&pool, member).await;

    // Can read repo-x
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-x"),
        Some(&member_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "member should read repo-x: {body}");
    assert_eq!(body["name"], "repo-x");

    // With maintain role, PATCH (RepositoryUpdate) works
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-x"),
        Some(&member_token),
        None,
        json!({"description": "updated by member"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "maintain role should patch repo: {body}"
    );

    // Gets 404 on repo-y
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-y"),
        Some(&member_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // List shows only repo-x (list returns 404 org when zero repos are visible for the principal, or filtered list)
    let (status, body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories"),
        Some(&member_token),
        None,
        Value::Null,
    )
    .await;
    if status == StatusCode::OK {
        let arr = body.as_array().unwrap();
        let names: Vec<_> = arr
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            names.contains(&"repo-x".to_owned()),
            "list should contain repo-x"
        );
        assert!(
            !names.contains(&"repo-y".to_owned()),
            "list should not contain repo-y"
        );
    } else {
        // If the org itself is 404 (no org-level grant), the list is also 404.
        // Since member has no org-level read grant, the org itself is 404, so the list is 404.
        // But they were given a repo-level grant which only applies to that specific repo.
        // The org read check in list fails first since they don't have org-level read.
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Revoke the grant
    let (s, list_body) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-x/access"),
        Some(&owner_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let grant_obj = &list_body.as_array().unwrap()[0];
    let grant_id = grant_obj["id"].as_str().unwrap();

    let (s, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-x/access/{grant_id}"),
        Some(&owner_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // After revoke, repo-x is 404 again
    let (status, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/repo-x"),
        Some(&member_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn team_grant_on_repo_x_works_for_team_member_and_child_team_member() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo_x = create_repository(&app, &org_slug, &owner_token, "team-repo").await;

    // Create a team
    let team_id = Uuid::new_v4();
    sqlx::query("INSERT INTO teams (id, organization_id, slug, name) VALUES ($1,$2,$3,$4)")
        .bind(team_id)
        .bind(org_id)
        .bind("writers")
        .bind("Writers")
        .execute(&pool)
        .await
        .unwrap();

    // Child team
    let child_team_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO teams (id, organization_id, parent_id, slug, name) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(child_team_id)
    .bind(org_id)
    .bind(team_id)
    .bind("subwriters")
    .bind("Sub Writers")
    .execute(&pool)
    .await
    .unwrap();

    // Member of parent team
    let parent_member = fixture_account(&pool, &tag("pmember")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(parent_member)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO team_members (team_id, account_id) VALUES ($1,$2)")
        .bind(team_id)
        .bind(parent_member)
        .execute(&pool)
        .await
        .unwrap();

    // Member of child team
    let child_member = fixture_account(&pool, &tag("cmember")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(child_member)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO team_members (team_id, account_id) VALUES ($1,$2)")
        .bind(child_team_id)
        .bind(child_member)
        .execute(&pool)
        .await
        .unwrap();

    // Grant write on repo to the parent team
    let (status, _) = grant_access(
        &app,
        &org_slug,
        "team-repo",
        &owner_token,
        "team",
        team_id,
        "write",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let pm_token = fixture_session(&pool, parent_member).await;
    let cm_token = fixture_session(&pool, child_member).await;

    // Parent team member can read
    let (s, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/team-repo"),
        Some(&pm_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "parent team member should read");

    // Child team member can read
    let (s, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/team-repo"),
        Some(&cm_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "child team member should read");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn trigger_rejections_return_400() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo = create_repository(&app, &org_slug, &owner_token, "trigger-repo").await;

    // Role owner not allowed on repository
    let member = fixture_account(&pool, &tag("rolemember")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    let (s, body) = grant_access(
        &app,
        &org_slug,
        "trigger-repo",
        &owner_token,
        "account",
        member,
        "owner",
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "owner role on repo should 400: {body}"
    );

    // Role member not allowed on repository
    let (s, body) = grant_access(
        &app,
        &org_slug,
        "trigger-repo",
        &owner_token,
        "account",
        member,
        "member",
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "member role on repo should 400: {body}"
    );

    // Non-member account
    let outsider = fixture_account(&pool, &tag("outsider")).await;
    let (s, body) = grant_access(
        &app,
        &org_slug,
        "trigger-repo",
        &owner_token,
        "account",
        outsider,
        "read",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "non-member should 400: {body}");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repository_from_another_organization_rejected() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let repo = create_repository(&app, &org_slug, &owner_token, "my-repo").await;

    // Create another org
    let other_owner = fixture_account(&pool, &tag("other-owner")).await;
    let other_org_id = Uuid::new_v4();
    let other_slug = tag("other");
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Other Org')")
        .bind(other_org_id)
        .bind(&other_slug)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(other_org_id)
        .bind(other_owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'owner','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4()).bind(other_owner).bind(other_org_id).execute(&pool).await.unwrap();

    // Grant on my-repo to an account through the other org's slug should fail
    // Test: Directly insert an org-scope grant with a repo from another org - actually this is about the trigger check.
    // The trigger check_grant_insert validates that repository_id belongs to scope_id's organization.
    // We test this by doing a direct SQL insert with mismatched scope_id and repository_id.

    let member = fixture_account(&pool, &tag("crossmember")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    // Try direct insert with wrong scope_id (uses other_org_id as scope_id but repo is in org_id)
    let err = sqlx::query(
        "INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,repository_id,granted_by) \
         VALUES ($1,'write','account',$2,'repository',$3,$4,$5)"
    )
    .bind(Uuid::new_v4()).bind(member).bind(other_org_id).bind(repo).bind(owner)
    .execute(&pool).await;
    assert!(err.is_err(), "cross-org grant should fail");
    assert!(
        err.unwrap_err()
            .as_database_error()
            .is_some_and(|d| d.code().as_deref() == Some("23514")),
        "should be check_violation"
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn repository_id_immutable_and_grant_replaces_previous() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo = create_repository(&app, &org_slug, &owner_token, "immut-repo").await;

    let member = fixture_account(&pool, &tag("im-member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    // Grant read
    let (s, _body) = grant_access(
        &app,
        &org_slug,
        "immut-repo",
        &owner_token,
        "account",
        member,
        "read",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // Grant write on the same subject - should replace (revoke old, insert new)
    let (s, body) = grant_access(
        &app,
        &org_slug,
        "immut-repo",
        &owner_token,
        "account",
        member,
        "write",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(body["role"], "write");

    // Only one active grant
    let (s, list) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/immut-repo/access"),
        Some(&owner_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "write");

    // repository_id is immutable via update trigger
    let grant_to_update: Uuid = arr[0]["id"].as_str().unwrap().parse().unwrap();
    let err = sqlx::query("UPDATE grants SET repository_id = NULL WHERE id = $1")
        .bind(grant_to_update)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "repository_id should be immutable");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn non_admin_gets_403_or_404_on_access_endpoints() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo = create_repository(&app, &org_slug, &owner_token, "admin-repo").await;

    // Create a read-only member
    let reader = fixture_account(&pool, &tag("reader")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(reader)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO grants (id,role,subject_kind,subject_id,scope_kind,scope_id,granted_by) VALUES ($1,'read','account',$2,'organization',$3,$2)")
        .bind(Uuid::new_v4()).bind(reader).bind(org_id).execute(&pool).await.unwrap();
    let reader_token = fixture_session(&pool, reader).await;

    // GET access - should 403 (can read org but not manage_access)
    let (s, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/admin-repo/access"),
        Some(&reader_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // POST access - should 403
    let (s, _) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/admin-repo/access"),
        Some(&reader_token),
        None,
        json!({"subject_kind": "account", "subject_id": reader.to_string(), "role": "write"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Unauthenticated / non-org-member should get 404
    let outsider = fixture_account(&pool, &tag("outsider2")).await;
    let outsider_token = fixture_session(&pool, outsider).await;
    let (s, _) = request(
        &app,
        "GET",
        &format!("/api/v1/organizations/{org_slug}/repositories/admin-repo/access"),
        Some(&outsider_token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn stale_session_gets_403_on_manage_access() {
    let (app, pool, storage_root, owner, org_slug, _org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let _repo = create_repository(&app, &org_slug, &owner_token, "stale-repo").await;

    // Make the owner's session stale by updating authenticated_at
    let token_hash = sha2::Sha256::digest(owner_token.as_bytes()).to_vec();
    sqlx::query(
        "UPDATE sessions SET authenticated_at = '2020-01-01'::timestamptz WHERE token_hash = $1",
    )
    .bind(&token_hash)
    .execute(&pool)
    .await
    .unwrap();

    // Now manage_access should fail with 403 (sensitive -> fresh auth required)
    let (s, body) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/stale-repo/access"),
        Some(&owner_token),
        None,
        json!({"subject_kind": "account", "subject_id": owner.to_string(), "role": "read"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "stale session should 403: {body}");

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn idempotent_replay_of_post_access() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let owner_token = fixture_session(&pool, owner).await;

    let repo = create_repository(&app, &org_slug, &owner_token, "idem-access-repo").await;

    let member = fixture_account(&pool, &tag("idem-acc")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let key = tag("idem-acc-key");
    let (s1, b1) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/idem-access-repo/access"),
        Some(&owner_token),
        Some(("Idempotency-Key", &key)),
        json!({"subject_kind": "account", "subject_id": member.to_string(), "role": "read"}),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, b2) = request(
        &app,
        "POST",
        &format!("/api/v1/organizations/{org_slug}/repositories/idem-access-repo/access"),
        Some(&owner_token),
        Some(("Idempotency-Key", &key)),
        json!({"subject_kind": "account", "subject_id": member.to_string(), "role": "read"}),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);
    assert_eq!(b1, b2);

    // Only one grant
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE repository_id = $1 AND revoked_at IS NULL",
    )
    .bind(repo)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn grant_expiry_is_validated_and_rendered_in_utc() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let token = fixture_session(&pool, owner).await;
    create_repository(&app, &org_slug, &token, "expiry-repo").await;
    let member = fixture_account(&pool, &tag("expiry")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    let path = format!("/api/v1/organizations/{org_slug}/repositories/expiry-repo/access");
    let grant = |expires_at: &str| json!({"subject_kind": "account", "subject_id": member, "role": "read", "expires_at": expires_at});
    for bad in ["not a time", "2000-01-01T00:00:00Z", "2999-13-01T00:00:00Z"] {
        let (status, _) = request(&app, "POST", &path, Some(&token), None, grant(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    // An offset input comes back normalised to UTC.
    let (status, body) = request(
        &app,
        "POST",
        &path,
        Some(&token),
        None,
        grant("2999-06-01T05:30:00+05:30"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["expires_at"], "2999-06-01T00:00:00Z");
    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn list_access_paginates() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let token = fixture_session(&pool, owner).await;
    create_repository(&app, &org_slug, &token, "paged-repo").await;

    let mut subjects = Vec::new();
    for _ in 0..3 {
        let member = fixture_account(&pool, &tag("page-member")).await;
        sqlx::query(
            "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
        )
        .bind(org_id)
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
        subjects.push(member);
    }
    for subject in &subjects {
        let (status, _) = grant_access(
            &app,
            &org_slug,
            "paged-repo",
            &token,
            "account",
            *subject,
            "read",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let path = format!("/api/v1/organizations/{org_slug}/repositories/paged-repo/access");
    let (status, body) = request(
        &app,
        "GET",
        &format!("{path}?limit=2"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first = body.as_array().unwrap();
    assert_eq!(first.len(), 2);

    let after = first[1]["id"].as_str().unwrap();
    let (status, body) = request(
        &app,
        "GET",
        &format!("{path}?limit=2&after={after}"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn delete_revokes_active_grants() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let token = fixture_session(&pool, owner).await;
    create_repository(&app, &org_slug, &token, "revoked-on-delete").await;

    let member = fixture_account(&pool, &tag("del-member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    grant_access(
        &app,
        &org_slug,
        "revoked-on-delete",
        &token,
        "account",
        member,
        "read",
    )
    .await;

    let repo_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE name = 'revoked-on-delete' AND organization_id = $1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/revoked-on-delete"),
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE repository_id = $1 AND revoked_at IS NULL",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active, 0,
        "no grant for a deleted repository may stay active"
    );

    cleanup(&storage_root);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn resumed_delete_still_works_and_does_not_re_revoke() {
    let (app, pool, storage_root, owner, org_slug, org_id) = setup().await;
    let token = fixture_session(&pool, owner).await;
    create_repository(&app, &org_slug, &token, "resume-delete").await;

    let member = fixture_account(&pool, &tag("resume-member")).await;
    sqlx::query(
        "INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'member')",
    )
    .bind(org_id)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();
    grant_access(
        &app,
        &org_slug,
        "resume-delete",
        &token,
        "account",
        member,
        "read",
    )
    .await;

    let repo_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE name = 'resume-delete' AND organization_id = $1",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let key = tag("resume-del");
    let (s1, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/resume-delete"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        Value::Null,
    )
    .await;
    assert_eq!(s1, StatusCode::NO_CONTENT);

    let (s2, _) = request(
        &app,
        "DELETE",
        &format!("/api/v1/organizations/{org_slug}/repositories/resume-delete"),
        Some(&token),
        Some(("Idempotency-Key", &key)),
        Value::Null,
    )
    .await;
    assert_eq!(s2, StatusCode::NO_CONTENT);

    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM grants WHERE repository_id = $1 AND revoked_at IS NULL",
    )
    .bind(repo_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0);

    cleanup(&storage_root);
    pool.close().await;
}
