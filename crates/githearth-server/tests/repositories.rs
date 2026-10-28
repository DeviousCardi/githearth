use githearth_server::MIGRATOR;
use githearth_server::config::Config;
use githearth_server::{connect, repositories};
use uuid::Uuid;

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

async fn pool() -> sqlx::PgPool {
    let config = Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
            .expect("set GITHEARTH_TEST_DATABASE_URL"),
        database_max_connections: 4,
        database_timeout_seconds: 1,
        ..Config::default()
    };
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    pool
}

async fn fixture_account(pool: &sqlx::PgPool, username: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'unused') RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture_organization(pool: &sqlx::PgPool, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture Org')")
        .bind(id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships (organization_id,account_id,role) VALUES ($1,$2,'owner')")
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn insert_creating_and_find() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let op_id = Uuid::new_v4();
    let mut conn = pool.acquire().await.unwrap();
    let row = repositories::insert_creating(&mut conn, org, "my-repo", owner, op_id)
        .await
        .unwrap();
    assert_eq!(row.name, "my-repo");
    assert_eq!(row.organization_id, org);
    assert_eq!(row.created_by, owner);
    assert_eq!(row.operation_id, op_id);
    assert_eq!(row.status, githearth_core::RepositoryStatus::Creating);
    assert!(!row.archived);
    assert_eq!(row.visibility, "private");
    assert_eq!(row.description, "");

    let found = repositories::find_by_operation(&mut conn, op_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row.id);

    let found = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(found.name, "my-repo");

    let found = repositories::find_by_name(&mut conn, org, "MY-REPO")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row.id);

    assert!(
        repositories::find_by_name(&mut conn, org, "nonexistent")
            .await
            .unwrap()
            .is_none()
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn name_check_rejections() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let long_name = "a".repeat(101);
    for name in ["", ".", "..", "a.git", "a/b", &long_name[..]] {
        let err = repositories::insert_creating(&mut conn, org, name, owner, Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(
            err.as_database_error()
                .is_some_and(|e| e.code().as_deref() == Some("23514")),
            "name '{name}' should be rejected by CHECK constraint"
        );
    }

    // valid names
    for name in ["valid-repo", "valid_repo", "Valid123", "x.y.z", "x"] {
        let row = repositories::insert_creating(&mut conn, org, name, owner, Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(row.name, name);
    }

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn name_uniqueness_case_insensitive() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    repositories::insert_creating(&mut conn, org, "My-Repo", owner, Uuid::new_v4())
        .await
        .unwrap();

    let err = repositories::insert_creating(&mut conn, org, "my-repo", owner, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.is_unique_violation())
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn deleted_name_reusable() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "reuse-me", owner, Uuid::new_v4())
        .await
        .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Active,
        githearth_core::RepositoryStatus::Deleting,
    )
    .await
    .unwrap();

    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Deleting,
        githearth_core::RepositoryStatus::Deleted,
    )
    .await
    .unwrap();

    let row2 = repositories::insert_creating(&mut conn, org, "reuse-me", owner, Uuid::new_v4())
        .await
        .unwrap();
    assert_ne!(row2.id, row.id);
    assert_eq!(row2.name, "reuse-me");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn status_transitions_allowed() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "transitions", owner, Uuid::new_v4())
        .await
        .unwrap();

    // creating -> active
    assert!(
        repositories::set_status(
            &mut conn,
            row.id,
            githearth_core::RepositoryStatus::Creating,
            githearth_core::RepositoryStatus::Active
        )
        .await
        .unwrap()
    );

    // active -> deleting
    assert!(
        repositories::set_status(
            &mut conn,
            row.id,
            githearth_core::RepositoryStatus::Active,
            githearth_core::RepositoryStatus::Deleting
        )
        .await
        .unwrap()
    );

    // deleting -> deleted
    assert!(
        repositories::set_status(
            &mut conn,
            row.id,
            githearth_core::RepositoryStatus::Deleting,
            githearth_core::RepositoryStatus::Deleted
        )
        .await
        .unwrap()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn status_transition_disallowed() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "nope", owner, Uuid::new_v4())
        .await
        .unwrap();

    // active -> creating is not allowed
    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    let err = repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Active,
        githearth_core::RepositoryStatus::Creating,
    )
    .await
    .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514")),
        "active -> creating should be rejected"
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn creating_to_failed_then_retry() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "retry-me", owner, Uuid::new_v4())
        .await
        .unwrap();

    // creating -> failed
    assert!(
        repositories::set_status(
            &mut conn,
            row.id,
            githearth_core::RepositoryStatus::Creating,
            githearth_core::RepositoryStatus::Failed,
        )
        .await
        .unwrap()
    );

    // failed -> creating (retry)
    assert!(
        repositories::set_status(
            &mut conn,
            row.id,
            githearth_core::RepositoryStatus::Failed,
            githearth_core::RepositoryStatus::Creating,
        )
        .await
        .unwrap()
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn failed_repository_can_be_deleted_and_name_reused() {
    use githearth_core::RepositoryStatus::{Creating, Deleted, Deleting, Failed};
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "stuck", owner, Uuid::new_v4())
        .await
        .unwrap();
    assert!(
        repositories::set_status(&mut conn, row.id, Creating, Failed)
            .await
            .unwrap()
    );
    let pending = repositories::pending(&mut conn, 1000).await.unwrap();
    assert!(
        pending.iter().any(|r| r.id == row.id),
        "failed rows are visible to the reconciler"
    );

    assert!(
        repositories::set_status(&mut conn, row.id, Failed, Deleting)
            .await
            .unwrap()
    );
    assert!(
        repositories::set_status(&mut conn, row.id, Deleting, Deleted)
            .await
            .unwrap()
    );
    repositories::insert_creating(&mut conn, org, "stuck", owner, Uuid::new_v4())
        .await
        .expect("name is free once the failed row is deleted");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cas_returns_false_on_wrong_from() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "cas-test", owner, Uuid::new_v4())
        .await
        .unwrap();

    // CAS from wrong state
    let matched = repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Active,
        githearth_core::RepositoryStatus::Deleting,
    )
    .await
    .unwrap();
    assert!(!matched);

    // state unchanged
    let found = repositories::get(&mut conn, row.id).await.unwrap().unwrap();
    assert_eq!(found.status, githearth_core::RepositoryStatus::Creating);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rename_with_redirect() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "old-name", owner, Uuid::new_v4())
        .await
        .unwrap();

    // Must be active to rename
    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    let renamed = repositories::rename(&mut conn, row.id, "new-name")
        .await
        .unwrap();
    assert!(renamed);

    // new name finds it
    let found = repositories::find_by_name(&mut conn, org, "new-name")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row.id);

    // old name redirects
    let found = repositories::find_by_name(&mut conn, org, "old-name")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row.id);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn rename_removes_redirect_on_name_reuse() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "first", owner, Uuid::new_v4())
        .await
        .unwrap();
    repositories::set_status(
        &mut conn,
        row.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    // Rename first -> second
    repositories::rename(&mut conn, row.id, "second")
        .await
        .unwrap();

    // Create new repo with "first" name
    let row2 = repositories::insert_creating(&mut conn, org, "first", owner, Uuid::new_v4())
        .await
        .unwrap();
    repositories::set_status(
        &mut conn,
        row2.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    // Rename row2 to third => the redirect for "first" is deleted
    repositories::rename(&mut conn, row2.id, "third")
        .await
        .unwrap();

    // "first" redirects to row2 (old name redirect for the renamed second repo)
    let found = repositories::find_by_name(&mut conn, org, "first")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row2.id);

    // "second" still finds the original via redirect
    let found = repositories::find_by_name(&mut conn, org, "second")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, row.id);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn pending_lists_creating_and_deleting() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let r1 = repositories::insert_creating(&mut conn, org, "p1", owner, Uuid::new_v4())
        .await
        .unwrap();
    let r2 = repositories::insert_creating(&mut conn, org, "p2", owner, Uuid::new_v4())
        .await
        .unwrap();

    repositories::set_status(
        &mut conn,
        r2.id,
        githearth_core::RepositoryStatus::Creating,
        githearth_core::RepositoryStatus::Active,
    )
    .await
    .unwrap();

    repositories::set_status(
        &mut conn,
        r2.id,
        githearth_core::RepositoryStatus::Active,
        githearth_core::RepositoryStatus::Deleting,
    )
    .await
    .unwrap();

    let pending = repositories::pending(&mut conn, 1000).await.unwrap();
    let ids: Vec<Uuid> = pending.iter().map(|r| r.id).collect();
    assert!(ids.contains(&r1.id), "creating should be pending");
    assert!(ids.contains(&r2.id), "deleting should be pending");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn operations_key_check() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;

    // valid key
    let hash = vec![0u8; 32];
    sqlx::query("INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1,'valid-key',$2,'running',now() + interval '1 day')")
        .bind(owner)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();

    // invalid: too long
    let err = sqlx::query("INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1,$2,$3,'running',now() + interval '1 day')")
        .bind(owner)
        .bind("x".repeat(129))
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514"))
    );

    // invalid: control char
    let err =
        sqlx::query("INSERT INTO operations (actor_id, key, request_hash, status, expires_at) VALUES ($1,'key\nname',$2,'running',now() + interval '1 day')")
            .bind(owner)
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514"))
    );

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn authz_repository_mapping() {
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();

    let row = repositories::insert_creating(&mut conn, org, "authz-repo", owner, Uuid::new_v4())
        .await
        .unwrap();
    let repo = repositories::authz_repository(&row);
    assert_eq!(repo.id, row.id);
    assert_eq!(repo.organization, org);
    assert_eq!(repo.visibility, githearth_authz::Visibility::Private);
    assert!(!repo.archived);

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn redirects_never_outlive_their_name() {
    use githearth_core::RepositoryStatus::{Active, Creating, Deleted, Deleting};
    let pool = pool().await;
    let owner = fixture_account(&pool, &tag("owner")).await;
    let org = fixture_organization(&pool, owner).await;
    let mut conn = pool.acquire().await.unwrap();
    let redirects = |name: &'static str| {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM repository_redirects WHERE organization_id=$1 AND name=$2",
        )
        .bind(org)
        .bind(name)
    };

    // Taking a redirected name retires the redirect, so deleting the new owner does not revive it.
    let old = repositories::insert_creating(&mut conn, org, "old", owner, Uuid::new_v4())
        .await
        .unwrap();
    assert!(
        repositories::rename(&mut conn, old.id, "moved")
            .await
            .unwrap()
    );
    let reuse = repositories::insert_creating(&mut conn, org, "old", owner, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(redirects("old").fetch_one(&mut *conn).await.unwrap(), 0);
    for (from, to) in [(Creating, Active), (Active, Deleting), (Deleting, Deleted)] {
        assert!(
            repositories::set_status(&mut conn, reuse.id, from, to)
                .await
                .unwrap()
        );
    }
    assert!(
        repositories::find_by_name(&mut conn, org, "old")
            .await
            .unwrap()
            .is_none()
    );

    // Renaming a deleted repository changes nothing, including redirects.
    assert!(
        !repositories::rename(&mut conn, reuse.id, "ghost")
            .await
            .unwrap()
    );
    assert_eq!(redirects("old").fetch_one(&mut *conn).await.unwrap(), 0);
    assert!(
        !repositories::rename(&mut conn, Uuid::new_v4(), "ghost")
            .await
            .unwrap()
    );
    pool.close().await;
}
