//! Append-only push journal (ADR 0003 "Journal format" and "Recovery").

use githearth_core::ObjectId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;

/// Largest accepted record payload.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

const CRC_POLY: u32 = 0x82F63B78;

static CRC_TABLE: LazyLock<[u32; 256]> = LazyLock::new(build_table);

fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC_POLY
            } else {
                crc >> 1
            };
        }
        *entry = crc;
    }
    table
}

/// CRC-32C (Castagnoli), reflected.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let table = &*CRC_TABLE;
    let mut crc = !0u32;
    for &b in bytes {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Prepared,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub old: String,
    pub new: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub sequence: u64,
    pub operation: Uuid,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<Update>,
}

#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    Corrupt { offset: u64 },
    TooLarge { size: usize },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal i/o error: {e}"),
            Self::Corrupt { offset } => write!(f, "journal corrupt at byte offset {offset}"),
            Self::TooLarge { size } => write!(
                f,
                "journal record is {size} bytes; limit is {MAX_RECORD_BYTES}"
            ),
        }
    }
}
impl std::error::Error for JournalError {}
impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct Journal {
    path: PathBuf,
    file: fs::File,
    next_sequence: u64,
    records: Vec<Record>,
}

fn read_chunk(file: &mut fs::File, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

fn frame(record: &Record) -> Vec<u8> {
    let payload = serde_json::to_vec(record).expect("journal record serializes");
    let mut bytes = Vec::with_capacity(8 + payload.len());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&crc32c(&payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

fn write_record<W: Write>(out: &mut W, record: &Record) -> io::Result<()> {
    out.write_all(&frame(record))
}

impl Journal {
    pub fn open(repo_path: &Path) -> Result<Self, JournalError> {
        let path = repo_path.join("githearth").join("journal");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let mut records = Vec::new();
        let mut next_sequence = 1;
        let mut truncated: Option<u64> = None;
        loop {
            let offset = file.stream_position()?;
            let len_bytes = read_chunk(&mut file, 4)?;
            if len_bytes.is_empty() {
                break;
            }
            if len_bytes.len() < 4 {
                truncated = Some(offset);
                break;
            }
            let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            if len > MAX_RECORD_BYTES {
                return Err(JournalError::Corrupt { offset });
            }
            let crc_bytes = read_chunk(&mut file, 4)?;
            if crc_bytes.len() < 4 {
                truncated = Some(offset);
                break;
            }
            let crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
            let payload = read_chunk(&mut file, len)?;
            if payload.len() < len {
                truncated = Some(offset);
                break;
            }
            if crc32c(&payload) != crc {
                if read_chunk(&mut file, 1)?.is_empty() {
                    truncated = Some(offset);
                    break;
                }
                return Err(JournalError::Corrupt { offset });
            }
            let record: Record =
                serde_json::from_slice(&payload).map_err(|_| JournalError::Corrupt { offset })?;
            next_sequence = next_sequence.max(record.sequence + 1);
            records.push(record);
        }
        if let Some(offset) = truncated {
            file.set_len(offset)?;
            file.sync_data()?;
        }
        Ok(Self {
            path,
            file,
            next_sequence,
            records,
        })
    }

    fn append(&mut self, mut record: Record) -> Result<u64, JournalError> {
        record.sequence = self.next_sequence;
        let payload = serde_json::to_vec(&record).expect("journal record serializes");
        if payload.len() > MAX_RECORD_BYTES {
            return Err(JournalError::TooLarge {
                size: payload.len(),
            });
        }
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&crc32c(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        self.next_sequence += 1;
        let sequence = record.sequence;
        self.records.push(record);
        Ok(sequence)
    }

    pub fn append_prepared(
        &mut self,
        operation: Uuid,
        actor: Option<Uuid>,
        updates: Vec<Update>,
    ) -> Result<u64, JournalError> {
        self.append(Record {
            sequence: 0,
            operation,
            kind: Kind::Prepared,
            actor,
            updates,
        })
    }

    pub fn append_marker(&mut self, operation: Uuid, kind: Kind) -> Result<u64, JournalError> {
        debug_assert!(matches!(kind, Kind::Committed | Kind::Aborted));
        self.append(Record {
            sequence: 0,
            operation,
            kind,
            actor: None,
            updates: Vec::new(),
        })
    }

    pub fn records_after(&self, sequence: u64) -> impl Iterator<Item = &Record> {
        self.records.iter().filter(move |r| r.sequence > sequence)
    }

    pub fn rotate(&mut self, max_bytes: u64, applied_through: u64) -> Result<(), JournalError> {
        if self.file.metadata()?.len() <= max_bytes {
            return Ok(());
        }
        let split = self
            .records
            .iter()
            .take_while(|r| r.sequence <= applied_through)
            .count()
            // Keep the newest record live: `open` derives the next sequence from it, and an
            // empty journal would restart at 1 and collide with already-recorded pushes.
            .min(self.records.len().saturating_sub(1));
        if split == 0 {
            return Ok(());
        }
        let first = self.records[0].sequence;
        let dir = self
            .path
            .parent()
            .expect("journal path has a parent")
            .to_path_buf();
        let moved_path = dir.join(format!("journal.{first}"));
        {
            let mut moved = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&moved_path)?;
            for record in &self.records[..split] {
                write_record(&mut moved, record)?;
            }
            moved.sync_data()?;
        }
        let tmp = dir.join("journal.tmp");
        {
            let mut out = fs::File::create(&tmp)?;
            for record in &self.records[split..] {
                write_record(&mut out, record)?;
            }
            out.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        sync_dir(&dir)?;
        self.records.drain(..split);
        self.file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Aborted,
    CommittedMarkerLost,
    AbortedMarkerLost,
    Diverged,
}

fn is_zero(oid: &str) -> bool {
    oid.bytes().all(|b| b == b'0')
}

fn all_at(prepared: &Record, refs_now: &HashMap<String, ObjectId>, new_side: bool) -> bool {
    prepared.updates.iter().all(|update| {
        let wanted = if new_side { &update.new } else { &update.old };
        match refs_now.get(&update.reference) {
            None => is_zero(wanted),
            Some(current) => !is_zero(wanted) && current.to_string() == *wanted,
        }
    })
}

/// Classify a prepared record against the current refs, exactly per ADR 0003.
pub fn classify(
    prepared: &Record,
    marker: Option<Kind>,
    refs_now: &HashMap<String, ObjectId>,
) -> Outcome {
    match marker {
        Some(Kind::Committed) => Outcome::Committed,
        Some(Kind::Aborted) => Outcome::Aborted,
        Some(Kind::Prepared) => Outcome::Diverged,
        None if all_at(prepared, refs_now, true) => Outcome::CommittedMarkerLost,
        None if all_at(prepared, refs_now, false) => Outcome::AbortedMarkerLost,
        None => Outcome::Diverged,
    }
}
