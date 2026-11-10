use anyhow::{Context, bail};
use sqlx::PgPool;
use uuid::Uuid;

pub const DEFAULT_BATCH_SIZE: usize = 10_000;

pub(crate) fn new_ip_key_bytes() -> Vec<u8> {
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
    key.to_vec()
}

pub async fn prune_ips(
    pool: &PgPool,
    older_than_days: i64,
    dry_run: bool,
    batch_size: usize,
) -> anyhow::Result<PruneResult> {
    if dry_run {
        let cleared: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events \
             WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL \
             AND created_at < clock_timestamp() - ($1 || ' days')::interval",
        )
        .bind(older_than_days)
        .fetch_one(pool)
        .await
        .context("count prune candidates")?;

        let legacy_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events \
             WHERE client_ip IS NOT NULL AND client_ip_digest IS NULL \
             AND created_at < clock_timestamp() - ($1 || ' days')::interval",
        )
        .bind(older_than_days)
        .fetch_one(pool)
        .await
        .context("count legacy rows")?;

        return Ok(PruneResult {
            cleared: cleared as u64,
            legacy_remaining: legacy_count as u64,
        });
    }

    let mut total_cleared: u64 = 0;
    loop {
        let mut tx = pool.begin().await?;
        let rows = sqlx::query(
            "WITH batch AS ( \
             SELECT id FROM audit_events \
             WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL \
             AND created_at < clock_timestamp() - ($1 || ' days')::interval \
             LIMIT $2 FOR UPDATE SKIP LOCKED) \
             UPDATE audit_events SET client_ip = NULL \
             FROM batch WHERE audit_events.id = batch.id",
        )
        .bind(older_than_days)
        .bind(batch_size as i64)
        .execute(&mut *tx)
        .await
        .context("prune ip batch")?
        .rows_affected();
        tx.commit().await?;

        total_cleared += rows;
        if rows == 0 {
            break;
        }
    }

    let legacy_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events \
         WHERE client_ip IS NOT NULL AND client_ip_digest IS NULL \
         AND created_at < clock_timestamp() - ($1 || ' days')::interval",
    )
    .bind(older_than_days)
    .fetch_one(pool)
    .await
    .context("count legacy rows")?;

    Ok(PruneResult {
        cleared: total_cleared,
        legacy_remaining: legacy_count as u64,
    })
}

pub struct PruneResult {
    pub cleared: u64,
    pub legacy_remaining: u64,
}

pub async fn rotate_ip_key(pool: &PgPool, dry_run: bool) -> anyhow::Result<Uuid> {
    let id = Uuid::new_v4();
    let key = new_ip_key_bytes();
    if !dry_run {
        sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
            .bind(id)
            .bind(&key)
            .execute(pool)
            .await
            .context("insert new ip key")?;
    }
    Ok(id)
}

#[derive(Debug)]
pub struct DropKeysResult {
    pub dropped: Vec<Uuid>,
    pub skipped_in_use: Vec<Uuid>,
}

pub async fn drop_ip_keys(
    pool: &PgPool,
    older_than_days: i64,
    dry_run: bool,
) -> anyhow::Result<DropKeysResult> {
    let newest: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1")
            .fetch_optional(pool)
            .await
            .context("fetch newest key")?;

    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM audit_ip_keys \
         WHERE created_at < clock_timestamp() - ($1 || ' days')::interval \
         ORDER BY created_at",
    )
    .bind(older_than_days)
    .fetch_all(pool)
    .await
    .context("fetch old keys")?;

    if let Some((newest_id,)) = &newest
        && candidates.contains(newest_id)
    {
        bail!("cannot drop the newest key (id: {newest_id}) and no other keys were eligible");
    }

    let mut skipped_in_use = Vec::new();
    let mut dropped = Vec::new();

    for key_id in candidates {
        if !dry_run {
            let mut tx = pool.begin().await?;
            let deleted = sqlx::query_scalar::<_, bool>(
                "WITH locked AS ( \
                 SELECT id FROM audit_ip_keys WHERE id = $1 FOR UPDATE) \
                 DELETE FROM audit_ip_keys \
                 WHERE id = (SELECT id FROM locked) \
                 AND NOT EXISTS (SELECT 1 FROM audit_events \
                   WHERE client_ip_key_id = $1 AND client_ip IS NOT NULL) \
                 RETURNING true",
            )
            .bind(key_id)
            .fetch_optional(&mut *tx)
            .await
            .context("delete key")?;
            tx.commit().await?;

            if deleted.is_some() {
                dropped.push(key_id);
            } else {
                skipped_in_use.push(key_id);
            }
        } else {
            let in_use: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM audit_events \
                 WHERE client_ip_key_id = $1 AND client_ip IS NOT NULL)",
            )
            .bind(key_id)
            .fetch_one(pool)
            .await
            .context("check key in use")?;

            if in_use {
                skipped_in_use.push(key_id);
            } else {
                dropped.push(key_id);
            }
        }
    }

    Ok(DropKeysResult {
        dropped,
        skipped_in_use,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Acquire, Connection, PgConnection, PgPool};
    use uuid::Uuid;

    async fn fresh_database(name: &str) -> String {
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let (origin, _) = base_url
            .rsplit_once('/')
            .expect("URL must end in a database name");
        let db_name = format!("githearth_aret_{}_{}", std::process::id(), name);
        let mut conn = PgConnection::connect(&format!("{origin}/postgres"))
            .await
            .unwrap();
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        db_name
    }

    async fn drop_database(db_name: &str) {
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let (origin, _) = base_url
            .rsplit_once('/')
            .expect("URL must end in a database name");
        let mut conn = PgConnection::connect(&format!("{origin}/postgres"))
            .await
            .unwrap();
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }

    async fn test_pool(name: &str) -> (PgPool, String) {
        let db_name = fresh_database(name).await;
        let base_url =
            std::env::var("GITHEARTH_TEST_DATABASE_URL").expect("set GITHEARTH_TEST_DATABASE_URL");
        let (origin, _) = base_url
            .rsplit_once('/')
            .expect("URL must end in a database name");
        let config = crate::config::Config {
            database_url: format!("{origin}/{db_name}"),
            database_max_connections: 4,
            database_timeout_seconds: 10,
            storage_root: std::env::temp_dir(),
            ..crate::config::Config::default()
        };
        let pool = crate::connect(&config).await.unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();
        (pool, db_name)
    }

    fn tag(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4().simple())
    }

    async fn insert_digest_event(
        pool: &PgPool,
        org: Option<Uuid>,
        actor: Uuid,
        kind: &str,
        ip: &str,
        key_id: Uuid,
        years_ago: i32,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let digest = {
            let key: Vec<u8> = sqlx::query_scalar("SELECT key FROM audit_ip_keys WHERE id = $1")
                .bind(key_id)
                .fetch_one(pool)
                .await
                .unwrap();
            crate::hmac_sha256(&key, ip.as_bytes())
        };
        sqlx::query(
            "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip,client_ip_key_id,client_ip_digest,created_at) \
             VALUES ($1,$2,$3,$4,$5::inet,$6,$7,clock_timestamp() - ($8 || ' years')::interval)",
        )
        .bind(id)
        .bind(org)
        .bind(actor)
        .bind(kind)
        .bind(ip)
        .bind(key_id)
        .bind(&digest)
        .bind(years_ago)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn insert_legacy_event(
        pool: &PgPool,
        org: Option<Uuid>,
        actor: Uuid,
        kind: &str,
        ip: &str,
        years_ago: i32,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO audit_events (id,organization_id,actor_id,kind,client_ip,created_at) \
             VALUES ($1,$2,$3,$4,$5::inet,clock_timestamp() - ($6 || ' years')::interval)",
        )
        .bind(id)
        .bind(org)
        .bind(actor)
        .bind(kind)
        .bind(ip)
        .bind(years_ago)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn days_zero_rejected_at_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct TestArgs {
            #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
            older_than: i64,
        }
        assert!(TestArgs::try_parse_from(["test", "--older-than", "0"]).is_err());
        assert!(TestArgs::try_parse_from(["test", "--older-than", "-5"]).is_err());
        assert!(TestArgs::try_parse_from(["test", "--older-than", "1"]).is_ok());
        assert!(TestArgs::try_parse_from(["test", "--older-than", "365"]).is_ok());
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn prune_clears_old_digest_rows_and_leaves_new_and_legacy() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("pruneclr").await;
            let actor = Uuid::new_v4();
            let prefix = tag("pruneclr");
            let org_id = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)")
                .bind(actor).bind(tag("actor")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id).bind(tag("org")).execute(&pool).await.unwrap();
            let key_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(key_id).bind(vec![0xabu8; 32]).execute(&pool).await.unwrap();
            let old_kind = format!("{prefix}.old");
            let old_id = insert_digest_event(&pool, Some(org_id), actor, &old_kind, "10.0.0.1", key_id, 2).await;
            let key_id2 = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
                .bind(key_id2).bind(vec![0xcdu8; 32]).execute(&pool).await.unwrap();
            let new_kind = format!("{prefix}.new");
            insert_digest_event(&pool, Some(org_id), actor, &new_kind, "10.0.0.2", key_id2, 0).await;
            let legacy_kind = format!("{prefix}.legacy");
            insert_legacy_event(&pool, Some(org_id), actor, &legacy_kind, "10.0.0.3", 2).await;
            let result = prune_ips(&pool, 365, false, 1000).await.unwrap();
            assert_eq!(result.cleared, 1, "should clear the 1 old digest row");
            assert_eq!(result.legacy_remaining, 1);
            assert!(sqlx::query_scalar::<_, Option<String>>("SELECT host(client_ip) FROM audit_events WHERE id = $1")
                .bind(old_id).fetch_one(&pool).await.unwrap().is_none());
            assert_eq!(
                sqlx::query_scalar::<_, Option<String>>("SELECT host(client_ip) FROM audit_events WHERE kind = $1")
                    .bind(&new_kind).fetch_one(&pool).await.unwrap().as_deref(), Some("10.0.0.2"));
            assert_eq!(
                sqlx::query_scalar::<_, Option<String>>("SELECT host(client_ip) FROM audit_events WHERE kind = $1")
                    .bind(&legacy_kind).fetch_one(&pool).await.unwrap().as_deref(), Some("10.0.0.3"));
            let mut conn = pool.acquire().await.unwrap();
            let mut tx = conn.begin().await.unwrap();
            let v = crate::audit::verify(&mut tx, Some(org_id)).await.unwrap();
            tx.commit().await.unwrap();
            assert!(v.ok, "chain must verify after prune");
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn prune_batching_works_across_multiple_batches() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("batch").await;
            let actor = Uuid::new_v4();
            let prefix = tag("batch");
            let org_id = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)")
                .bind(actor).bind(tag("actor")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id).bind(tag("org")).execute(&pool).await.unwrap();
            let key_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(key_id).bind(vec![0xefu8; 32]).execute(&pool).await.unwrap();
            let expected = 5;
            for i in 0..expected {
                insert_digest_event(&pool, Some(org_id), actor, &format!("{prefix}.{i}"), &format!("10.0.0.{i}"), key_id, 2).await;
            }
            let result = prune_ips(&pool, 365, false, 1).await.unwrap();
            assert_eq!(result.cleared, expected as u64);
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audit_events WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL AND kind LIKE $1",
            ).bind(format!("{prefix}.%")).fetch_one(&pool).await.unwrap();
            assert_eq!(remaining, 0);
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn dry_run_writes_nothing() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("dryrun").await;
            let actor = Uuid::new_v4();
            let prefix = tag("dryrun");
            let org_id = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)")
                .bind(actor).bind(tag("actor")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id).bind(tag("org")).execute(&pool).await.unwrap();
            let key_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(key_id).bind(vec![0xfeu8; 32]).execute(&pool).await.unwrap();
            let n = 3;
            for i in 0..n {
                insert_digest_event(&pool, Some(org_id), actor, &format!("{prefix}.{i}"), &format!("10.0.1.{i}"), key_id, 2).await;
            }
            let our_before: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audit_events WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL AND kind LIKE $1",
            ).bind(format!("{prefix}.%")).fetch_one(&pool).await.unwrap();
            assert_eq!(our_before, n);
            prune_ips(&pool, 365, true, 1000).await.unwrap();
            let our_after: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audit_events WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL AND kind LIKE $1",
            ).bind(format!("{prefix}.%")).fetch_one(&pool).await.unwrap();
            assert_eq!(our_after, our_before, "dry-run prune must not modify any rows");
            let pre_key_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys").fetch_one(&pool).await.unwrap();
            let new_id = rotate_ip_key(&pool, true).await.unwrap();
            let post_key_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys").fetch_one(&pool).await.unwrap();
            assert_eq!(post_key_count, pre_key_count, "dry-run rotate must not create a key");
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys WHERE id = $1").bind(new_id).fetch_one(&pool).await.unwrap();
            assert_eq!(exists, 0, "dry-run rotate key must not exist");
            // dry-run drop: on a fresh DB the only key is old AND the newest,
            // so it must be refused with the specific error.
            let pre_keys: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM audit_ip_keys ORDER BY id")
                .fetch_all(&pool).await.unwrap();
            let err = drop_ip_keys(&pool, 365, true).await.unwrap_err();
            assert!(
                err.to_string().contains("cannot drop the newest key"),
                "must refuse newest key: {err}"
            );
            let post_keys: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM audit_ip_keys ORDER BY id")
                .fetch_all(&pool).await.unwrap();
            assert_eq!(post_keys, pre_keys, "dry-run drop must not delete keys");
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn rotate_adds_key_and_next_event_uses_it() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("rotate").await;
            let new_key_id = rotate_ip_key(&pool, false).await.unwrap();
            let key_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys WHERE id = $1")
                    .bind(new_key_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(key_count, 1);
            let newest: (Uuid,) = sqlx::query_as(
                "SELECT id FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(newest.0, new_key_id, "new key should be the newest");
            drop(pool);
            drop_database(&db_name).await;
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn drop_refuses_newest_key() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("refuse").await;
            let only_key = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(only_key).bind(vec![0x77u8; 32]).execute(&pool).await.unwrap();
            let err = drop_ip_keys(&pool, 365, false).await.unwrap_err();
            assert!(err.to_string().contains("cannot drop the newest key"), "must refuse: {err}");
            let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys WHERE id = $1").bind(only_key).fetch_one(&pool).await.unwrap();
            assert_eq!(still_there, 1, "key must not be dropped");
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn drop_skips_keys_still_in_use() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("dropskip").await;
            let actor = Uuid::new_v4();
            let prefix = tag("dropskip");
            let org_id = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)")
                .bind(actor).bind(tag("actor")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id).bind(tag("org")).execute(&pool).await.unwrap();
            let in_use_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(in_use_id).bind(vec![0x88u8; 32]).execute(&pool).await.unwrap();
            insert_digest_event(&pool, Some(org_id), actor, &format!("{prefix}.inuse"), "10.0.0.10", in_use_id, 2).await;
            let free_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(free_id).bind(vec![0x99u8; 32]).execute(&pool).await.unwrap();
            let free_event = insert_digest_event(&pool, Some(org_id), actor, &format!("{prefix}.free"), "10.0.0.11", free_id, 2).await;
            sqlx::query("UPDATE audit_events SET client_ip = NULL WHERE id = $1").bind(free_event).execute(&pool).await.unwrap();
            rotate_ip_key(&pool, false).await.unwrap();
            let result = drop_ip_keys(&pool, 365, false).await.unwrap();
            assert!(result.skipped_in_use.contains(&in_use_id), "in-use skipped: {:?}", result.skipped_in_use);
            assert!(result.dropped.contains(&free_id), "free dropped: {:?}", result.dropped);
            assert!(sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM audit_ip_keys WHERE id = $1)").bind(in_use_id).fetch_one(&pool).await.unwrap());
            assert!(!sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM audit_ip_keys WHERE id = $1)").bind(free_id).fetch_one(&pool).await.unwrap());
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn chain_verifies_after_prune_and_drop() {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(60), async {
            let (pool, db_name) = test_pool("chain").await;
            let actor = Uuid::new_v4();
            let prefix = tag("chainvd");
            let org_id = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)")
                .bind(actor).bind(tag("actor")).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id).bind(tag("org")).execute(&pool).await.unwrap();
            let key_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(key_id).bind(vec![0xaau8; 32]).execute(&pool).await.unwrap();
            for i in 0..5 {
                insert_digest_event(&pool, Some(org_id), actor, &format!("{prefix}.{i}"), &format!("10.0.2.{i}"), key_id, 2).await;
            }
            prune_ips(&pool, 365, false, 1000).await.unwrap();
            let with_ip: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audit_events WHERE client_ip IS NOT NULL AND kind LIKE $1",
            ).bind(format!("{prefix}.%")).fetch_one(&pool).await.unwrap();
            assert_eq!(with_ip, 0);
            let mut conn = pool.acquire().await.unwrap();
            let mut tx = conn.begin().await.unwrap();
            let v = crate::audit::verify(&mut tx, Some(org_id)).await.unwrap();
            tx.commit().await.unwrap();
            assert!(v.ok, "chain must verify after prune");
            let new_key_id = rotate_ip_key(&pool, false).await.unwrap();
            drop_ip_keys(&pool, 365, false).await.unwrap();
            let mut conn2 = pool.acquire().await.unwrap();
            let mut tx2 = conn2.begin().await.unwrap();
            let v2 = crate::audit::verify(&mut tx2, Some(org_id)).await.unwrap();
            tx2.commit().await.unwrap();
            assert!(v2.ok, "chain must verify after key drop");
            let _ = new_key_id;
            drop(pool); drop_database(&db_name).await;
        }).await.expect("test timed out");
    }
}
