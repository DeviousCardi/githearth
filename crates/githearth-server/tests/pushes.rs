use githearth_git::journal::{Kind, Record, Update};
use githearth_server::{MIGRATOR, pushes};
use sqlx::{ConnectOptions, PgPool};
use std::str::FromStr;
use uuid::Uuid;

async fn pool() -> PgPool {
    let cfg = sqlx::postgres::PgConnectOptions::from_str(
        &std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL"),
    )
    .unwrap()
    .disable_statement_logging();
    let pool = PgPool::connect_with(cfg).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    pool
}

fn tag(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn sha1(c: char) -> String {
    format!("{:0>40}", c.to_string().repeat(40))
}

fn sha256(c: char) -> String {
    format!("{:0>64}", c.to_string().repeat(64))
}

async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let org_id = Uuid::new_v4();
    let actor = Uuid::new_v4();
    let repo_id = Uuid::new_v4();
    sqlx::query("INSERT INTO accounts (id,username,password_hash) VALUES ($1,$2,'hash')")
        .bind(actor)
        .bind(tag("actor"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Test Org')")
        .bind(org_id)
        .bind(tag("org"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repositories (id,organization_id,name,created_by,operation_id,status,visibility) VALUES ($1,$2,$3,$4,$5,'active','private')")
        .bind(repo_id)
        .bind(org_id)
        .bind("test-repo")
        .bind(actor)
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    (org_id, actor, repo_id)
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn apply_once_true_and_rows_created() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let record = Record {
        sequence: 1,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(actor),
        updates: vec![Update {
            old: sha1('a'),
            new: sha1('b'),
            reference: "refs/heads/main".into(),
        }],
    };
    let applied = pushes::apply_committed(&mut conn, repo_id, &record)
        .await
        .unwrap();
    assert!(applied);

    let pc: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(repo_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(pc, 1);

    let rc: i64 = sqlx::query_scalar("SELECT count(*) FROM ref_updates WHERE repository_id=$1")
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(rc, 1);

    let ac: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE kind='repository.pushed' AND actor_id=$1",
    )
    .bind(actor)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(ac, 1);

    let oc: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE kind='repository.pushed' AND payload->>'repository_id' = $1",
    )
    .bind(repo_id.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(oc, 1);

    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind='repository.pushed' AND payload->>'repository_id' = $1")
            .bind(repo_id.to_string())            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(payload["repository_id"], repo_id.to_string());
    assert_eq!(payload["journal_sequence"], 1);
    assert_eq!(payload["actor_id"], actor.to_string());
    assert_eq!(payload["updates"].as_array().unwrap().len(), 1);

    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ja, 0, "apply_committed must not move the resume mark");

    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn apply_twice_no_duplicates() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let record = Record {
        sequence: 1,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(actor),
        updates: vec![Update {
            old: sha1('0'),
            new: sha1('1'),
            reference: "refs/heads/main".into(),
        }],
    };
    assert!(
        pushes::apply_committed(&mut conn, repo_id, &record)
            .await
            .unwrap()
    );
    assert!(
        !pushes::apply_committed(&mut conn, repo_id, &record)
            .await
            .unwrap()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(repo_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn out_of_order_keeps_highest_watermark() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let r5 = Record {
        sequence: 5,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(actor),
        updates: vec![Update {
            old: sha1('0'),
            new: sha1('5'),
            reference: "refs/heads/main".into(),
        }],
    };
    let r3 = Record {
        sequence: 3,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(actor),
        updates: vec![Update {
            old: sha1('0'),
            new: sha1('3'),
            reference: "refs/heads/feature".into(),
        }],
    };
    assert!(
        pushes::apply_committed(&mut conn, repo_id, &r5)
            .await
            .unwrap()
    );
    assert!(
        pushes::apply_committed(&mut conn, repo_id, &r3)
            .await
            .unwrap()
    );
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ja, 0, "out-of-order applies must not move the resume mark");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repository_pushes WHERE repository_id=$1")
            .bind(repo_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(count, 2);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn all_invalid_cases_table_driven() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let good_oid = sha1('g');

    let cases: Vec<(&str, Record)> = vec![
        (
            "wrong kind (Committed)",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Committed,
                actor: Some(actor),
                updates: vec![Update {
                    old: good_oid.clone(),
                    new: good_oid.clone(),
                    reference: "refs/heads/main".into(),
                }],
            },
        ),
        (
            "empty updates",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: Some(actor),
                updates: vec![],
            },
        ),
        (
            "no actor",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: None,
                updates: vec![Update {
                    old: good_oid.clone(),
                    new: good_oid.clone(),
                    reference: "refs/heads/main".into(),
                }],
            },
        ),
        (
            "bad old oid",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: Some(actor),
                updates: vec![Update {
                    old: "short".into(),
                    new: good_oid.clone(),
                    reference: "refs/heads/main".into(),
                }],
            },
        ),
        (
            "bad new oid",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: Some(actor),
                updates: vec![Update {
                    old: good_oid.clone(),
                    new: "short".into(),
                    reference: "refs/heads/main".into(),
                }],
            },
        ),
        (
            "mixed OID lengths",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: Some(actor),
                updates: vec![Update {
                    old: good_oid.clone(),
                    new: "b".repeat(64),
                    reference: "refs/heads/main".into(),
                }],
            },
        ),
        (
            "bad ref namespace",
            Record {
                sequence: 1,
                operation: Uuid::new_v4(),
                kind: Kind::Prepared,
                actor: Some(actor),
                updates: vec![Update {
                    old: good_oid.clone(),
                    new: good_oid,
                    reference: "refs/notes/commits".into(),
                }],
            },
        ),
    ];
    for (label, rec) in cases {
        let err = pushes::apply_committed(&mut conn, repo_id, &rec)
            .await
            .unwrap_err();
        assert!(
            matches!(err, pushes::PushError::Invalid(_)),
            "case '{label}' should be Invalid, got {err:?}"
        );
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn advance() {
    let pool = pool().await;
    let (_org, _actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    pushes::advance(&mut conn, repo_id, 7).await.unwrap();
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ja, 7);
    pushes::advance(&mut conn, repo_id, 3).await.unwrap();
    let ja: i64 = sqlx::query_scalar("SELECT journal_applied FROM repositories WHERE id=$1")
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(ja, 7);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn applied_through() {
    let pool = pool().await;
    let (_org, _actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        pushes::applied_through(&mut conn, repo_id).await.unwrap(),
        0
    );
    pushes::advance(&mut conn, repo_id, 42).await.unwrap();
    assert_eq!(
        pushes::applied_through(&mut conn, repo_id).await.unwrap(),
        42
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn sha256_oid_accepted() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let record = Record {
        sequence: 1,
        operation: Uuid::new_v4(),
        kind: Kind::Prepared,
        actor: Some(actor),
        updates: vec![Update {
            old: sha256('a'),
            new: sha256('b'),
            reference: "refs/heads/main".into(),
        }],
    };
    assert!(
        pushes::apply_committed(&mut conn, repo_id, &record)
            .await
            .unwrap()
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn check_constraints_in_database() {
    let pool = pool().await;
    let (_org, actor, repo_id) = fixture(&pool).await;

    // journal_sequence must be > 0 via CHECK
    let err = sqlx::query(
        "INSERT INTO repository_pushes (repository_id,journal_sequence,operation_id,actor_id) VALUES ($1,0,$2,$3)",
    )
    .bind(repo_id)
    .bind(Uuid::new_v4())
    .bind(actor)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514")),
        "CHECK constraint failed"
    );

    // ref_name must start with refs/heads/ or refs/tags/
    let seq = 99;
    sqlx::query("INSERT INTO repository_pushes (repository_id,journal_sequence,operation_id,actor_id) VALUES ($1,$2,$3,$4)")
        .bind(repo_id)
        .bind(seq)
        .bind(Uuid::new_v4())
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();
    let err = sqlx::query(
        "INSERT INTO ref_updates (repository_id,journal_sequence,ref_name,old_oid,new_oid) VALUES ($1,$2,'badref',$3,$4)",
    )
    .bind(repo_id)
    .bind(seq)
    .bind(sha1('a'))
    .bind(sha1('b'))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514")),
        "ref_name CHECK failed"
    );

    // oid must be 40 or 64 hex chars
    let err = sqlx::query(
        "INSERT INTO ref_updates (repository_id,journal_sequence,ref_name,old_oid,new_oid) VALUES ($1,$2,'refs/heads/main','bad',$3)",
    )
    .bind(repo_id)
    .bind(seq)
    .bind(sha1('b'))
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("23514")),
        "oid CHECK failed"
    );

    pool.close().await;
}
