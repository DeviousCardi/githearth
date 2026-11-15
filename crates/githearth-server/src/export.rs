//! The versioned export contract and its import (M4.2 U30a).
//!
//! An export is a directory with three parts, named by the plan:
//!
//! ```text
//! export/
//!   manifest.json              the format version, part checksums and counts
//!   metadata.jsonl             one JSON record per line (product metadata)
//!   objects.jsonl              the object manifest: tenant, key, sha256, size
//!   objects/data/<tenant>/...  the large objects named by that manifest
//!   git/<repository>.git/      native bare Git repositories (history)
//! ```
//!
//! The metadata schema is deliberately a subset of the installation: it
//! carries what a later milestone must round-trip and omits every secret and
//! every piece of runtime bookkeeping. `manifest.excluded` states the omissions
//! in the export itself, so a reader does not have to infer them.
//!
//! Import is **refusable, not idempotent**. It needs a database that does not
//! exist and a storage root that is missing or empty, exactly like `restore`.
//! An export is a whole-installation snapshot, not a delta; re-running it over
//! its own result would need conflict resolution and would hide a partial
//! import behind a success. Every check that can be made before writing is made
//! first: the format version, every part checksum, the metadata references and
//! every object payload's SHA-256. Only then is the database created and the
//! storage staged. A failure drops the database it created and removes the
//! staging directory; a database a concurrent import created is left alone.
//!
//! The contract is versioned with [`EXPORT_FORMAT_VERSION`]. A reader refuses
//! any other version instead of guessing: a version 2 export read by version 1
//! could silently skip a new part.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use githearth_git::layout::{ObjectFormat, Storage, StorageOptions};
use githearth_git::object_store::{
    FsObjectStore, ObjectKey, ObjectMeta, ObjectRecord, ObjectStore, TenantId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgConnectOptions;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const EXPORT_FORMAT: &str = "githearth-export";
/// The only version this binary writes and reads. A bump is required whenever a
/// reader of the previous version would accept the new bytes and then misread
/// or skip a part (ADR 0014 open item 7).
pub const EXPORT_FORMAT_VERSION: u32 = 1;
const METADATA_FILE: &str = "metadata.jsonl";
const OBJECTS_FILE: &str = "objects.jsonl";
const MANIFEST_FILE: &str = "manifest.json";
const CHUNK_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Manifest and part headers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportManifest {
    pub format: String,
    pub format_version: u32,
    pub created_unix_seconds: u64,
    /// Records per metadata kind. The harness compares these across the two
    /// exports; a reader can size a destination before parsing.
    pub counts: BTreeMap<String, u64>,
    pub metadata: Part,
    pub objects: ObjectPart,
    pub git: Vec<GitPart>,
    /// What is deliberately not in the contract. This is data, not prose in a
    /// document: an exporter must record its own omissions.
    pub excluded: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub lines: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectPart {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub records: u64,
    pub payload_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitPart {
    pub repository: Uuid,
    /// Always `git/<repository>.git`; checked on import so a manifest cannot
    /// point outside the export directory.
    pub path: String,
    pub refs: u64,
    /// SHA-256 over sorted `object-id refname` lines. Ref names and targets are
    /// the part of a repository a client can see; the object payloads travel in
    /// the native repository itself.
    pub refs_sha256: String,
    /// The symbolic `HEAD`, when the repository has one.
    pub head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MetadataHeader {
    kind: String,
    format: String,
    format_version: u32,
    created_unix_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ObjectsHeader {
    kind: String,
    format: String,
    format_version: u32,
    created_unix_seconds: u64,
}

/// One object in the export. This is the object store's inventory record plus
/// the two things a reader needs: whether the object is appendable, and where
/// the bytes live inside the export.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportedObject {
    pub tenant: Uuid,
    pub key: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub appendable: bool,
    pub path: String,
}

// ---------------------------------------------------------------------------
// Metadata records
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountRecord {
    pub id: Uuid,
    pub username: String,
    pub email: Option<String>,
    pub email_verified_at: Option<String>,
    pub is_admin: bool,
    pub suspended: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrganizationRecord {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub suspended: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipRecord {
    pub organization_id: Uuid,
    pub account_id: Uuid,
    pub role: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamRecord {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub slug: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TeamMemberRecord {
    pub team_id: Uuid,
    pub account_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRecord {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub description: String,
    pub visibility: String,
    pub archived: bool,
    /// Always `active` in this version: a deleted or creating repository is not
    /// part of the contract.
    pub status: String,
    pub object_format: String,
    pub created_by: Uuid,
    pub parent_repository_id: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantRecord {
    pub id: Uuid,
    pub role: String,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub scope_kind: String,
    pub scope_id: Uuid,
    pub repository_id: Option<Uuid>,
    pub expires_at: Option<String>,
    pub granted_by: Uuid,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueRecord {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub author_id: Uuid,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PullRequestRecord {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub author_id: Uuid,
    pub source_branch: String,
    pub target_branch: String,
    pub source_repository_id: Uuid,
    pub state: String,
    pub draft: bool,
    pub closed_reason: Option<String>,
    pub merged_at: Option<String>,
    pub merged_by: Option<Uuid>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewRecord {
    pub id: Uuid,
    pub pull_request_id: Uuid,
    pub reviewer_id: Uuid,
    pub verdict: String,
    pub commit_id: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataRecord {
    Account(AccountRecord),
    Organization(OrganizationRecord),
    Membership(MembershipRecord),
    Team(TeamRecord),
    TeamMember(TeamMemberRecord),
    Repository(RepositoryRecord),
    Grant(GrantRecord),
    Issue(IssueRecord),
    PullRequest(PullRequestRecord),
    PullRequestReview(ReviewRecord),
}

impl MetadataRecord {
    fn kind(&self) -> &'static str {
        match self {
            Self::Account(_) => "account",
            Self::Organization(_) => "organization",
            Self::Membership(_) => "membership",
            Self::Team(_) => "team",
            Self::TeamMember(_) => "team_member",
            Self::Repository(_) => "repository",
            Self::Grant(_) => "grant",
            Self::Issue(_) => "issue",
            Self::PullRequest(_) => "pull_request",
            Self::PullRequestReview(_) => "pull_request_review",
        }
    }

    fn sort_key(&self) -> (u8, String) {
        let rank = match self {
            Self::Account(_) => 0,
            Self::Organization(_) => 1,
            Self::Membership(_) => 2,
            Self::Team(_) => 3,
            Self::TeamMember(_) => 4,
            Self::Repository(_) => 5,
            Self::Grant(_) => 6,
            Self::Issue(_) => 7,
            Self::PullRequest(_) => 8,
            Self::PullRequestReview(_) => 9,
        };
        let key = match self {
            Self::Account(r) => r.id.to_string(),
            Self::Organization(r) => r.id.to_string(),
            Self::Membership(r) => format!("{}/{}", r.organization_id, r.account_id),
            Self::Team(r) => r.id.to_string(),
            Self::TeamMember(r) => format!("{}/{}", r.team_id, r.account_id),
            Self::Repository(r) => r.id.to_string(),
            Self::Grant(r) => r.id.to_string(),
            Self::Issue(r) => format!("{}/{:010}", r.repository_id, r.number),
            Self::PullRequest(r) => format!("{}/{:010}", r.repository_id, r.number),
            Self::PullRequestReview(r) => r.id.to_string(),
        };
        (rank, key)
    }
}

/// The omissions, recorded in every export. The first entry is secret
/// material; the rest is runtime state that is rebuilt, derived, or outside
/// this contract.
fn excluded_parts() -> Vec<String> {
    vec![
        "secrets: password hashes, session and access-token hashes, SSH private keys, \
         push credentials, password-reset and email-verification tokens, the audit IP \
         HMAC key, SMTP credentials and the database URL/configuration"
            .into(),
        "audit retention rules: M4.2 persists none; IP pruning and IP-key dropping are \
         operator command arguments (`githearth audit prune-ips --older-than`, \
         `audit drop-ip-keys --older-than`), so there is no rule row to export"
            .into(),
        "audit events and the audit hash chain: not part of this contract (the backup \
         still carries the database)"
            .into(),
        "user-facing state this version does not carry: pending invitations and their \
         token hashes, pull-request review requests, repository rename/transfer \
         redirects, auto-merge configuration, notification preferences, and the \
         authorization event and cache tables"
            .into(),
        "runtime and derived state: jobs, notifications, outbox deliveries, sessions, \
         issue and pull-request comments, merge intents and checks, commit statuses, \
         repository push records, the per-repository journal position, and the \
         object-store usage cache"
            .into(),
        "policy tables: branch protection, tag protection and code owners are not in \
         this version; M12 adds the full product export"
            .into(),
        "deleted, deleting, failed and creating repositories, and metadata that refers \
         only to them, are excluded; active repositories and their reachable Git \
         history are exported"
            .into(),
        "operator configuration and the SSH host key are not in an export; they are \
         written by `setup`/`restore`, and an imported installation needs a new \
         administrator password (`reset-admin`) because password hashes are absent"
            .into(),
    ]
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

pub struct ExportReport {
    pub archive: PathBuf,
    pub metadata_records: u64,
    pub repositories: usize,
    pub objects: usize,
    pub payload_bytes: u64,
    pub format_version: u32,
}

pub struct ImportReport {
    pub archive: PathBuf,
    pub database: String,
    pub storage_root: PathBuf,
    pub metadata_records: u64,
    pub repositories: usize,
    pub objects: usize,
    pub format_version: u32,
}

pub struct ExportOptions {
    pub archive: PathBuf,
    pub database_url: String,
    pub storage_root: PathBuf,
}

pub struct ImportOptions {
    pub archive: PathBuf,
    pub database_url: String,
    pub storage_root: PathBuf,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub async fn run_export(options: &ExportOptions) -> Result<ExportReport> {
    if options.storage_root.as_os_str().is_empty() {
        bail!("storage root is empty; pass --storage-root or configure one");
    }
    if options.database_url.trim().is_empty() {
        bail!("no database URL; set GITHEARTH_DATABASE_URL or the configuration file");
    }

    let target = &options.archive;
    if target.exists() {
        let empty = fs::read_dir(target)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            bail!(
                "export destination {} is not empty; export needs a new directory",
                target.display()
            );
        }
        fs::remove_dir(target)
            .with_context(|| format!("cannot replace the empty directory {}", target.display()))?;
    }
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.export-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("export"),
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&staging)?;
    set_private_dir(&staging)?;

    let result = export_into(&staging, options).await;
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::rename(&staging, target)
        .with_context(|| format!("cannot publish the export at {}", target.display()))?;
    export_into_report(target).inspect_err(|_| {
        let _ = fs::remove_dir_all(target);
    })
}

fn export_into_report(target: &Path) -> Result<ExportReport> {
    let manifest = read_manifest(target)?;
    Ok(ExportReport {
        archive: target.to_path_buf(),
        metadata_records: manifest.counts.values().sum(),
        repositories: manifest.git.len(),
        objects: manifest.objects.records as usize,
        payload_bytes: manifest.objects.payload_bytes,
        format_version: manifest.format_version,
    })
}

async fn export_into(staging: &Path, options: &ExportOptions) -> Result<()> {
    let config = Config {
        database_url: options.database_url.clone(),
        storage_root: options.storage_root.clone(),
        ..Config::default()
    };
    let pool = crate::connect(&config).await?;
    // One snapshot for every metadata row. The export is not an online backup
    // and proves no write fence (M12 and backup own those); a snapshot plus the
    // store's before/after inventory is the consistency this contract claims.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let mut records = load_metadata(&mut tx).await?;
    tx.rollback().await?;
    pool.close().await;

    records.sort_by_key(MetadataRecord::sort_key);
    let counts = count_records(&records);

    let created = now_unix_seconds();
    let metadata_path = staging.join(METADATA_FILE);
    {
        let mut file = private_file(&metadata_path)?;
        let header = MetadataHeader {
            kind: EXPORT_FORMAT.into(),
            format: EXPORT_FORMAT.into(),
            format_version: EXPORT_FORMAT_VERSION,
            created_unix_seconds: created,
        };
        write_json_line(&mut file, &header)?;
        for record in &records {
            write_json_line(&mut file, record)?;
        }
        file.sync_all()?;
    }

    let object_root = object_root(&options.storage_root);
    let store = FsObjectStore::open_existing(&object_root).with_context(|| {
        format!(
            "the object store {} is missing; an installation has one after first start",
            object_root.display()
        )
    })?;
    let inventory_before = store.inventory()?;
    let mut objects = Vec::with_capacity(inventory_before.len());
    let payload_root = staging.join("objects").join("data");
    for record in &inventory_before {
        let tenant = TenantId::new(record.tenant);
        let key = ObjectKey::parse(&record.key)
            .map_err(|error| anyhow::anyhow!("the object store inventory is invalid: {error}"))?;
        let meta = store.stat(tenant, &key)?;
        let bytes = store.get(tenant, &key, None)?;
        let sha = sha256_bytes(&bytes);
        if bytes.len() as u64 != record.bytes || sha != record.sha256 {
            bail!(
                "object {} in tenant {} does not match its recorded checksum; run \
                 `doctor`/object verification before exporting",
                record.key,
                record.tenant
            );
        }
        let relative = object_relative_path(record.tenant, &record.key);
        let path = payload_root.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private_bytes(&path, &bytes)?;
        objects.push(ExportedObject {
            tenant: record.tenant,
            key: record.key.clone(),
            bytes: record.bytes,
            sha256: record.sha256.clone(),
            appendable: meta.appendable,
            path: format!("objects/data/{}", relative.display()),
        });
    }
    let inventory_after = store.inventory()?;
    if normalize_inventory(&inventory_before) != normalize_inventory(&inventory_after) {
        bail!(
            "the object store inventory changed during the export; stop writers and retry \
             (an export is not an online snapshot)"
        );
    }
    objects.sort_by(|left, right| {
        (left.tenant, left.key.as_str()).cmp(&(right.tenant, right.key.as_str()))
    });

    let objects_path = staging.join(OBJECTS_FILE);
    {
        let mut file = private_file(&objects_path)?;
        let header = ObjectsHeader {
            kind: EXPORT_FORMAT.into(),
            format: EXPORT_FORMAT.into(),
            format_version: EXPORT_FORMAT_VERSION,
            created_unix_seconds: created,
        };
        write_json_line(&mut file, &header)?;
        for object in &objects {
            write_json_line(&mut file, object)?;
        }
        file.sync_all()?;
    }

    let git_parts = export_git(staging, &options.storage_root, &records).await?;

    let metadata_part = part_for(staging, METADATA_FILE)?;
    let objects_part = object_part_for(staging, OBJECTS_FILE, objects.len(), &objects)?;
    let manifest = ExportManifest {
        format: EXPORT_FORMAT.into(),
        format_version: EXPORT_FORMAT_VERSION,
        created_unix_seconds: created,
        counts,
        metadata: metadata_part,
        objects: objects_part,
        git: git_parts,
        excluded: excluded_parts(),
        notes: vec![
            "metadata.jsonl is a stable-ordered JSON Lines stream; the first line is the \
             format header and every other line is one record with a `kind` field"
                .into(),
            "an account record is the identity mapping: stable account id, username, email \
             and its verification, and administrator/suspension state; no password hash or \
             token/key is present"
                .into(),
            "git/ contains native bare repositories, one per exported repository; a plain \
             `git clone` of one is the history"
                .into(),
            "objects.jsonl is the object manifest; every payload is checked against its \
             SHA-256 on import before any destination is written"
                .into(),
        ],
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    write_private_bytes(&staging.join(MANIFEST_FILE), &manifest_bytes)?;
    Ok(())
}

async fn load_metadata(conn: &mut sqlx::PgConnection) -> Result<Vec<MetadataRecord>> {
    let mut records = Vec::new();

    // Accounts are the identity mapping: stable id, username, email and its
    // verification, plus the account's status. No credential material.
    let accounts = sqlx::query(
        "SELECT id, username, email, \
         CASE WHEN email_verified_at IS NULL THEN NULL \
              ELSE to_char(email_verified_at AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') END AS email_verified_at, \
         is_admin, suspended, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM accounts ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    use sqlx::Row;
    for row in accounts {
        let id: Uuid = row.try_get("id")?;
        records.push(MetadataRecord::Account(AccountRecord {
            id,
            username: row.try_get("username")?,
            email: row.try_get("email")?,
            email_verified_at: row.try_get("email_verified_at")?,
            is_admin: row.try_get("is_admin")?,
            suspended: row.try_get("suspended")?,
            created_at: row.try_get("created_at")?,
        }));
    }

    let organizations = sqlx::query(
        "SELECT id, slug, name, suspended, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM organizations ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in organizations {
        let id: Uuid = row.try_get("id")?;
        records.push(MetadataRecord::Organization(OrganizationRecord {
            id,
            slug: row.try_get("slug")?,
            name: row.try_get("name")?,
            suspended: row.try_get("suspended")?,
            created_at: row.try_get("created_at")?,
        }));
    }

    let memberships = sqlx::query(
        "SELECT organization_id, account_id, role FROM memberships ORDER BY organization_id, account_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in memberships {
        records.push(MetadataRecord::Membership(MembershipRecord {
            organization_id: row.try_get("organization_id")?,
            account_id: row.try_get("account_id")?,
            role: row.try_get("role")?,
        }));
    }

    let teams = sqlx::query(
        "SELECT id, organization_id, parent_id, slug, name, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM teams ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in teams {
        let id: Uuid = row.try_get("id")?;
        records.push(MetadataRecord::Team(TeamRecord {
            id,
            organization_id: row.try_get("organization_id")?,
            parent_id: row.try_get("parent_id")?,
            slug: row.try_get("slug")?,
            name: row.try_get("name")?,
            created_at: row.try_get("created_at")?,
        }));
    }

    let team_members =
        sqlx::query("SELECT team_id, account_id FROM team_members ORDER BY team_id, account_id")
            .fetch_all(&mut *conn)
            .await?;
    for row in team_members {
        records.push(MetadataRecord::TeamMember(TeamMemberRecord {
            team_id: row.try_get("team_id")?,
            account_id: row.try_get("account_id")?,
        }));
    }

    let repositories = sqlx::query(
        "SELECT id, organization_id, name, description, visibility, archived, status, \
         object_format, created_by, parent_repository_id, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at, \
         to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS updated_at \
         FROM repositories \
         WHERE status = 'active' AND deleted_at IS NULL \
         ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    // Collect every exported id first: a fork's parent may sort after its
    // child, and a filter against a set filled inside the loop would silently
    // write that parent as `null`.
    let repo_ids: HashSet<Uuid> = repositories
        .iter()
        .map(|row| row.try_get::<Uuid, _>("id"))
        .collect::<std::result::Result<_, sqlx::Error>>()?;
    for row in repositories {
        let id: Uuid = row.try_get("id")?;
        let organization_id: Uuid = row.try_get("organization_id")?;
        records.push(MetadataRecord::Repository(RepositoryRecord {
            id,
            organization_id,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            visibility: row.try_get("visibility")?,
            archived: row.try_get("archived")?,
            status: "active".into(),
            object_format: row.try_get("object_format")?,
            created_by: row.try_get("created_by")?,
            parent_repository_id: row
                .try_get::<Option<Uuid>, _>("parent_repository_id")?
                .filter(|parent| repo_ids.contains(parent)),
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        }));
    }

    // Active, unexpired grants only. Revocation is permanent and audited; a
    // revoked grant is history, not access, and an expired grant could not be
    // inserted again under the grant trigger.
    let grants = sqlx::query(
        "SELECT id, role, subject_kind, subject_id, scope_kind, scope_id, repository_id, \
         CASE WHEN expires_at IS NULL THEN NULL \
              ELSE to_char(expires_at AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') END AS expires_at, \
         granted_by, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM grants \
         WHERE revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > clock_timestamp()) \
         ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in grants {
        let repository_id: Option<Uuid> = row.try_get("repository_id")?;
        if repository_id.is_some_and(|id| !repo_ids.contains(&id)) {
            continue;
        }
        records.push(MetadataRecord::Grant(GrantRecord {
            id: row.try_get("id")?,
            role: row.try_get("role")?,
            subject_kind: row.try_get("subject_kind")?,
            subject_id: row.try_get("subject_id")?,
            scope_kind: row.try_get("scope_kind")?,
            scope_id: row.try_get("scope_id")?,
            repository_id,
            expires_at: row.try_get("expires_at")?,
            granted_by: row.try_get("granted_by")?,
            created_at: row.try_get("created_at")?,
        }));
    }

    let issues = sqlx::query(
        "SELECT id, repository_id, number, title, body, author_id, state, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at, \
         to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS updated_at, \
         CASE WHEN closed_at IS NULL THEN NULL \
              ELSE to_char(closed_at AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') END AS closed_at \
         FROM issues ORDER BY repository_id, number",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in issues {
        let repository_id: Uuid = row.try_get("repository_id")?;
        if !repo_ids.contains(&repository_id) {
            continue;
        }
        records.push(MetadataRecord::Issue(IssueRecord {
            id: row.try_get("id")?,
            repository_id,
            number: row.try_get("number")?,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            author_id: row.try_get("author_id")?,
            state: row.try_get("state")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            closed_at: row.try_get("closed_at")?,
        }));
    }

    let pulls = sqlx::query(
        "SELECT id, repository_id, number, title, body, author_id, source_branch, \
         target_branch, source_repository_id, state, draft, closed_reason, \
         CASE WHEN merged_at IS NULL THEN NULL \
              ELSE to_char(merged_at AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') END AS merged_at, \
         merged_by, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at, \
         to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS updated_at, \
         CASE WHEN closed_at IS NULL THEN NULL \
              ELSE to_char(closed_at AT TIME ZONE 'UTC', \
                   'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') END AS closed_at \
         FROM pull_requests ORDER BY repository_id, number",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut pull_ids = HashSet::new();
    for row in pulls {
        let repository_id: Uuid = row.try_get("repository_id")?;
        let source_repository_id: Uuid = row.try_get("source_repository_id")?;
        if !repo_ids.contains(&repository_id) || !repo_ids.contains(&source_repository_id) {
            continue;
        }
        let id: Uuid = row.try_get("id")?;
        pull_ids.insert(id);
        records.push(MetadataRecord::PullRequest(PullRequestRecord {
            id,
            repository_id,
            number: row.try_get("number")?,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            author_id: row.try_get("author_id")?,
            source_branch: row.try_get("source_branch")?,
            target_branch: row.try_get("target_branch")?,
            source_repository_id,
            state: row.try_get("state")?,
            draft: row.try_get("draft")?,
            closed_reason: row.try_get("closed_reason")?,
            merged_at: row.try_get("merged_at")?,
            merged_by: row.try_get("merged_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            closed_at: row.try_get("closed_at")?,
        }));
    }

    let reviews = sqlx::query(
        "SELECT id, pull_request_id, reviewer_id, verdict, commit_id, body, \
         to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
         FROM pull_request_reviews ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in reviews {
        let pull_request_id: Uuid = row.try_get("pull_request_id")?;
        if !pull_ids.contains(&pull_request_id) {
            continue;
        }
        records.push(MetadataRecord::PullRequestReview(ReviewRecord {
            id: row.try_get("id")?,
            pull_request_id,
            reviewer_id: row.try_get("reviewer_id")?,
            verdict: row.try_get("verdict")?,
            commit_id: row.try_get("commit_id")?,
            body: row.try_get("body")?,
            created_at: row.try_get("created_at")?,
        }));
    }

    Ok(records)
}

async fn export_git(
    staging: &Path,
    storage_root: &Path,
    records: &[MetadataRecord],
) -> Result<Vec<GitPart>> {
    let mut parts = Vec::new();
    let storage = Storage::open_existing(storage_root).with_context(|| {
        format!(
            "the repository storage {} is missing; an installation has one after first start",
            storage_root.display()
        )
    })?;
    let git_root = staging.join("git");
    fs::create_dir_all(&git_root)?;
    for record in records {
        let MetadataRecord::Repository(repository) = record else {
            continue;
        };
        let source = storage.repository_path(repository.id);
        if !source.is_dir() {
            bail!(
                "repository {} ({}) is active but its Git directory {} is missing",
                repository.name,
                repository.id,
                source.display()
            );
        }
        let destination = git_root.join(format!("{}.git", repository.id));
        let output = Command::new("git")
            .args(["clone", "--mirror", "--no-hardlinks", "--quiet"])
            .arg(&source)
            .arg(&destination)
            .output()
            .context("cannot run git clone for the export")?;
        if !output.status.success() {
            bail!(
                "cannot export repository {}: {}",
                repository.id,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let (refs, refs_sha256) = refs_digest(&destination)?;
        let head = symbolic_head(&destination)?;
        parts.push(GitPart {
            repository: repository.id,
            path: format!("git/{}.git", repository.id),
            refs,
            refs_sha256,
            head,
        });
    }
    parts.sort_by_key(|part| part.repository);
    Ok(parts)
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

pub async fn run_import(options: &ImportOptions) -> Result<ImportReport> {
    if options.storage_root.as_os_str().is_empty() {
        bail!("storage root is empty; pass --storage-root");
    }
    // The repositories record an absolute `core.hooksPath`; resolve a relative
    // destination now, before the staging tree is renamed.
    let storage_root = if options.storage_root.is_absolute() {
        options.storage_root.clone()
    } else {
        std::env::current_dir()?.join(&options.storage_root)
    };
    // Everything is validated before a database is created or a byte is
    // written to the destination.
    let inspected = inspect_export(&options.archive)?;
    let manifest = &inspected.manifest;
    let database = database_name(&options.database_url)?;
    if crate::backup::destination_database_exists(&options.database_url, &database).await? {
        bail!(
            "destination database `{database}` already exists; import needs a fresh database \
             (an export is a whole installation, not a delta, and a re-run is refused)"
        );
    }
    if storage_root.exists() {
        let empty = fs::read_dir(&storage_root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            bail!(
                "destination storage {} is not empty; import needs a fresh installation",
                storage_root.display()
            );
        }
    }

    let parent = storage_root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.import-{}",
        storage_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("storage"),
        Uuid::new_v4().simple()
    ));

    // Create the destination database here, so the failure path below drops it
    // only when this process created it. A concurrent import that wins the name
    // is left untouched.
    if !crate::backup::destination_create_database(&options.database_url, &database).await? {
        bail!(
            "destination database `{database}` already exists; import needs a fresh database \
             (a re-run is refused), and a database this import did not create is never dropped"
        );
    }

    let result = import_into(
        &options.archive,
        &staging,
        &storage_root,
        &options.database_url,
        &inspected,
    )
    .await;
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        if let Err(cleanup) =
            crate::backup::destination_drop_database(&options.database_url, &database).await
        {
            return Err(anyhow::anyhow!(
                "{error}\n\nthe destination database `{database}` could not be dropped \
                 ({cleanup}); it may be partially imported. Inspect it and drop it manually \
                 before retrying the import"
            ));
        }
        return Err(error);
    }

    if storage_root.exists() {
        fs::remove_dir(&storage_root)
            .with_context(|| format!("cannot replace {}", storage_root.display()))?;
    }
    if let Err(error) = fs::rename(&staging, &storage_root) {
        let _ = fs::remove_dir_all(&staging);
        let _ = crate::backup::destination_drop_database(&options.database_url, &database).await;
        return Err(anyhow::anyhow!(
            "cannot publish the imported storage at {}: {error}",
            storage_root.display()
        ));
    }

    Ok(ImportReport {
        archive: options.archive.clone(),
        database,
        storage_root,
        metadata_records: manifest.counts.values().sum(),
        repositories: manifest.git.len(),
        objects: manifest.objects.records as usize,
        format_version: manifest.format_version,
    })
}

struct InspectedExport {
    manifest: ExportManifest,
    records: Vec<MetadataRecord>,
    objects: Vec<ExportedObject>,
}

fn inspect_export(archive: &Path) -> Result<InspectedExport> {
    let manifest = read_manifest(archive)?;
    let (records, metadata_bytes) = read_metadata(archive, &manifest)?;
    let (objects, object_bytes) = read_object_manifest(archive, &manifest)?;
    check_counts(&manifest, &records)?;
    validate_records(&records)?;
    check_git_parts(archive, &manifest, &records)?;

    for object in &objects {
        let expected_path = format!(
            "objects/data/{}",
            object_relative_path(object.tenant, &object.key).display()
        );
        if object.path != expected_path {
            bail!(
                "object {} ({}) names path {:?}, expected {:?}",
                object.key,
                object.tenant,
                object.path,
                expected_path
            );
        }
        let path = archive.join(&object.path);
        let bytes = fs::read(&path)
            .with_context(|| format!("cannot read the exported object {}", path.display()))?;
        if bytes.len() as u64 != object.bytes {
            bail!(
                "object {} ({}) is {} bytes in the export, the manifest says {}",
                object.key,
                object.tenant,
                bytes.len(),
                object.bytes
            );
        }
        let actual = sha256_bytes(&bytes);
        if actual != object.sha256 {
            bail!(
                "object {} ({}) failed verification: manifest sha256 {}, read {}; the import \
                 refuses before writing anything",
                object.key,
                object.tenant,
                object.sha256,
                actual
            );
        }
    }
    if manifest.objects.records != objects.len() as u64
        || manifest.objects.payload_bytes != objects.iter().map(|o| o.bytes).sum::<u64>()
        || manifest.objects.sha256 != sha256_bytes(&object_bytes)
        || manifest.objects.bytes != object_bytes.len() as u64
    {
        bail!("the object manifest does not agree with its own counts and checksum");
    }
    // Exact tolerance: an object payload that the manifest does not name, and
    // a Git directory that the manifest does not name, are a failed import,
    // never a warning. (ADR 0014 open item 7.)
    let expected_objects: HashSet<String> =
        objects.iter().map(|object| object.path.clone()).collect();
    let actual_objects = files_under(archive, "objects/data")?;
    if actual_objects != expected_objects {
        let extra: Vec<_> = actual_objects
            .difference(&expected_objects)
            .take(3)
            .collect();
        let missing: Vec<_> = expected_objects
            .difference(&actual_objects)
            .take(3)
            .collect();
        bail!(
            "the export's object payloads are not exactly the manifest (extra {extra:?}, \
             missing {missing:?})"
        );
    }
    let expected_git: HashSet<String> = manifest.git.iter().map(|part| part.path.clone()).collect();
    let actual_git = entries_under(archive, "git")?;
    if actual_git != expected_git {
        let extra: Vec<_> = actual_git.difference(&expected_git).take(3).collect();
        let missing: Vec<_> = expected_git.difference(&actual_git).take(3).collect();
        bail!(
            "the export's Git directories are not exactly the manifest (extra {extra:?}, \
             missing {missing:?})"
        );
    }
    if manifest.metadata.sha256 != sha256_bytes(&metadata_bytes)
        || manifest.metadata.bytes != metadata_bytes.len() as u64
        // The part's line count includes the format header; the records start
        // on line 2.
        || manifest.metadata.lines != records.len() as u64 + 1
    {
        bail!("the metadata part does not agree with its own checksum and count");
    }
    Ok(InspectedExport {
        manifest,
        records,
        objects,
    })
}

async fn import_into(
    archive: &Path,
    staging: &Path,
    final_storage: &Path,
    database_url: &str,
    inspected: &InspectedExport,
) -> Result<()> {
    let config = Config {
        database_url: database_url.to_owned(),
        ..Config::default()
    };
    let pool = crate::connect(&config).await?;
    crate::MIGRATOR.run(&pool).await.map_err(|_| {
        anyhow::anyhow!("migration of the imported database failed; inspect database availability")
    })?;

    // Git and objects go into a staging tree that is published only after the
    // database transaction commits.
    import_storage(
        archive,
        staging,
        final_storage,
        &inspected.manifest,
        &inspected.records,
        &inspected.objects,
    )?;

    let result = async {
        let mut tx = pool.begin().await?;
        insert_metadata(&mut tx, &inspected.records).await?;
        verify_imported(&mut tx, &inspected.records).await?;
        tx.commit().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    pool.close().await;
    result?;
    Ok(())
}

fn import_storage(
    archive: &Path,
    staging: &Path,
    final_storage: &Path,
    manifest: &ExportManifest,
    records: &[MetadataRecord],
    objects: &[ExportedObject],
) -> Result<()> {
    // The repositories are built in the staging tree, but their `core.hooksPath`
    // names the final location: the server writes and execs hooks there, and
    // the staging directory disappears once the tree is published.
    let storage = Storage::open_with(
        staging,
        StorageOptions {
            hooks_path: final_storage.join("hooks"),
            max_input_size: Config::default().git_push_limit_bytes,
        },
    )?;
    let formats: HashMap<Uuid, ObjectFormat> = records
        .iter()
        .filter_map(|record| match record {
            MetadataRecord::Repository(repository) => Some((
                repository.id,
                if repository.object_format == "sha256" {
                    ObjectFormat::Sha256
                } else {
                    ObjectFormat::Sha1
                },
            )),
            _ => None,
        })
        .collect();

    // Create each destination repository through the storage interface, then
    // fetch the exported refs into it. The destination keeps its own
    // configuration and an empty journal (the journal position is runtime
    // bookkeeping, not metadata).
    for part in &manifest.git {
        let source = archive.join(&part.path);
        let format = formats.get(&part.repository).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "the export has Git history for {} but no repository record names its format",
                part.repository
            )
        })?;
        let destination = storage.create(part.repository, Uuid::new_v4(), format)?;
        fetch_refs(&source, &destination)?;
        if let Some(head) = &part.head {
            run_git(&[
                "--git-dir".into(),
                destination.to_string_lossy().into_owned(),
                "symbolic-ref".into(),
                "HEAD".into(),
                head.clone(),
            ])?;
        }
        let (refs, digest) = refs_digest(&destination)?;
        if refs != part.refs || digest != part.refs_sha256 {
            bail!(
                "repository {} was imported with different refs than the manifest records",
                part.repository
            );
        }
    }

    let object_root = object_root(staging);
    let store = FsObjectStore::open(&object_root)?;
    for object in objects {
        let key = ObjectKey::parse(&object.key)?;
        let tenant = TenantId::new(object.tenant);
        let path = archive.join(&object.path);
        let bytes = fs::read(&path)
            .with_context(|| format!("cannot read the exported object {}", path.display()))?;
        if bytes.len() as u64 != object.bytes || sha256_bytes(&bytes) != object.sha256 {
            bail!(
                "object {} ({}) changed between validation and import; refusing",
                object.key,
                object.tenant
            );
        }
        let meta: ObjectMeta = if object.appendable {
            let created = store.create_appendable(tenant, &key)?;
            if bytes.is_empty() {
                created
            } else {
                store.append(tenant, &key, &bytes)?
            }
        } else {
            store.put(tenant, &key, &bytes)?
        };
        if meta.bytes != object.bytes || meta.sha256 != object.sha256 {
            bail!(
                "object {} ({}) was stored with a different checksum than the manifest",
                object.key,
                object.tenant
            );
        }
        store.verify(tenant, &key)?;
    }
    let imported: Vec<ObjectRecord> = store.inventory()?;
    let expected: Vec<ObjectRecord> = objects
        .iter()
        .map(|object| ObjectRecord {
            tenant: object.tenant,
            key: object.key.clone(),
            bytes: object.bytes,
            sha256: object.sha256.clone(),
        })
        .collect();
    if normalize_inventory(&imported) != normalize_inventory(&expected) {
        bail!("the imported object inventory does not match the export manifest");
    }
    Ok(())
}

fn fetch_refs(source: &Path, destination: &Path) -> Result<()> {
    let refs = list_refs(source)?;
    if refs.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(destination)
        .args(["fetch", "--force", "--prune", "--no-tags"])
        .arg(source);
    for reference in &refs {
        command.arg(format!("+{reference}:{reference}"));
    }
    let output = command
        .output()
        .context("cannot run git fetch during import")?;
    if !output.status.success() {
        bail!(
            "cannot import Git refs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn list_refs(repository: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["--git-dir"])
        .arg(repository)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .context("cannot run git for-each-ref")?;
    if !output.status.success() {
        bail!(
            "cannot list refs in {}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

async fn insert_metadata(conn: &mut sqlx::PgConnection, records: &[MetadataRecord]) -> Result<()> {
    // Fixed kind order, the same order the exporter writes. Foreign keys and
    // the membership/team/grant triggers require it.
    for record in records {
        if let MetadataRecord::Account(account) = record {
            sqlx::query(
                "INSERT INTO accounts \
                 (id, username, password_hash, is_admin, suspended, email, \
                  email_verified_at, created_at, must_change_password) \
                 VALUES ($1, $2, '!githearth-export-import-unusable', $3, $4, $5, \
                         $6::timestamptz, $7::timestamptz, true)",
            )
            .bind(account.id)
            .bind(&account.username)
            .bind(account.is_admin)
            .bind(account.suspended)
            .bind(&account.email)
            .bind(&account.email_verified_at)
            .bind(&account.created_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    for record in records {
        if let MetadataRecord::Organization(organization) = record {
            sqlx::query(
                "INSERT INTO organizations (id, slug, name, suspended, created_at) \
                 VALUES ($1, $2, $3, $4, $5::timestamptz)",
            )
            .bind(organization.id)
            .bind(&organization.slug)
            .bind(&organization.name)
            .bind(organization.suspended)
            .bind(&organization.created_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    for record in records {
        if let MetadataRecord::Membership(membership) = record {
            sqlx::query(
                "INSERT INTO memberships (organization_id, account_id, role) \
                 VALUES ($1, $2, $3)",
            )
            .bind(membership.organization_id)
            .bind(membership.account_id)
            .bind(&membership.role)
            .execute(&mut *conn)
            .await?;
        }
    }
    // Teams may name a parent; insert roots, then repeatedly the next level.
    let teams: Vec<&TeamRecord> = records
        .iter()
        .filter_map(|record| match record {
            MetadataRecord::Team(team) => Some(team),
            _ => None,
        })
        .collect();
    let mut remaining: Vec<&TeamRecord> = teams.clone();
    let mut inserted: HashSet<Uuid> = HashSet::new();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for team in remaining {
            if team
                .parent_id
                .is_some_and(|parent| !inserted.contains(&parent))
            {
                next.push(team);
                continue;
            }
            sqlx::query(
                "INSERT INTO teams (id, organization_id, parent_id, slug, name, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6::timestamptz)",
            )
            .bind(team.id)
            .bind(team.organization_id)
            .bind(team.parent_id)
            .bind(&team.slug)
            .bind(&team.name)
            .bind(&team.created_at)
            .execute(&mut *conn)
            .await?;
            inserted.insert(team.id);
        }
        remaining = next;
        if remaining.len() == before {
            bail!("export metadata contains a team parent cycle");
        }
    }
    for record in records {
        if let MetadataRecord::TeamMember(member) = record {
            sqlx::query("INSERT INTO team_members (team_id, account_id) VALUES ($1, $2)")
                .bind(member.team_id)
                .bind(member.account_id)
                .execute(&mut *conn)
                .await?;
        }
    }
    // A fork names its parent, so insert roots first and then each next level.
    let repositories: Vec<&RepositoryRecord> = records
        .iter()
        .filter_map(|record| match record {
            MetadataRecord::Repository(repository) => Some(repository),
            _ => None,
        })
        .collect();
    let mut remaining: Vec<&RepositoryRecord> = repositories.clone();
    let mut inserted_repositories: HashSet<Uuid> = HashSet::new();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut next = Vec::new();
        for repository in remaining {
            if repository
                .parent_repository_id
                .is_some_and(|parent| !inserted_repositories.contains(&parent))
            {
                next.push(repository);
                continue;
            }
            sqlx::query(
                "INSERT INTO repositories \
                 (id, organization_id, name, description, visibility, archived, status, \
                  operation_id, created_by, created_at, updated_at, object_format, \
                  parent_repository_id, journal_applied) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'active', $7, $8, $9::timestamptz, \
                         $10::timestamptz, $11, $12, 0)",
            )
            .bind(repository.id)
            .bind(repository.organization_id)
            .bind(&repository.name)
            .bind(&repository.description)
            .bind(&repository.visibility)
            .bind(repository.archived)
            .bind(Uuid::new_v4())
            .bind(repository.created_by)
            .bind(&repository.created_at)
            .bind(&repository.updated_at)
            .bind(&repository.object_format)
            .bind(repository.parent_repository_id)
            .execute(&mut *conn)
            .await?;
            inserted_repositories.insert(repository.id);
        }
        remaining = next;
        if remaining.len() == before {
            bail!("export metadata contains a repository parent cycle");
        }
    }
    for record in records {
        if let MetadataRecord::Grant(grant) = record {
            sqlx::query(
                "INSERT INTO grants \
                 (id, role, subject_kind, subject_id, scope_kind, scope_id, repository_id, \
                  expires_at, granted_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9, $10::timestamptz)",
            )
            .bind(grant.id)
            .bind(&grant.role)
            .bind(&grant.subject_kind)
            .bind(grant.subject_id)
            .bind(&grant.scope_kind)
            .bind(grant.scope_id)
            .bind(grant.repository_id)
            .bind(&grant.expires_at)
            .bind(grant.granted_by)
            .bind(&grant.created_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    for record in records {
        if let MetadataRecord::Issue(issue) = record {
            sqlx::query(
                "INSERT INTO issues \
                 (id, repository_id, number, title, body, author_id, state, \
                  created_at, updated_at, closed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $9::timestamptz, \
                         $10::timestamptz)",
            )
            .bind(issue.id)
            .bind(issue.repository_id)
            .bind(issue.number)
            .bind(&issue.title)
            .bind(&issue.body)
            .bind(issue.author_id)
            .bind(&issue.state)
            .bind(&issue.created_at)
            .bind(&issue.updated_at)
            .bind(&issue.closed_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    for record in records {
        if let MetadataRecord::PullRequest(pull) = record {
            sqlx::query(
                "INSERT INTO pull_requests \
                 (id, repository_id, number, title, body, author_id, source_branch, \
                  target_branch, source_repository_id, state, draft, closed_reason, \
                  merged_at, merged_by, created_at, updated_at, closed_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                         $13::timestamptz, $14, $15::timestamptz, $16::timestamptz, \
                         $17::timestamptz)",
            )
            .bind(pull.id)
            .bind(pull.repository_id)
            .bind(pull.number)
            .bind(&pull.title)
            .bind(&pull.body)
            .bind(pull.author_id)
            .bind(&pull.source_branch)
            .bind(&pull.target_branch)
            .bind(pull.source_repository_id)
            .bind(&pull.state)
            .bind(pull.draft)
            .bind(&pull.closed_reason)
            .bind(&pull.merged_at)
            .bind(pull.merged_by)
            .bind(&pull.created_at)
            .bind(&pull.updated_at)
            .bind(&pull.closed_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    for record in records {
        if let MetadataRecord::PullRequestReview(review) = record {
            sqlx::query(
                "INSERT INTO pull_request_reviews \
                 (id, pull_request_id, reviewer_id, verdict, commit_id, body, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)",
            )
            .bind(review.id)
            .bind(review.pull_request_id)
            .bind(review.reviewer_id)
            .bind(&review.verdict)
            .bind(&review.commit_id)
            .bind(&review.body)
            .bind(&review.created_at)
            .execute(&mut *conn)
            .await?;
        }
    }
    // Issue and pull-request numbers share one counter per repository. It is
    // derived state, not metadata; recomputing it keeps the next allocation
    // from colliding with an imported number.
    sqlx::query(
        "INSERT INTO issue_counters (repository_id, last) \
         SELECT repository_id, MAX(number) FROM ( \
           SELECT repository_id, number FROM issues \
           UNION ALL SELECT repository_id, number FROM pull_requests \
         ) numbered GROUP BY repository_id \
         ON CONFLICT (repository_id) DO UPDATE \
         SET last = GREATEST(issue_counters.last, EXCLUDED.last)",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn verify_imported(conn: &mut sqlx::PgConnection, records: &[MetadataRecord]) -> Result<()> {
    let expected = count_records(records);
    for (kind, table) in [
        ("account", "accounts"),
        ("organization", "organizations"),
        ("membership", "memberships"),
        ("team", "teams"),
        ("team_member", "team_members"),
        ("repository", "repositories"),
        ("grant", "grants"),
        ("issue", "issues"),
        ("pull_request", "pull_requests"),
        ("pull_request_review", "pull_request_reviews"),
    ] {
        let expected = expected.get(kind).copied().unwrap_or(0);
        let actual: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *conn)
            .await?;
        if actual != expected as i64 {
            bail!(
                "imported {actual} {table} rows, the export records {expected}; refusing the \
                 import"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading and validating an export
// ---------------------------------------------------------------------------

pub fn read_manifest(archive: &Path) -> Result<ExportManifest> {
    let path = archive.join(MANIFEST_FILE);
    let bytes = fs::read(&path)
        .with_context(|| format!("cannot read the export manifest {}", path.display()))?;
    let manifest: ExportManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse the export manifest {}", path.display()))?;
    check_header(&manifest.format, manifest.format_version)?;
    if manifest.metadata.name != METADATA_FILE || manifest.objects.name != OBJECTS_FILE {
        bail!(
            "the export manifest names parts {:?} and {:?}; expected {METADATA_FILE} and \
             {OBJECTS_FILE}",
            manifest.metadata.name,
            manifest.objects.name
        );
    }
    Ok(manifest)
}

fn check_header(format: &str, version: u32) -> Result<()> {
    if format != EXPORT_FORMAT {
        bail!("unrecognized export format {format:?}; expected {EXPORT_FORMAT:?}");
    }
    if version != EXPORT_FORMAT_VERSION {
        bail!(
            "unsupported export format version {version}; this binary reads \
             {EXPORT_FORMAT_VERSION} only"
        );
    }
    Ok(())
}

fn read_metadata(
    archive: &Path,
    manifest: &ExportManifest,
) -> Result<(Vec<MetadataRecord>, Vec<u8>)> {
    let path = archive.join(&manifest.metadata.name);
    let bytes = fs::read(&path)
        .with_context(|| format!("cannot read the export metadata {}", path.display()))?;
    if sha256_bytes(&bytes) != manifest.metadata.sha256 {
        bail!(
            "the export metadata failed its SHA-256 check; the import refuses before writing \
             anything"
        );
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("the export metadata is not UTF-8"))?;
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("the export metadata is empty"))?;
    let header: MetadataHeader = serde_json::from_str(header_line)
        .map_err(|error| anyhow::anyhow!("the metadata header is not JSON: {error}"))?;
    if header.kind != EXPORT_FORMAT {
        bail!("the metadata header has kind {:?}", header.kind);
    }
    check_header(&header.format, header.format_version)?;
    if header.format_version != manifest.format_version {
        bail!("the metadata header and the manifest disagree on the format version");
    }
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let record: MetadataRecord = serde_json::from_str(line).map_err(|error| {
            anyhow::anyhow!("metadata line {} is not a valid record: {error}", index + 2)
        })?;
        records.push(record);
    }
    Ok((records, bytes))
}

fn read_object_manifest(
    archive: &Path,
    manifest: &ExportManifest,
) -> Result<(Vec<ExportedObject>, Vec<u8>)> {
    let path = archive.join(&manifest.objects.name);
    let bytes = fs::read(&path)
        .with_context(|| format!("cannot read the object manifest {}", path.display()))?;
    if sha256_bytes(&bytes) != manifest.objects.sha256 {
        bail!(
            "the object manifest failed its SHA-256 check; the import refuses before writing \
             anything"
        );
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("the object manifest is not UTF-8"))?;
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("the object manifest is empty"))?;
    let header: ObjectsHeader = serde_json::from_str(header_line)
        .map_err(|error| anyhow::anyhow!("the object manifest header is not JSON: {error}"))?;
    if header.kind != EXPORT_FORMAT {
        bail!("the object manifest header has kind {:?}", header.kind);
    }
    check_header(&header.format, header.format_version)?;
    if header.format_version != manifest.format_version {
        bail!("the object manifest header and the manifest disagree on the format version");
    }
    let mut objects = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let object: ExportedObject = serde_json::from_str(line)
            .map_err(|error| anyhow::anyhow!("object manifest line {}: {error}", index + 2))?;
        // The key grammar makes a path traversal impossible, and the same
        // grammar is what the object store enforces.
        ObjectKey::parse(&object.key)?;
        if !seen.insert((object.tenant, object.key.clone())) {
            bail!(
                "object manifest repeats {} in tenant {}",
                object.key,
                object.tenant
            );
        }
        objects.push(object);
    }
    objects.sort_by(|left, right| {
        (left.tenant, left.key.as_str()).cmp(&(right.tenant, right.key.as_str()))
    });
    Ok((objects, bytes))
}

fn check_counts(manifest: &ExportManifest, records: &[MetadataRecord]) -> Result<()> {
    let actual = count_records(records);
    if actual != manifest.counts {
        bail!(
            "the export's metadata counts do not match its manifest (manifest {:?}, records \
             {:?})",
            manifest.counts,
            actual
        );
    }
    Ok(())
}

fn count_records(records: &[MetadataRecord]) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for record in records {
        *counts.entry(record.kind().to_owned()).or_insert(0) += 1;
    }
    counts
}

fn validate_records(records: &[MetadataRecord]) -> Result<()> {
    let mut accounts = HashSet::new();
    let mut organizations = HashSet::new();
    let mut teams = HashSet::new();
    let mut team_orgs = HashMap::new();
    let mut repositories = HashSet::new();
    let mut repo_orgs = HashMap::new();
    let mut memberships = HashSet::new();
    let mut pulls = HashSet::new();
    let mut issue_numbers = HashSet::new();
    let mut pull_numbers = HashSet::new();
    let mut ids = HashSet::new();
    // Precollect pull ids so a review validates regardless of the order a
    // hand-written export used.
    for record in records {
        if let MetadataRecord::PullRequest(pull) = record {
            pulls.insert(pull.id);
        }
    }

    for record in records {
        let error = |message: String| anyhow::anyhow!("export metadata is invalid: {message}");
        match record {
            MetadataRecord::Account(account) => {
                if !accounts.insert(account.id) {
                    return Err(error(format!("duplicate account {}", account.id)));
                }
                if !ids.insert(account.id) {
                    return Err(error(format!("duplicate record id {}", account.id)));
                }
            }
            MetadataRecord::Organization(organization) => {
                if !organizations.insert(organization.id) {
                    return Err(error(format!("duplicate organization {}", organization.id)));
                }
                if !ids.insert(organization.id) {
                    return Err(error(format!("duplicate record id {}", organization.id)));
                }
            }
            MetadataRecord::Membership(membership) => {
                if !memberships.insert((membership.organization_id, membership.account_id)) {
                    return Err(error("duplicate membership".into()));
                }
            }
            MetadataRecord::Team(team) => {
                if !teams.insert(team.id) {
                    return Err(error(format!("duplicate team {}", team.id)));
                }
                if !ids.insert(team.id) {
                    return Err(error(format!("duplicate record id {}", team.id)));
                }
                team_orgs.insert(team.id, team.organization_id);
            }
            MetadataRecord::TeamMember(_) => {}
            MetadataRecord::Repository(repository) => {
                if !repositories.insert(repository.id) {
                    return Err(error(format!("duplicate repository {}", repository.id)));
                }
                if !ids.insert(repository.id) {
                    return Err(error(format!("duplicate record id {}", repository.id)));
                }
                repo_orgs.insert(repository.id, repository.organization_id);
            }
            MetadataRecord::Grant(grant) => {
                if !ids.insert(grant.id) {
                    return Err(error(format!("duplicate grant {}", grant.id)));
                }
            }
            MetadataRecord::Issue(issue) => {
                if !ids.insert(issue.id) {
                    return Err(error(format!("duplicate issue {}", issue.id)));
                }
                if !issue_numbers.insert((issue.repository_id, issue.number)) {
                    return Err(error(format!(
                        "duplicate issue number {} in repository {}",
                        issue.number, issue.repository_id
                    )));
                }
            }
            MetadataRecord::PullRequest(pull) => {
                if !ids.insert(pull.id) {
                    return Err(error(format!("duplicate pull request {}", pull.id)));
                }
                if !pull_numbers.insert((pull.repository_id, pull.number)) {
                    return Err(error(format!(
                        "duplicate pull request number {} in repository {}",
                        pull.number, pull.repository_id
                    )));
                }
            }
            MetadataRecord::PullRequestReview(review) => {
                if !ids.insert(review.id) {
                    return Err(error(format!("duplicate review {}", review.id)));
                }
            }
        }
    }

    for record in records {
        let error = |message: String| anyhow::anyhow!("export metadata is invalid: {message}");
        match record {
            MetadataRecord::Membership(membership) => {
                if !organizations.contains(&membership.organization_id) {
                    return Err(error(format!(
                        "membership names organization {} which is not exported",
                        membership.organization_id
                    )));
                }
                if !accounts.contains(&membership.account_id) {
                    return Err(error(format!(
                        "membership names account {} which is not exported",
                        membership.account_id
                    )));
                }
            }
            MetadataRecord::Team(team) => {
                if !organizations.contains(&team.organization_id) {
                    return Err(error(format!(
                        "team {} names organization {} which is not exported",
                        team.id, team.organization_id
                    )));
                }
                if let Some(parent) = team.parent_id {
                    if !teams.contains(&parent) {
                        return Err(error(format!(
                            "team {} names parent {} which is not exported",
                            team.id, parent
                        )));
                    }
                    if team_orgs.get(&parent) != Some(&team.organization_id) {
                        return Err(error(format!(
                            "team {} and its parent belong to different organizations",
                            team.id
                        )));
                    }
                }
            }
            MetadataRecord::TeamMember(member) => {
                let Some(organization) = team_orgs.get(&member.team_id) else {
                    return Err(error(format!(
                        "team member names team {} which is not exported",
                        member.team_id
                    )));
                };
                if !accounts.contains(&member.account_id) {
                    return Err(error(format!(
                        "team member names account {} which is not exported",
                        member.account_id
                    )));
                }
                if !memberships.contains(&(*organization, member.account_id)) {
                    return Err(error(format!(
                        "team member {} is not an organization member",
                        member.account_id
                    )));
                }
            }
            MetadataRecord::Repository(repository) => {
                if !organizations.contains(&repository.organization_id) {
                    return Err(error(format!(
                        "repository {} names organization {} which is not exported",
                        repository.id, repository.organization_id
                    )));
                }
                if !accounts.contains(&repository.created_by) {
                    return Err(error(format!(
                        "repository {} names creator {} which is not exported",
                        repository.id, repository.created_by
                    )));
                }
                if let Some(parent) = repository.parent_repository_id
                    && !repositories.contains(&parent)
                {
                    return Err(error(format!(
                        "repository {} names parent {} which is not exported",
                        repository.id, parent
                    )));
                }
            }
            MetadataRecord::Grant(grant) => {
                if !organizations.contains(&grant.scope_id) {
                    return Err(error(format!(
                        "grant {} names scope {} which is not exported",
                        grant.id, grant.scope_id
                    )));
                }
                if !accounts.contains(&grant.granted_by) {
                    return Err(error(format!(
                        "grant {} names grantor {} which is not exported",
                        grant.id, grant.granted_by
                    )));
                }
                match grant.subject_kind.as_str() {
                    "account" => {
                        if !accounts.contains(&grant.subject_id) {
                            return Err(error(format!(
                                "grant {} names account {} which is not exported",
                                grant.id, grant.subject_id
                            )));
                        }
                        if !memberships.contains(&(grant.scope_id, grant.subject_id)) {
                            return Err(error(format!(
                                "grant {} subject is not a member of the organization",
                                grant.id
                            )));
                        }
                    }
                    "team" => {
                        if !teams.contains(&grant.subject_id)
                            || team_orgs.get(&grant.subject_id) != Some(&grant.scope_id)
                        {
                            return Err(error(format!(
                                "grant {} names a team outside the scope organization",
                                grant.id
                            )));
                        }
                    }
                    other => {
                        return Err(error(format!(
                            "grant {} has subject kind {other:?}",
                            grant.id
                        )));
                    }
                }
                match (grant.scope_kind.as_str(), grant.repository_id) {
                    ("organization", None) => {}
                    ("repository", Some(repository)) => {
                        if repo_orgs.get(&repository) != Some(&grant.scope_id) {
                            return Err(error(format!(
                                "grant {} names a repository outside the scope organization",
                                grant.id
                            )));
                        }
                    }
                    _ => {
                        return Err(error(format!(
                            "grant {} has an inconsistent scope and repository",
                            grant.id
                        )));
                    }
                }
            }
            MetadataRecord::Issue(issue) => {
                if !repositories.contains(&issue.repository_id) {
                    return Err(error(format!(
                        "issue {} names repository {} which is not exported",
                        issue.id, issue.repository_id
                    )));
                }
                if !accounts.contains(&issue.author_id) {
                    return Err(error(format!(
                        "issue {} names author {} which is not exported",
                        issue.id, issue.author_id
                    )));
                }
            }
            MetadataRecord::PullRequest(pull) => {
                if !repositories.contains(&pull.repository_id)
                    || !repositories.contains(&pull.source_repository_id)
                {
                    return Err(error(format!(
                        "pull request {} names a repository which is not exported",
                        pull.id
                    )));
                }
                if !accounts.contains(&pull.author_id) {
                    return Err(error(format!(
                        "pull request {} names author {} which is not exported",
                        pull.id, pull.author_id
                    )));
                }
                if let Some(merged_by) = pull.merged_by
                    && !accounts.contains(&merged_by)
                {
                    return Err(error(format!(
                        "pull request {} names merger {} which is not exported",
                        pull.id, merged_by
                    )));
                }
            }
            MetadataRecord::PullRequestReview(review) => {
                if !pulls.contains(&review.pull_request_id) {
                    return Err(error(format!(
                        "review {} names pull request {} which is not exported",
                        review.id, review.pull_request_id
                    )));
                }
                if !accounts.contains(&review.reviewer_id) {
                    return Err(error(format!(
                        "review {} names reviewer {} which is not exported",
                        review.id, review.reviewer_id
                    )));
                }
            }
            MetadataRecord::Account(_) | MetadataRecord::Organization(_) => {}
        }
    }

    // A parent cycle cannot be inserted (and, for teams, would never be
    // reachable); refuse it while still validating, before any write.
    let mut team_parents = HashMap::new();
    let mut repository_parents = HashMap::new();
    for record in records {
        match record {
            MetadataRecord::Team(team) => {
                team_parents.insert(team.id, team.parent_id);
            }
            MetadataRecord::Repository(repository) => {
                repository_parents.insert(repository.id, repository.parent_repository_id);
            }
            _ => {}
        }
    }
    for (label, parents) in [("team", &team_parents), ("repository", &repository_parents)] {
        for start in parents.keys() {
            let mut seen = HashSet::new();
            let mut current = Some(*start);
            while let Some(id) = current {
                if !seen.insert(id) {
                    bail!("export metadata contains a {label} parent cycle at {id}");
                }
                current = parents.get(&id).copied().flatten();
            }
        }
    }
    Ok(())
}

fn check_git_parts(
    archive: &Path,
    manifest: &ExportManifest,
    records: &[MetadataRecord],
) -> Result<()> {
    let repositories: HashSet<Uuid> = records
        .iter()
        .filter_map(|record| match record {
            MetadataRecord::Repository(repository) => Some(repository.id),
            _ => None,
        })
        .collect();
    let mut seen = HashSet::new();
    for part in &manifest.git {
        if !repositories.contains(&part.repository) {
            bail!(
                "the manifest has Git history for {} but no repository record",
                part.repository
            );
        }
        if !seen.insert(part.repository) {
            bail!("the manifest repeats Git history for {}", part.repository);
        }
        let expected = format!("git/{}.git", part.repository);
        if part.path != expected {
            bail!(
                "Git part for {} names {:?}, expected {:?}",
                part.repository,
                part.path,
                expected
            );
        }
        let path = archive.join(&part.path);
        if !path.is_dir() {
            bail!("the manifest names {} but it is missing", path.display());
        }
        let (refs, digest) = refs_digest(&path)?;
        if refs != part.refs || digest != part.refs_sha256 || symbolic_head(&path)? != part.head {
            bail!(
                "the Git history for {} does not match the manifest's refs checksum",
                part.repository
            );
        }
    }
    for repository in repositories {
        if !seen.contains(&repository) {
            bail!("repository {repository} has no Git history in the export");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn object_root(storage_root: &Path) -> PathBuf {
    storage_root.join("objects")
}

fn object_relative_path(tenant: Uuid, key: &str) -> PathBuf {
    let mut path = PathBuf::from(tenant.simple().to_string());
    for segment in key.split('/') {
        path.push(segment);
    }
    path
}

/// Every regular file under `prefix`, as `prefix/relative` strings. Symlinks
/// are refused: an export must not be able to point the reader at host files.
fn files_under(archive: &Path, prefix: &str) -> Result<HashSet<String>> {
    let root = archive.join(prefix);
    if !root.exists() {
        return Ok(HashSet::new());
    }
    let mut files = HashSet::new();
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!("the export contains a symlink at {}", path.display());
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(archive)
                    .map_err(|_| anyhow::anyhow!("export path escapes {}", archive.display()))?;
                files.insert(relative.to_string_lossy().into_owned());
            }
        }
    }
    Ok(files)
}

/// The immediate entries under `prefix`, as `prefix/name` strings.
fn entries_under(archive: &Path, prefix: &str) -> Result<HashSet<String>> {
    let root = archive.join(prefix);
    if !root.exists() {
        return Ok(HashSet::new());
    }
    let mut entries = HashSet::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "the export contains a symlink at {}",
                entry.path().display()
            );
        }
        entries.insert(format!("{prefix}/{}", entry.file_name().to_string_lossy()));
    }
    Ok(entries)
}

fn normalize_inventory(records: &[ObjectRecord]) -> Vec<(Uuid, String, u64, String)> {
    let mut normalized: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record.tenant,
                record.key.clone(),
                record.bytes,
                record.sha256.clone(),
            )
        })
        .collect();
    normalized.sort();
    normalized
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn part_for(directory: &Path, name: &str) -> Result<Part> {
    let path = directory.join(name);
    let bytes = fs::metadata(&path)?.len();
    let text = fs::read_to_string(&path)?;
    Ok(Part {
        name: name.into(),
        sha256: sha256_file(&path)?,
        bytes,
        lines: text.lines().filter(|line| !line.is_empty()).count() as u64,
    })
}

fn object_part_for(
    directory: &Path,
    name: &str,
    records: usize,
    objects: &[ExportedObject],
) -> Result<ObjectPart> {
    let path = directory.join(name);
    Ok(ObjectPart {
        name: name.into(),
        sha256: sha256_file(&path)?,
        bytes: fs::metadata(&path)?.len(),
        records: records as u64,
        payload_bytes: objects.iter().map(|object| object.bytes).sum(),
    })
}

fn write_json_line<T: Serialize>(file: &mut fs::File, value: &T) -> Result<()> {
    let line = serde_json::to_vec(value)?;
    file.write_all(&line)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn private_file(path: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    Ok(file)
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn set_private_dir(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn run_git(args: &[String]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn refs_digest(repository: &Path) -> Result<(u64, String)> {
    let output = Command::new("git")
        .args(["--git-dir"])
        .arg(repository)
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .output()
        .context("cannot run git for-each-ref")?;
    if !output.status.success() {
        bail!(
            "cannot read refs in {}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    let mut digest = Sha256::new();
    for line in &lines {
        digest.update(line.as_bytes());
        digest.update(b"\n");
    }
    Ok((lines.len() as u64, hex::encode(digest.finalize())))
}

fn symbolic_head(repository: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["--git-dir"])
        .arg(repository)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .context("cannot run git symbolic-ref")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn database_name(url: &str) -> Result<String> {
    let options = PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection URL"))?;
    options
        .get_database()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("database URL names no database"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_export_version_is_refused_instead_of_guessed() {
        let error = check_header(EXPORT_FORMAT, EXPORT_FORMAT_VERSION + 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported export format version"),
            "{error}"
        );
        check_header(EXPORT_FORMAT, EXPORT_FORMAT_VERSION).unwrap();
    }

    #[test]
    fn an_unknown_export_format_is_refused() {
        let error = check_header("other-export", EXPORT_FORMAT_VERSION).unwrap_err();
        assert!(
            error.to_string().contains("unrecognized export format"),
            "{error}"
        );
    }
}
