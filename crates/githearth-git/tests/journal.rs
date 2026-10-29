use githearth_core::ObjectId;
use githearth_git::journal::{
    Journal, JournalError, Kind, MAX_RECORD_BYTES, Origin, Outcome, Record, Update, classify,
    crc32c,
};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "githearth-journal-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn repo(&self) -> PathBuf {
        let repo = self.0.join("repo");
        fs::create_dir_all(repo.join("githearth")).unwrap();
        repo
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn update(old: &str, new: &str, reference: &str) -> Update {
    Update {
        old: old.to_owned(),
        new: new.to_owned(),
        reference: reference.to_owned(),
    }
}

fn oid(s: &str) -> ObjectId {
    s.parse().unwrap()
}

fn journal_bytes(repo: &Path) -> Vec<u8> {
    fs::read(repo.join("githearth").join("journal")).unwrap()
}

/// Byte offsets where each framed record begins.
fn record_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0;
    while offset + 8 <= bytes.len() {
        offsets.push(offset);
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 8 + len;
    }
    offsets
}

#[test]
fn crc32c_matches_standard_check_value() {
    assert_eq!(crc32c(b"123456789"), 0xE3069283);
}

#[test]
fn error_display_and_io_conversion() {
    assert!(
        JournalError::Corrupt { offset: 7 }
            .to_string()
            .contains("offset 7")
    );
    assert!(JournalError::TooLarge { size: 9 }.to_string().contains("9"));
    let e = JournalError::from(std::io::Error::other("boom"));
    assert!(matches!(e, JournalError::Io(_)));
    assert!(e.to_string().contains("boom"));
}

#[test]
fn round_trips_across_reopen() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let op = Uuid::new_v4();
    let actor = Uuid::new_v4();
    let updates = vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")];

    let (seq_prepared, seq_marker);
    let mut journal = Journal::open(&repo).unwrap();
    seq_prepared = journal
        .append_prepared(op, Some(actor), updates.clone(), None)
        .unwrap();
    seq_marker = journal.append_marker(op, Kind::Committed).unwrap();
    drop(journal);

    let journal = Journal::open(&repo).unwrap();
    assert_eq!(seq_prepared, 1);
    assert_eq!(seq_marker, 2);
    let records: Vec<&Record> = journal.records_after(0).collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].kind, Kind::Prepared);
    assert_eq!(records[0].operation, op);
    assert_eq!(records[0].actor, Some(actor));
    assert_eq!(records[0].updates, updates);
    assert_eq!(records[1].kind, Kind::Committed);
    assert!(records[1].updates.is_empty());
    assert_eq!(records[1].actor, None);
}

#[test]
fn origin_round_trips_with_and_without() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let updates = vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")];

    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(Uuid::new_v4(), None, updates.clone(), Some(Origin::Merge))
        .unwrap();
    journal
        .append_prepared(Uuid::new_v4(), None, updates, None)
        .unwrap();
    drop(journal);

    let journal = Journal::open(&repo).unwrap();
    let records: Vec<&Record> = journal.records_after(0).collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].origin, Some(Origin::Merge));
    assert_eq!(records[1].origin, None);
}

#[test]
fn old_format_without_origin_parses_as_none() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let payload = serde_json::json!({
        "sequence": 1u64,
        "operation": Uuid::new_v4(),
        "kind": "prepared",
        "actor": null,
        "updates": [{
            "old": "0".repeat(40),
            "new": "a".repeat(40),
            "ref": "refs/heads/main",
        }],
    });
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut bytes = Vec::with_capacity(8 + payload.len());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&crc32c(&payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    fs::write(repo.join("githearth").join("journal"), bytes).unwrap();

    let journal = Journal::open(&repo).unwrap();
    let records: Vec<&Record> = journal.records_after(0).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, Kind::Prepared);
    assert_eq!(records[0].origin, None);
}

#[test]
fn truncates_torn_length_and_crc_fields() {
    // Torn length field: only part of the 4-byte length remains.
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            None,
            vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    drop(journal);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(repo.join("githearth").join("journal"))
        .unwrap();
    file.set_len(2).unwrap();
    drop(file);
    assert_eq!(Journal::open(&repo).unwrap().records_after(0).count(), 0);

    // Torn CRC field: the length is complete but the CRC is short.
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(
            Uuid::new_v4(),
            None,
            vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    drop(journal);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(repo.join("githearth").join("journal"))
        .unwrap();
    file.set_len(6).unwrap();
    drop(file);
    assert_eq!(Journal::open(&repo).unwrap().records_after(0).count(), 0);
}

#[test]
fn truncates_torn_final_record() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let op = Uuid::new_v4();
    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(
            op,
            None,
            vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    journal.append_marker(op, Kind::Committed).unwrap();
    drop(journal);

    let full = journal_bytes(&repo);
    let cut = full.len() - 3;
    let out = fs::OpenOptions::new()
        .write(true)
        .open(repo.join("githearth").join("journal"))
        .unwrap();
    out.set_len(cut as u64).unwrap();
    drop(out);

    let journal = Journal::open(&repo).unwrap();
    assert_eq!(journal.records_after(0).count(), 1);
}

#[test]
fn truncates_bad_crc_in_final_record() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let op = Uuid::new_v4();
    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(
            op,
            None,
            vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    journal.append_marker(op, Kind::Committed).unwrap();
    drop(journal);

    let mut bytes = journal_bytes(&repo);
    let offsets = record_offsets(&bytes);
    let last = *offsets.last().unwrap();
    bytes[last + 4] ^= 0xFF;
    fs::write(repo.join("githearth").join("journal"), &bytes).unwrap();

    let mut journal = Journal::open(&repo).unwrap();
    assert_eq!(journal.records_after(0).count(), 1);
    assert!(journal.append_marker(op, Kind::Aborted).is_ok());
}

#[test]
fn refuses_corruption_in_middle_record_and_appends() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let op = Uuid::new_v4();
    let mut journal = Journal::open(&repo).unwrap();
    journal
        .append_prepared(
            op,
            None,
            vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    journal.append_marker(op, Kind::Committed).unwrap();
    journal
        .append_prepared(
            op,
            None,
            vec![update(&"0".repeat(40), &"b".repeat(40), "refs/heads/other")],
            None,
        )
        .unwrap();
    drop(journal);

    let mut bytes = journal_bytes(&repo);
    let offsets = record_offsets(&bytes);
    let middle = offsets[1];
    bytes[middle + 8 + 4] ^= 0xFF;
    fs::write(repo.join("githearth").join("journal"), &bytes).unwrap();

    assert!(matches!(
        Journal::open(&repo),
        Err(JournalError::Corrupt { .. })
    ));
    // The corrupt journal refuses all use: appends are impossible without a Journal.
    assert!(matches!(
        Journal::open(&repo),
        Err(JournalError::Corrupt { .. })
    ));
}

#[test]
fn rejects_oversize_length_field() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut file = fs::File::create(repo.join("githearth").join("journal")).unwrap();
    let big = (MAX_RECORD_BYTES as u32) + 1;
    file.write_all(&big.to_le_bytes()).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    drop(file);
    assert!(matches!(
        Journal::open(&repo),
        Err(JournalError::Corrupt { .. })
    ));
}

#[test]
fn refuses_oversize_append() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut journal = Journal::open(&repo).unwrap();
    let updates = vec![update(
        &"0".repeat(40),
        &"a".repeat(MAX_RECORD_BYTES),
        "refs/heads/main",
    )];
    assert!(matches!(
        journal.append_prepared(Uuid::new_v4(), None, updates, None),
        Err(JournalError::TooLarge { .. })
    ));
}

#[test]
fn rotation_keeps_unapplied_and_sequences() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let op = Uuid::new_v4();
    let mut journal = Journal::open(&repo).unwrap();
    for i in 0..4u64 {
        journal
            .append_prepared(
                op,
                None,
                vec![update(
                    &"0".repeat(40),
                    &format!("{i:040x}"),
                    "refs/heads/main",
                )],
                None,
            )
            .unwrap();
    }
    // Under the limit, nothing happens.
    journal.rotate(u64::MAX, 3).unwrap();
    assert!(!repo.join("githearth").join("journal.0").exists());

    let seqs: Vec<u64> = journal.records_after(0).map(|r| r.sequence).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);

    journal.rotate(0, 1).unwrap();
    let remaining: Vec<u64> = journal.records_after(0).map(|r| r.sequence).collect();
    assert_eq!(remaining, vec![2, 3, 4]);
    assert!(repo.join("githearth").join("journal.1").exists());

    // applied_through below the first remaining sequence is a no-op.
    journal.rotate(0, 1).unwrap();
    assert_eq!(journal.records_after(0).count(), 3);

    // Sequences continue strictly increasing across the rewrite.
    let next = journal
        .append_prepared(
            op,
            None,
            vec![update(&"0".repeat(40), &"c".repeat(40), "refs/heads/x")],
            None,
        )
        .unwrap();
    assert_eq!(next, 5);
}

#[test]
fn rotation_of_a_fully_applied_journal_keeps_sequences_after_reopen() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut journal = Journal::open(&repo).unwrap();
    for i in 0..3u64 {
        journal
            .append_prepared(
                Uuid::new_v4(),
                None,
                vec![update(
                    &"0".repeat(40),
                    &format!("{i:040x}"),
                    "refs/heads/main",
                )],
                None,
            )
            .unwrap();
    }
    journal.rotate(0, 3).unwrap();
    let live: Vec<u64> = journal.records_after(0).map(|r| r.sequence).collect();
    assert_eq!(live, vec![3]);
    drop(journal);

    let mut reopened = Journal::open(&repo).unwrap();
    let next = reopened
        .append_prepared(
            Uuid::new_v4(),
            None,
            vec![update(&"0".repeat(40), &"d".repeat(40), "refs/heads/main")],
            None,
        )
        .unwrap();
    assert_eq!(next, 4);
}

#[test]
fn rotation_rewrites_an_archive_left_by_a_crashed_rotation() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let mut journal = Journal::open(&repo).unwrap();
    for i in 0..3u64 {
        journal
            .append_prepared(
                Uuid::new_v4(),
                None,
                vec![update(
                    &"0".repeat(40),
                    &format!("{i:040x}"),
                    "refs/heads/main",
                )],
                None,
            )
            .unwrap();
    }
    // A crash after writing the archive but before replacing the journal leaves it behind.
    let archive = repo.join("githearth").join("journal.1");
    std::fs::write(&archive, b"left over from a crashed rotation").unwrap();
    journal.rotate(0, 2).unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    assert!(!bytes.starts_with(b"left over"));
    // Exactly the two rotated records: 4-byte length + 4-byte CRC + payload each.
    let mut offset = 0;
    let mut records = 0;
    while offset < bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 8 + len;
        records += 1;
    }
    assert_eq!((offset, records), (bytes.len(), 2));
}

#[test]
fn classify_covers_every_outcome() {
    let prepared = |updates: Vec<Update>| Record {
        sequence: 0,
        operation: Uuid::nil(),
        kind: Kind::Prepared,
        actor: None,
        origin: None,
        updates,
    };
    let u = vec![update(&"0".repeat(40), &"a".repeat(40), "refs/heads/main")];
    let record = prepared(u);

    let mut at_new = HashMap::new();
    at_new.insert("refs/heads/main".to_owned(), oid(&"a".repeat(40)));
    // old is the zero oid (ref absent), so "all at old" means the ref is still absent.
    let at_old: HashMap<String, ObjectId> = HashMap::new();

    assert_eq!(
        classify(&record, Some(Kind::Committed), &at_new),
        Outcome::Committed
    );
    assert_eq!(
        classify(&record, Some(Kind::Aborted), &at_new),
        Outcome::Aborted
    );
    assert_eq!(
        classify(&record, Some(Kind::Prepared), &at_new),
        Outcome::Diverged
    );

    assert_eq!(
        classify(&record, None, &at_new),
        Outcome::CommittedMarkerLost
    );
    assert_eq!(classify(&record, None, &at_old), Outcome::AbortedMarkerLost);

    let mut mixed = HashMap::new();
    mixed.insert("refs/heads/main".to_owned(), oid(&"b".repeat(40)));
    assert_eq!(classify(&record, None, &mixed), Outcome::Diverged);

    // Zero oid means the ref is absent.
    let deletion = prepared(vec![update(
        &"a".repeat(40),
        &"0".repeat(40),
        "refs/heads/deleted",
    )]);
    let empty: HashMap<String, ObjectId> = HashMap::new();
    assert_eq!(
        classify(&deletion, None, &empty),
        Outcome::CommittedMarkerLost
    );

    let creation = prepared(vec![update(
        &"0".repeat(40),
        &"a".repeat(40),
        "refs/heads/new",
    )]);
    assert_eq!(
        classify(&creation, None, &empty),
        Outcome::AbortedMarkerLost
    );

    let mut now = HashMap::new();
    now.insert("refs/heads/new".to_owned(), oid(&"a".repeat(40)));
    assert_eq!(
        classify(&creation, None, &now),
        Outcome::CommittedMarkerLost
    );
}
