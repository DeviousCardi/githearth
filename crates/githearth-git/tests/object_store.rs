//! The filesystem object store behind the S3-shaped interface (ADR 0014 §1,
//! M4.2 increment 7A). Each case drives the interface exactly as a consumer
//! will: nothing here reads a node-local path except the corruption and torn-
//! tail cases, which need to simulate the damage a crash or a disk produces.

use githearth_git::object_store::{
    ByteRange, FsObjectStore, ObjectKey, ObjectProblemKind, ObjectStore, ObjectStoreError,
    ObjectStoreOptions, TenantId,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "githearth-objects-{}-{label}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn store(&self) -> FsObjectStore {
        FsObjectStore::open(self.0.join("objects")).unwrap()
    }

    fn store_with(&self, options: ObjectStoreOptions) -> FsObjectStore {
        FsObjectStore::open_with(self.0.join("objects"), options).unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::new(Uuid::new_v4())
    }

    fn key(text: &str) -> ObjectKey {
        ObjectKey::parse(text).unwrap()
    }

    /// The sidecar path a crash-orphan case needs to remove.
    fn meta_path(&self, tenant: TenantId, key: &str) -> PathBuf {
        self.0
            .join("objects/meta")
            .join(tenant.as_uuid().simple().to_string())
            .join(format!("{key}.json"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn put_get_stat_checksum_list_and_delete_round_trip() {
    let fixture = Fixture::new("round-trip");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/artifacts/app.tar");

    let meta = store.put(tenant, &key, b"artifact bytes").unwrap();
    assert_eq!(meta.bytes, 14);
    assert_eq!(meta.sha256, sha256(b"artifact bytes"));
    assert!(!meta.appendable);
    assert_eq!(store.get(tenant, &key, None).unwrap(), b"artifact bytes");
    assert_eq!(store.stat(tenant, &key).unwrap(), meta);
    assert_eq!(store.checksum(tenant, &key).unwrap(), meta.sha256);
    store.verify(tenant, &key).unwrap();

    let listed = store.list(tenant, "ci/").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);
    assert_eq!(listed[0].bytes, 14);
    assert_eq!(store.list(tenant, "ci/artifacts/").unwrap().len(), 1);
    assert!(store.list(tenant, "other/").unwrap().is_empty());

    // A put is an overwrite, not an error.
    let replaced = store.put(tenant, &key, b"new").unwrap();
    assert_eq!(replaced.bytes, 3);
    assert_eq!(store.get(tenant, &key, None).unwrap(), b"new");
    assert_eq!(store.list(tenant, "ci/").unwrap().len(), 1);

    // Delete is idempotent and removes the metadata with the bytes.
    store.delete(tenant, &key).unwrap();
    store.delete(tenant, &key).unwrap();
    assert!(store.list(tenant, "ci/").unwrap().is_empty());
    assert!(matches!(
        store.get(tenant, &key, None),
        Err(ObjectStoreError::NotFound { .. })
    ));
}

#[test]
fn ranged_read_crosses_a_chunk_boundary_and_verifies_both_chunks() {
    let fixture = Fixture::new("range");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("logs/run-1.log");
    let mut data = vec![b'a'; githearth_git::object_store::CHUNK_BYTES];
    data.extend_from_slice(b"01234567890123456789");
    store.put(tenant, &key, &data).unwrap();

    // Ten bytes before and ten bytes after the 1 MiB boundary.
    let boundary = githearth_git::object_store::CHUNK_BYTES as u64;
    let range = ByteRange::new(boundary - 10, boundary + 10).unwrap();
    let read = store.get(tenant, &key, Some(range)).unwrap();
    assert_eq!(
        read,
        data[(boundary - 10) as usize..=(boundary + 10) as usize]
    );

    let whole = store
        .get(
            tenant,
            &key,
            Some(ByteRange::new(0, data.len() as u64 - 1).unwrap()),
        )
        .unwrap();
    assert_eq!(whole, data);

    let start = data.len() as u64 - 2;
    assert_eq!(
        store
            .get(
                tenant,
                &key,
                Some(ByteRange::new(start, data.len() as u64 - 1).unwrap())
            )
            .unwrap(),
        b"89"
    );
    assert!(matches!(
        store.get(
            tenant,
            &key,
            Some(ByteRange::new(start, data.len() as u64).unwrap())
        ),
        Err(ObjectStoreError::RangeNotSatisfiable { .. })
    ));
}

#[test]
fn an_appended_log_reopens_correctly_and_a_torn_tail_is_dropped() {
    let fixture = Fixture::new("append");
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/logs/run-7.log");

    {
        let store = fixture.store();
        let meta = store.create_appendable(tenant, &key).unwrap();
        assert!(meta.appendable);
        store.append(tenant, &key, b"line one\n").unwrap();
        store.append(tenant, &key, b"line two\n").unwrap();
    }

    // Restart: a fresh handle over the same root.
    let store = fixture.store();
    assert_eq!(
        store.get(tenant, &key, None).unwrap(),
        b"line one\nline two\n"
    );
    store.append(tenant, &key, b"line three\n").unwrap();

    // A crash can leave bytes past the committed length. They were never
    // acknowledged, so the next append truncates them and starts clean.
    let path = store.data_path(tenant, &key);
    let committed = fs::metadata(&path).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"torn-tail")
        .unwrap();
    drop(store);
    let store = fixture.store();
    store.append(tenant, &key, b"line four\n").unwrap();
    let data = store.get(tenant, &key, None).unwrap();
    assert_eq!(
        data, b"line one\nline two\nline three\nline four\n",
        "the uncommitted tail must not survive"
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), data.len() as u64);
    assert!(committed < data.len() as u64);
}

#[test]
fn a_corrupted_object_is_detected_and_refused() {
    let fixture = Fixture::new("corrupt");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("artifacts/bad.bin");
    store.put(tenant, &key, b"the original bytes").unwrap();

    let path = store.data_path(tenant, &key);
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&path, &bytes).unwrap();

    let error = store.get(tenant, &key, None).unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::ChecksumMismatch { .. }),
        "{error}"
    );
    assert!(store.verify(tenant, &key).is_err());
    assert!(matches!(
        store.get(tenant, &key, Some(ByteRange::new(0, 1).unwrap())),
        Err(ObjectStoreError::ChecksumMismatch { .. })
    ));
    let problems = store.verify_all().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].key, key.as_str());
}

#[test]
fn an_abandoned_upload_is_cleaned_up_after_its_deadline() {
    let fixture = Fixture::new("abandoned");
    let store = fixture.store_with(ObjectStoreOptions {
        upload_deadline: Duration::from_secs(60),
        quota_bytes: 0,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/artifacts/large.bin");
    let upload = store.create_multipart(tenant, &key).unwrap();
    store
        .upload_part(tenant, &key, &upload, 1, b"unfinished")
        .unwrap();
    assert_eq!(store.usage(tenant).unwrap().upload_bytes, 10);

    let created = now();
    assert!(
        store.cleanup_abandoned(created).unwrap().is_empty(),
        "an upload inside its deadline must stay resumeable"
    );
    let cleaned = store.cleanup_abandoned(created + 61).unwrap();
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].upload, upload);
    assert_eq!(cleaned[0].tenant, tenant);
    assert_eq!(cleaned[0].key, key);
    assert_eq!(store.usage(tenant).unwrap().upload_bytes, 0);
    assert!(matches!(
        store.list_parts(tenant, &key, &upload),
        Err(ObjectStoreError::InvalidUpload(_))
    ));
}

#[test]
fn a_resumed_upload_completes_once_without_duplication() {
    let fixture = Fixture::new("resume");
    let tenant = Fixture::tenant();
    let key = Fixture::key("artifacts/pack-1.bin");
    let upload;
    {
        let store = fixture.store();
        upload = store.create_multipart(tenant, &key).unwrap();
        store
            .upload_part(tenant, &key, &upload, 1, b"first/")
            .unwrap();
        store
            .upload_part(tenant, &key, &upload, 2, b"second/")
            .unwrap();
    }

    // Resume after a restart: the parts are listed from disk.
    let store = fixture.store();
    let parts = store.list_parts(tenant, &key, &upload).unwrap();
    assert_eq!(parts.len(), 2);
    // Re-uploading a part replaces it; it is never appended a second time.
    store
        .upload_part(tenant, &key, &upload, 2, b"second/")
        .unwrap();
    store
        .upload_part(tenant, &key, &upload, 3, b"third")
        .unwrap();
    assert_eq!(store.list_parts(tenant, &key, &upload).unwrap().len(), 3);

    let meta = store
        .complete_multipart(tenant, &key, &upload, None)
        .unwrap();
    assert_eq!(meta.bytes, 18);
    assert_eq!(
        store.get(tenant, &key, None).unwrap(),
        b"first/second/third"
    );
    // The upload is gone; a duplicate completion cannot add the bytes again.
    assert!(matches!(
        store.complete_multipart(tenant, &key, &upload, None),
        Err(ObjectStoreError::InvalidUpload(_))
    ));
    assert!(matches!(
        store.list_parts(tenant, &key, &upload),
        Err(ObjectStoreError::InvalidUpload(_))
    ));

    let duplicate = store.create_multipart(tenant, &key).unwrap();
    store
        .upload_part(tenant, &key, &duplicate, 1, b"one")
        .unwrap();
    store
        .upload_part(tenant, &key, &duplicate, 1, b"one")
        .unwrap();
    assert!(matches!(
        store.complete_multipart(tenant, &key, &duplicate, Some(&[1, 1])),
        Err(ObjectStoreError::InvalidUpload(_))
    ));
}

#[test]
fn one_tenant_cannot_read_or_delete_another_tenants_prefix() {
    let fixture = Fixture::new("tenants");
    let store = fixture.store();
    let first = Fixture::tenant();
    let second = Fixture::tenant();
    let key = Fixture::key("ci/logs/shared-name.log");

    store.put(first, &key, b"first tenant").unwrap();
    store.put(second, &key, b"second tenant").unwrap();
    assert_eq!(store.get(first, &key, None).unwrap(), b"first tenant");
    assert_eq!(store.get(second, &key, None).unwrap(), b"second tenant");

    // The same key name is a different object in each tenant namespace.
    assert_eq!(store.list(first, "ci/").unwrap().len(), 1);
    assert_eq!(store.list(second, "ci/").unwrap().len(), 1);
    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(
        inventory
            .iter()
            .any(|record| record.tenant == first.as_uuid())
    );

    store.delete(first, &key).unwrap();
    assert_eq!(store.get(second, &key, None).unwrap(), b"second tenant");
    assert!(matches!(
        store.get(first, &key, None),
        Err(ObjectStoreError::NotFound { .. })
    ));
}

#[test]
fn quota_counts_committed_objects_and_open_upload_parts() {
    let fixture = Fixture::new("quota");
    let store = fixture.store_with(ObjectStoreOptions {
        upload_deadline: Duration::from_secs(60),
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    let first = Fixture::key("ci/one.bin");
    let second = Fixture::key("ci/two.bin");

    store.put(tenant, &first, &[0u8; 60]).unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 60);
    let error = store.put(tenant, &second, &[0u8; 50]).unwrap_err();
    assert!(
        matches!(
            error,
            ObjectStoreError::QuotaExceeded {
                used: 60,
                incoming: 50,
                limit: 100
            }
        ),
        "{error}"
    );
    store.delete(tenant, &first).unwrap();
    store.put(tenant, &second, &[0u8; 90]).unwrap();

    // Committed part bytes count until the upload is completed or swept.
    let upload = store.create_multipart(tenant, &second).unwrap();
    let error = store
        .upload_part(tenant, &second, &upload, 1, &[0u8; 20])
        .unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::QuotaExceeded { .. }),
        "{error}"
    );
    assert_eq!(store.usage(tenant).unwrap().bytes, 90);
    assert_eq!(store.usage(tenant).unwrap().upload_bytes, 0);
}

#[test]
fn an_immutable_object_cannot_be_turned_into_a_log() {
    let fixture = Fixture::new("appendable");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/not-a-log");
    store.put(tenant, &key, b"bytes").unwrap();
    assert!(matches!(
        store.create_appendable(tenant, &key),
        Err(ObjectStoreError::Conflict(_))
    ));
    assert!(matches!(
        store.append(tenant, &key, b"more"),
        Err(ObjectStoreError::Conflict(_))
    ));
}

#[test]
fn a_backup_style_inventory_verifies_the_whole_store() {
    let fixture = Fixture::new("inventory");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    store
        .put(tenant, &Fixture::key("ci/artifacts/a"), b"a")
        .unwrap();
    store
        .put(tenant, &Fixture::key("ci/artifacts/b"), b"bb")
        .unwrap();
    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.len(), 2);
    assert_eq!(inventory[0].key, "ci/artifacts/a");
    assert!(store.verify_all().unwrap().is_empty());

    // A key that is only metadata is an inconsistent store, not an empty one.
    let path = store.data_path(tenant, &Fixture::key("ci/artifacts/b"));
    fs::remove_file(&path).unwrap();
    let problems = store.verify_all().unwrap();
    assert!(
        problems
            .iter()
            .any(|problem| problem.message.contains("payload is missing")),
        "{problems:?}"
    );
}

#[test]
fn an_append_to_a_corrupted_log_is_refused_and_changes_nothing() {
    let fixture = Fixture::new("append-corrupt");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/logs/run-8.log");
    store.create_appendable(tenant, &key).unwrap();
    store.append(tenant, &key, b"line one\n").unwrap();

    let path = store.data_path(tenant, &key);
    let mut bytes = fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    let sidecar = fs::read(fixture.meta_path(tenant, key.as_str())).unwrap();

    // Appending to corrupt bytes must refuse them, not re-digest and bless
    // them into the sidecar (B1).
    let error = store.append(tenant, &key, b"line two\n").unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::ChecksumMismatch { .. }),
        "{error}"
    );
    assert_eq!(
        fs::read(fixture.meta_path(tenant, key.as_str())).unwrap(),
        sidecar,
        "the sidecar must not be rewritten from corrupt bytes"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the corrupt payload must not be truncated or extended"
    );
    assert!(matches!(
        store.get(tenant, &key, None),
        Err(ObjectStoreError::ChecksumMismatch { .. })
    ));
}

#[test]
fn an_append_over_the_quota_is_refused_and_leaves_nothing() {
    let fixture = Fixture::new("append-quota");
    let store = fixture.store_with(ObjectStoreOptions {
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/logs/run-9.log");
    store.create_appendable(tenant, &key).unwrap();
    store.append(tenant, &key, &[b'a'; 60]).unwrap();
    let path = store.data_path(tenant, &key);
    let sidecar = fs::read(fixture.meta_path(tenant, key.as_str())).unwrap();

    let error = store.append(tenant, &key, &[b'b'; 50]).unwrap_err();
    assert!(
        matches!(
            error,
            ObjectStoreError::QuotaExceeded {
                used: 60,
                incoming: 50,
                limit: 100
            }
        ),
        "{error}"
    );
    assert_eq!(store.usage(tenant).unwrap().bytes, 60);
    assert_eq!(fs::metadata(&path).unwrap().len(), 60);
    assert_eq!(
        fs::read(fixture.meta_path(tenant, key.as_str())).unwrap(),
        sidecar
    );
    // An append that exactly fits is still accepted.
    store.append(tenant, &key, &[b'c'; 40]).unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);
}

#[test]
fn concurrent_writes_cannot_exceed_the_quota() {
    let fixture = Fixture::new("quota-race");
    let store = Arc::new(fixture.store_with(ObjectStoreOptions {
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    }));
    let tenant = Fixture::tenant();
    store
        .put(tenant, &Fixture::key("ci/seed"), &[0u8; 10])
        .unwrap();

    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|index| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let key = ObjectKey::parse(&format!("ci/writer-{index}")).unwrap();
                barrier.wait();
                store.put(tenant, &key, &[1u8; 40]).map(|_| ())
            })
        })
        .collect();
    let accepted = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().is_ok())
        .filter(|accepted| *accepted)
        .count();

    // 10 already used, 40 each: exactly two fit; the rest are refused before
    // writing and leave nothing behind.
    assert_eq!(accepted, 2);
    assert_eq!(store.usage(tenant).unwrap().bytes, 90);
}

#[test]
fn a_payload_without_a_sidecar_does_not_break_listing_or_inventory() {
    let fixture = Fixture::new("orphan");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let kept = Fixture::key("ci/artifacts/kept");
    let orphan = Fixture::key("ci/artifacts/orphan");
    let partial = Fixture::key("ci/artifacts/partial");
    store.put(tenant, &kept, b"kept").unwrap();
    store.put(tenant, &orphan, b"orphan bytes").unwrap();
    store.put(tenant, &partial, b"partial bytes").unwrap();

    // The crash window: payload durable, sidecar never renamed into place.
    fs::remove_file(fixture.meta_path(tenant, orphan.as_str())).unwrap();
    // A partial sidecar is the other unreadable shape.
    fs::write(fixture.meta_path(tenant, partial.as_str()), b"not json").unwrap();

    let listed = store.list(tenant, "ci/").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, kept);
    assert_eq!(store.inventory().unwrap().len(), 1);
    assert_eq!(store.usage(tenant).unwrap().bytes, 4);
    let problems = store.verify_all().unwrap();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems
            .iter()
            .all(|problem| problem.kind == ObjectProblemKind::Orphan)
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.message.contains("sidecar is missing"))
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.message.contains("sidecar is unreadable"))
    );

    // A later put adopts both kinds of orphan for the same key.
    store.put(tenant, &orphan, b"adopted").unwrap();
    store.put(tenant, &partial, b"repaired").unwrap();
    assert_eq!(store.get(tenant, &orphan, None).unwrap(), b"adopted");
    assert_eq!(store.get(tenant, &partial, None).unwrap(), b"repaired");
    assert_eq!(store.list(tenant, "ci/").unwrap().len(), 3);
    assert_eq!(store.inventory().unwrap().len(), 3);
    assert!(store.verify_all().unwrap().is_empty());
}

#[test]
fn a_torn_tail_is_readable_up_to_the_last_committed_record() {
    let fixture = Fixture::new("torn-tail");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/logs/run-10.log");
    store.create_appendable(tenant, &key).unwrap();
    store.append(tenant, &key, b"line one\n").unwrap();

    let path = store.data_path(tenant, &key);
    let committed = fs::metadata(&path).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"torn")
        .unwrap();

    // The committed prefix is complete records; the tail was never
    // acknowledged, so a read returns the prefix rather than refusing.
    assert_eq!(store.get(tenant, &key, None).unwrap(), b"line one\n");
    assert_eq!(
        store
            .get(tenant, &key, Some(ByteRange::new(0, 3).unwrap()))
            .unwrap(),
        b"line"
    );
    store.verify(tenant, &key).unwrap();
    assert!(
        store.verify_all().unwrap().is_empty(),
        "a torn tail is not corruption"
    );
    assert!(matches!(
        store.get(
            tenant,
            &key,
            Some(ByteRange::new(0, committed).unwrap())
        ),
        Err(ObjectStoreError::RangeNotSatisfiable { len, .. }) if len == committed
    ));

    // The next append truncates the uncommitted tail.
    store.append(tenant, &key, b"line two\n").unwrap();
    assert_eq!(
        store.get(tenant, &key, None).unwrap(),
        b"line one\nline two\n"
    );
}

#[test]
fn a_log_cannot_be_replaced_by_an_immutable_put() {
    let fixture = Fixture::new("put-over-log");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/logs/run-11.log");
    store.create_appendable(tenant, &key).unwrap();
    store.append(tenant, &key, b"line one\n").unwrap();
    assert!(matches!(
        store.put(tenant, &key, b"replacement"),
        Err(ObjectStoreError::Conflict(_))
    ));
    assert_eq!(store.get(tenant, &key, None).unwrap(), b"line one\n");
}

#[test]
fn a_key_that_collides_with_another_as_a_path_prefix_is_refused() {
    let fixture = Fixture::new("key-prefix");
    let store = fixture.store();
    let tenant = Fixture::tenant();
    store.put(tenant, &Fixture::key("a"), b"a").unwrap();
    assert!(matches!(
        store.put(tenant, &Fixture::key("a/b"), b"b"),
        Err(ObjectStoreError::Conflict(_))
    ));
    store.put(tenant, &Fixture::key("x/y"), b"y").unwrap();
    assert!(matches!(
        store.put(tenant, &Fixture::key("x"), b"x"),
        Err(ObjectStoreError::Conflict(_))
    ));
    assert_eq!(store.get(tenant, &Fixture::key("a"), None).unwrap(), b"a");
    assert_eq!(store.get(tenant, &Fixture::key("x/y"), None).unwrap(), b"y");
}

#[test]
fn an_inverted_range_is_refused_without_inventing_a_length() {
    assert!(matches!(
        ByteRange::new(9, 3),
        Err(ObjectStoreError::InvalidRange { start: 9, end: 3 })
    ));
}

#[test]
fn a_stale_usage_counter_is_reconciled_from_the_objects() {
    let fixture = Fixture::new("usage-reconcile");
    let store = fixture.store_with(ObjectStoreOptions {
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    store
        .put(tenant, &Fixture::key("ci/one"), &[0u8; 60])
        .unwrap();

    // Simulate a crash between the payload and the counter update.
    fs::remove_file(
        fixture
            .0
            .join("objects/usage")
            .join(format!("{}.json", tenant.as_uuid().simple())),
    )
    .unwrap();

    // The next write rebuilds the counter from the objects, so the quota is
    // still refused rather than reset to zero.
    let error = store
        .put(tenant, &Fixture::key("ci/two"), &[0u8; 50])
        .unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::QuotaExceeded { used: 60, .. }),
        "{error}"
    );
    store.reconcile_usage().unwrap();
    store
        .put(tenant, &Fixture::key("ci/three"), &[0u8; 40])
        .unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);
}

#[test]
fn completing_an_upload_moves_part_bytes_without_charging_twice() {
    let fixture = Fixture::new("complete-quota");
    let store = fixture.store_with(ObjectStoreOptions {
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/artifacts/pack");
    let upload = store.create_multipart(tenant, &key).unwrap();
    store
        .upload_part(tenant, &key, &upload, 1, &[1u8; 60])
        .unwrap();
    store
        .upload_part(tenant, &key, &upload, 2, &[2u8; 40])
        .unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);

    let meta = store
        .complete_multipart(tenant, &key, &upload, None)
        .unwrap();
    assert_eq!(meta.bytes, 100);
    // Part bytes became object bytes; the total did not double.
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);
    // An overwrite is credited against the existing object.
    store.put(tenant, &key, &[3u8; 60]).unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 60);
}

#[test]
fn aborting_an_upload_releases_its_quota() {
    let fixture = Fixture::new("abort-quota");
    let store = fixture.store_with(ObjectStoreOptions {
        quota_bytes: 100,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    store
        .put(tenant, &Fixture::key("ci/one"), &[0u8; 60])
        .unwrap();
    let key = Fixture::key("ci/two");
    let upload = store.create_multipart(tenant, &key).unwrap();
    store
        .upload_part(tenant, &key, &upload, 1, &[0u8; 40])
        .unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);
    store.abort_multipart(tenant, &key, &upload).unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 60);
    // The released 40 bytes can be used by a committed object.
    store.put(tenant, &key, &[0u8; 40]).unwrap();
    assert_eq!(store.usage(tenant).unwrap().bytes, 100);
}

#[test]
fn multipart_completion_streams_and_respects_part_and_object_bounds() {
    const MIB: usize = 1 << 20;
    let fixture = Fixture::new("bounds");
    let store = fixture.store_with(ObjectStoreOptions {
        max_object_bytes: 3 * MIB as u64,
        max_part_bytes: 2 * MIB as u64,
        ..ObjectStoreOptions::default()
    });
    let tenant = Fixture::tenant();
    let key = Fixture::key("ci/artifacts/large.bin");
    let upload = store.create_multipart(tenant, &key).unwrap();

    let first = vec![9u8; 2 * MIB];
    store.upload_part(tenant, &key, &upload, 1, &first).unwrap();
    let error = store
        .upload_part(tenant, &key, &upload, 2, &vec![0u8; 2 * MIB + 1])
        .unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::TooLarge { limit, .. } if limit == 2 * MIB as u64),
        "{error}"
    );
    let second = vec![7u8; MIB];
    store
        .upload_part(tenant, &key, &upload, 2, &second)
        .unwrap();

    // Completion copies 3 MiB through the streaming path and verifies it.
    let meta = store
        .complete_multipart(tenant, &key, &upload, None)
        .unwrap();
    assert_eq!(meta.bytes, 3 * MIB as u64);
    let data = store.get(tenant, &key, None).unwrap();
    assert_eq!(data.len(), 3 * MIB);
    assert_eq!(&data[..2 * MIB], &first[..]);
    assert_eq!(&data[2 * MIB..], &second[..]);

    // The object bound is enforced for a single-shot put too.
    let error = store
        .put(
            tenant,
            &Fixture::key("ci/artifacts/too-large"),
            &vec![0u8; 3 * MIB + 1],
        )
        .unwrap_err();
    assert!(
        matches!(error, ObjectStoreError::TooLarge { limit, .. } if limit == 3 * MIB as u64),
        "{error}"
    );
}
