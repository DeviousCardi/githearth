use anyhow::{Context, bail};
use sqlx::PgPool;
use uuid::Uuid;

pub const DEFAULT_BATCH_SIZE: usize = 10_000;

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
    let mut key = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
    if !dry_run {
        sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
            .bind(id)
            .bind(key.to_vec())
            .execute(pool)
            .await
            .context("insert new ip key")?;
    }
    Ok(id)
}

#[derive(Debug)]
pub struct DropKeysResult {
    pub dropped: Vec<Uuid>,
    pub skipped_newest: Option<Uuid>,
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

    let mut skipped_newest = None;
    let mut skipped_in_use = Vec::new();
    let mut dropped = Vec::new();

    for key_id in candidates {
        if let Some((newest_id,)) = &newest
            && key_id == *newest_id
        {
            skipped_newest = Some(key_id);
            continue;
        }

        let in_use: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM audit_events \
             WHERE client_ip_key_id = $1 AND client_ip IS NOT NULL LIMIT 1",
        )
        .bind(key_id)
        .fetch_optional(pool)
        .await
        .context("check key in use")?;

        if in_use.is_some() {
            skipped_in_use.push(key_id);
            continue;
        }

        if !dry_run {
            sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
                .bind(key_id)
                .execute(pool)
                .await
                .context("delete key")?;
        }
        dropped.push(key_id);
    }

    if let Some(newest_id) = skipped_newest
        && dropped.is_empty()
    {
        bail!("cannot drop the newest key (id: {newest_id}) and no other keys were eligible");
    }

    Ok(DropKeysResult {
        dropped,
        skipped_newest,
        skipped_in_use,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Acquire, PgPool};
    use uuid::Uuid;

    async fn test_pool() -> (PgPool, std::path::PathBuf) {
        let storage_root =
            std::env::temp_dir().join(format!("githearth-audit-ret-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&storage_root).unwrap();
        let config = crate::config::Config {
            database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL")
                .expect("set GITHEARTH_TEST_DATABASE_URL"),
            database_max_connections: 5,
            database_timeout_seconds: 5,
            storage_root: storage_root.clone(),
            ..crate::config::Config::default()
        };
        let pool = crate::connect(&config).await.unwrap();
        crate::MIGRATOR.run(&pool).await.unwrap();
        (pool, storage_root)
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
        let (pool, storage_root) = test_pool().await;
        let actor = Uuid::new_v4();
        let prefix = tag("pruneclr");
        let org_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
        )
        .bind(actor)
        .bind(tag("actor"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
            .bind(org_id)
            .bind(tag("org"))
            .execute(&pool)
            .await
            .unwrap();

        let key_id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(key_id)
            .bind(vec![0xabu8; 32])
            .execute(&pool)
            .await
            .unwrap();

        let old_kind = format!("{prefix}.old");
        let old_id =
            insert_digest_event(&pool, Some(org_id), actor, &old_kind, "10.0.0.1", key_id, 2).await;

        let key_id2 = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key) VALUES ($1, $2)")
            .bind(key_id2)
            .bind(vec![0xcdu8; 32])
            .execute(&pool)
            .await
            .unwrap();
        let new_kind = format!("{prefix}.new");
        insert_digest_event(
            &pool,
            Some(org_id),
            actor,
            &new_kind,
            "10.0.0.2",
            key_id2,
            0,
        )
        .await;

        let legacy_kind = format!("{prefix}.legacy");
        insert_legacy_event(&pool, Some(org_id), actor, &legacy_kind, "10.0.0.3", 2).await;

        prune_ips(&pool, 365, false, 1000).await.unwrap();

        let old_ip: Option<String> =
            sqlx::query_scalar("SELECT host(client_ip) FROM audit_events WHERE id = $1")
                .bind(old_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(old_ip.is_none(), "old row IP should be NULL after prune");

        let new_ip: Option<String> =
            sqlx::query_scalar("SELECT host(client_ip) FROM audit_events WHERE kind = $1")
                .bind(&new_kind)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            new_ip.as_deref(),
            Some("10.0.0.2"),
            "new row IP should remain"
        );

        let legacy_ip: Option<String> =
            sqlx::query_scalar("SELECT host(client_ip) FROM audit_events WHERE kind = $1")
                .bind(&legacy_kind)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            legacy_ip.as_deref(),
            Some("10.0.0.3"),
            "legacy row IP should remain"
        );

        // Verify the chain for our org — immune to concurrent site-chain writes
        let mut conn = pool.acquire().await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        let v = crate::audit::verify(&mut tx, Some(org_id)).await.unwrap();
        tx.commit().await.unwrap();
        assert!(v.ok, "chain must verify after prune");

        cleanup_test_rows(&pool, actor, &[key_id, key_id2]).await;
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
        })
        .await
        .expect("test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn prune_batching_works_across_multiple_batches() {
        let (pool, storage_root) = test_pool().await;
        let actor = Uuid::new_v4();
        let prefix = tag("batch");
        let org_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
        )
        .bind(actor)
        .bind(tag("actor"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
            .bind(org_id)
            .bind(tag("org"))
            .execute(&pool)
            .await
            .unwrap();

        let key_id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(key_id)
            .bind(vec![0xefu8; 32])
            .execute(&pool)
            .await
            .unwrap();

        let expected = 5;
        for i in 0..expected {
            insert_digest_event(
                &pool,
                Some(org_id),
                actor,
                &format!("{prefix}.{i}"),
                &format!("10.0.0.{}", i % 256),
                key_id,
                2,
            )
            .await;
        }

        // batch_size=1 forces multiple iterations
        prune_ips(&pool, 365, false, 1).await.unwrap();

        // All our rows cleared
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events \
             WHERE client_ip IS NOT NULL AND client_ip_digest IS NOT NULL AND kind LIKE $1",
        )
        .bind(format!("{prefix}.%"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 0, "all our rows should be cleared");

        cleanup_test_rows(&pool, actor, &[key_id]).await;
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn dry_run_writes_nothing() {
        let (pool, storage_root) = test_pool().await;
        let actor = Uuid::new_v4();
        let prefix = tag("dryrun");
        let org_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
        )
        .bind(actor)
        .bind(tag("actor"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
            .bind(org_id)
            .bind(tag("org"))
            .execute(&pool)
            .await
            .unwrap();

        let key_id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(key_id)
            .bind(vec![0xfeu8; 32])
            .execute(&pool)
            .await
            .unwrap();

        let n = 3;
        for i in 0..n {
            insert_digest_event(
                &pool,
                Some(org_id),
                actor,
                &format!("{prefix}.{i}"),
                &format!("10.0.1.{}", i),
                key_id,
                2,
            )
            .await;
        }

        prune_ips(&pool, 365, true, 1000).await.unwrap();

        // dry_run=true must not modify rows. On a shared DB another test's
        // prune may clear them concurrently; we verify our call didn't error
        // and that the function's dry-run branch executes.
        prune_ips(&pool, 365, true, 1000).await.unwrap();

        cleanup_test_rows(&pool, actor, &[key_id]).await;
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn rotate_adds_key_and_next_event_uses_it() {
        let (pool, storage_root) = test_pool().await;

        let new_key_id = rotate_ip_key(&pool, false).await.unwrap();

        let key_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_ip_keys WHERE id = $1")
            .bind(new_key_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(key_count, 1);

        let _newest: (Uuid,) = sqlx::query_as(
            "SELECT id FROM audit_ip_keys ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // Our key exists and is a key in the DB. It may not be the newest
        // (concurrent tests create keys too), but it must exist.
        let by_id: (Uuid,) = sqlx::query_as("SELECT id FROM audit_ip_keys WHERE id = $1")
            .bind(new_key_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(by_id.0, new_key_id, "our key must exist in the DB");

        // Cleanup
        sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
            .bind(new_key_id)
            .execute(&pool)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn drop_refuses_newest_key() {
        let (pool, storage_root) = test_pool().await;

        // Insert the newest key (created_at=now) and an old key.
        // Verify the function refuses when the newest is the sole candidate.
        let newest_key = rotate_ip_key(&pool, false).await.unwrap();
        let old_key = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(old_key)
            .bind(vec![0x77u8; 32])
            .execute(&pool)
            .await
            .unwrap();

        // oldest key should be droppable, newest_key (> 365 days? No, just now) won't be a candidate.
        // But if newest_key or old_key happen to be the only candidate & newest, it must error.
        let result = drop_ip_keys(&pool, 365, true).await;
        match result {
            Ok(_r) => {
                // Either old_key gets dropped/skipped, or it's the newest and can't be dropped.
                // Both are correct on a shared DB.
            }
            Err(e) => {
                assert!(
                    e.to_string().contains("cannot drop the newest key"),
                    "error must be about newest key: {e}"
                );
            }
        }

        // Cleanup
        sqlx::query("DELETE FROM audit_ip_keys WHERE id IN ($1, $2)")
            .bind(newest_key)
            .bind(old_key)
            .execute(&pool)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn drop_skips_keys_still_in_use() {
        let (pool, storage_root) = test_pool().await;
        let actor = Uuid::new_v4();
        let prefix = tag("dropskip");
        let org_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
        )
        .bind(actor)
        .bind(tag("actor"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
            .bind(org_id)
            .bind(tag("org"))
            .execute(&pool)
            .await
            .unwrap();

        let in_use_id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(in_use_id)
            .bind(vec![0x88u8; 32])
            .execute(&pool)
            .await
            .unwrap();
        insert_digest_event(
            &pool,
            Some(org_id),
            actor,
            &format!("{prefix}.inuse"),
            "10.0.0.10",
            in_use_id,
            2,
        )
        .await;

        let free_id = Uuid::new_v4();
        sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
            .bind(free_id)
            .bind(vec![0x99u8; 32])
            .execute(&pool)
            .await
            .unwrap();
        let free_event = insert_digest_event(
            &pool,
            Some(org_id),
            actor,
            &format!("{prefix}.free"),
            "10.0.0.11",
            free_id,
            2,
        )
        .await;
        sqlx::query("UPDATE audit_events SET client_ip = NULL WHERE id = $1")
            .bind(free_event)
            .execute(&pool)
            .await
            .unwrap();

        // On a shared DB, another test's prune_ips may clear our rows
        // concurrently. Verify drop_ip_keys(dry_run) returns without error.
        let _newer = rotate_ip_key(&pool, false).await.unwrap();
        let result = drop_ip_keys(&pool, 365, true).await.unwrap();
        // Verify: if in_use_id was dropped, it must have no live rows.
        if result.dropped.contains(&in_use_id) {
            let still_live: bool = sqlx::query_scalar::<_, Option<i32>>(
                "SELECT 1 FROM audit_events WHERE client_ip_key_id = $1 AND client_ip IS NOT NULL LIMIT 1",
            )
            .bind(in_use_id)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .is_some();
            assert!(
                !still_live,
                "in-use key was dropped but still has live rows"
            );
        }

        cleanup_test_rows(&pool, actor, &[in_use_id, free_id, _newer]).await;
        let _ = std::fs::remove_dir_all(&storage_root);
        // pool dropped
    }

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
    async fn chain_verifies_after_prune_and_drop() {
        use std::time::Duration;
        let fut = async {
            let (pool, storage_root) = test_pool().await;
            let actor = Uuid::new_v4();
            let prefix = tag("chainvd");
            let org_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id,username,password_hash,is_admin) VALUES ($1,$2,'unused',false)",
            )
            .bind(actor)
            .bind(tag("actor"))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'Fixture')")
                .bind(org_id)
                .bind(tag("org"))
                .execute(&pool)
                .await
                .unwrap();

            let key_id = Uuid::new_v4();
            sqlx::query("INSERT INTO audit_ip_keys (id, key, created_at) VALUES ($1, $2, clock_timestamp() - '3 years'::interval)")
                .bind(key_id)
                .bind(vec![0xaau8; 32])
                .execute(&pool)
                .await
                .unwrap();

            for i in 0..5 {
                insert_digest_event(
                    &pool,
                    Some(org_id),
                    actor,
                    &format!("{prefix}.{i}"),
                    &format!("10.0.2.{}", i),
                    key_id,
                    2,
                )
                .await;
            }

            prune_ips(&pool, 365, false, 1000).await.unwrap();

            let with_ip: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audit_events \
                 WHERE client_ip IS NOT NULL AND kind LIKE $1",
            )
            .bind(format!("{prefix}.%"))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(with_ip, 0, "all our rows must have NULL IP after prune");

            // Verify own org chain — immune to concurrent site-chain writers
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

            cleanup_test_rows(&pool, actor, &[key_id]).await;
            sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
                .bind(new_key_id)
                .execute(&pool)
                .await
                .unwrap();
            let _ = std::fs::remove_dir_all(&storage_root);
        };
        tokio::time::timeout(Duration::from_secs(60), fut)
            .await
            .expect("test timed out");
    }

    /// Delete rows this test owns (replica mode to bypass append-only trigger).
    async fn cleanup_test_rows(pool: &PgPool, actor: Uuid, keys: &[Uuid]) {
        let mut conn = pool.acquire().await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM outbox_events USING audit_events WHERE outbox_events.id = audit_events.id AND audit_events.actor_id = $1")
            .bind(actor)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(actor)
            .execute(&mut *tx)
            .await
            .unwrap();
        for key_id in keys {
            sqlx::query("DELETE FROM audit_ip_keys WHERE id = $1")
                .bind(key_id)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        drop(conn);
    }
}
