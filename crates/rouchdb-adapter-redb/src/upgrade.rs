//! On-disk format management: recognizing which version wrote a file, the
//! format guard that keeps older rouchdb versions out of files they would
//! misread, the explicit upgrade of files written by rouchdb <= 0.4, and the
//! verified backup taken before upgrading.
//!
//! # Why a guard
//!
//! rouchdb <= 0.4 opens a file by opening its six tables, `metadata` among
//! them, as `Table<&str, &[u8]>`. A 0.4 build reading a 0.5 file does not
//! fail cleanly: it cannot decode the flat document records written by 0.5,
//! treats those documents as missing and silently replaces their whole
//! history on the next write, and it cannot find any attachment. So a 0.5
//! file must be one that 0.4 refuses to open. redb checks the key and value
//! types of every table it opens, so the `metadata` table of a 0.5 file is a
//! guard table whose value type is named [`FORMAT_GUARD_TYPE_NAME`]: 0.4
//! (and 0.1 - 0.3) fail in `open` with
//! `metadata is of type Table<&str, rouchdb-format-2 (this file requires rouchdb >= 0.5)>`
//! before writing anything. The 0.5 metadata lives in `rouchdb_meta`.
//!
//! Every committed 0.5 file has the guard: it is written in the same
//! transaction that creates the file or upgrades it, and `destroy` keeps it.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use redb::{MultimapTableHandle, ReadTransaction, TableError, TableHandle, WriteTransaction};

use super::*;

/// The type name of the format guard's values.
///
/// NEVER CHANGE THIS STRING. Every file written by rouchdb 0.5 or later
/// stores it, and it is the text rouchdb 0.4 and earlier print when they
/// refuse such a file. Changing it would make every existing 0.5 file unreadable (redb
/// checks it on open). A future incompatible format must use a *new* guard
/// (for instance by changing the guard table's type again), never edit this
/// one.
pub(crate) const FORMAT_GUARD_TYPE_NAME: &str =
    "rouchdb-format-2 (this file requires rouchdb >= 0.5)";

/// Zero-sized value type of the format guard table. Only its type name
/// matters (see [`FORMAT_GUARD_TYPE_NAME`]).
#[derive(Debug)]
pub(crate) struct FormatGuard;

impl redb::Value for FormatGuard {
    type SelfType<'a>
        = FormatGuard
    where
        Self: 'a;
    type AsBytes<'a>
        = [u8; 0]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        Some(0)
    }

    fn from_bytes<'a>(_data: &'a [u8]) -> FormatGuard
    where
        Self: 'a,
    {
        FormatGuard
    }

    fn as_bytes<'a, 'b: 'a>(_value: &'a FormatGuard) -> [u8; 0]
    where
        Self: 'b,
    {
        []
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(FORMAT_GUARD_TYPE_NAME)
    }
}

/// The format guard: the `metadata` table of a 0.5 file (see the module
/// documentation). NEVER CHANGE its name or types.
pub(crate) const GUARD_TABLE: TableDefinition<&str, FormatGuard> = TableDefinition::new("metadata");
const GUARD_KEY: &str = "format";

/// The metadata table of files written by rouchdb <= 0.4 and by unreleased
/// 0.5 development builds. Same name as [`GUARD_TABLE`], other value type.
pub(crate) const LEGACY_META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

/// Tables of a current file, the guard included.
const CURRENT_TABLES: [&str; 7] = [
    "docs",
    "rev_data",
    "changes",
    "local_docs",
    "attachments",
    "rouchdb_meta",
    "metadata",
];

/// How many document ids an [`UpgradeReport`] lists per category.
pub const REPORT_SAMPLE: usize = 50;

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// What to do when a file written by an older rouchdb is opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpgradePolicy {
    /// Refuse files written by rouchdb <= 0.4 with
    /// [`RouchError::UpgradeRequired`], without modifying them (the
    /// default).
    #[default]
    Refuse,
    /// Upgrade after writing a verified backup of the file: to the given
    /// path, or to `<file>.rouchdb-0.4.bak` next to it. The backup is a
    /// complete copy that rouchdb 0.4 can still open. The upgrade is refused
    /// if the backup path already exists.
    WithBackup(Option<PathBuf>),
    /// Upgrade in place without a backup. The upgrade itself is atomic (it
    /// commits completely or not at all), but afterwards the file can no
    /// longer be opened by rouchdb 0.4.
    InPlaceNoBackup,
}

/// Options of [`RedbAdapter::open_with`](crate::RedbAdapter::open_with).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenOptions {
    /// How files written by an older rouchdb are handled.
    pub upgrade: UpgradePolicy,
}

impl OpenOptions {
    /// The defaults: files written by rouchdb <= 0.4 are refused.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the [`UpgradePolicy`].
    pub fn upgrade(mut self, policy: UpgradePolicy) -> Self {
        self.upgrade = policy;
        self
    }
}

/// The on-disk format a file was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoredFormat {
    /// Written by rouchdb 0.1 - 0.4.
    Legacy,
    /// Written by an unreleased 0.5 development build (on-disk `schema` 1 or
    /// 2 without the format guard). Upgraded automatically on open.
    PreRelease {
        /// The schema number recorded in the file.
        schema: u32,
    },
    /// Already in the current format.
    Current,
}

/// What an upgrade found and did. Counts describe the file after the
/// upgrade; the id lists hold at most [`REPORT_SAMPLE`] ids each.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpgradeReport {
    /// The upgraded file.
    pub path: PathBuf,
    /// The format the file was in.
    pub from: StoredFormat,
    /// Whether changes were committed (`false` for a dry run
    /// ([`RedbAdapter::inspect_upgrade`](crate::RedbAdapter::inspect_upgrade))
    /// and for a file that already was current).
    pub upgraded: bool,
    /// Where the backup was written, if one was.
    pub backup: Option<PathBuf>,
    /// Live documents.
    pub doc_count: u64,
    /// Deleted documents.
    pub doc_del_count: u64,
    /// Attachment byte entries re-keyed from `doc_id\0name` to their digest.
    pub attachments_rekeyed: u64,
    /// Attachment references (in any stored revision) whose bytes are not in
    /// the file. rouchdb <= 0.4 stored one copy of the bytes per document
    /// and attachment name, so re-attaching a name overwrote the bytes older
    /// revisions pointed at: these were already lost before the upgrade.
    pub missing_attachment_refs: u64,
    /// Documents with at least one such missing attachment (sample).
    pub docs_with_missing_attachments: Vec<String>,
    /// `_local/` documents that rouchdb <= 0.4 stored as ordinary documents
    /// and that were moved to the local document store (where 0.5 reads
    /// them). Their current body is kept, with revision `0-N` where `N` is
    /// the generation of their last revision.
    pub local_docs_moved: u64,
    /// `_local/` documents whose last revision was a deletion: removed.
    pub local_tombstones_dropped: u64,
    /// Conflicting (losing) revisions of moved `_local/` documents: dropped,
    /// as local documents have no revision tree.
    pub local_conflicts_dropped: u64,
    /// Attachments of moved `_local/` documents: dropped, as local documents
    /// have none (their bytes stay until the next compaction).
    pub local_attachments_dropped: u64,
    /// Revision ids written in upper-case hexadecimal (accepted by
    /// rouchdb <= 0.4 in replicated writes) and rewritten in lower case,
    /// which is how 0.5 looks revisions up.
    pub revs_normalized: u64,
    /// Stored bodies of non-leaf (old) revisions. The first
    /// [`compact`](rouchdb_core::adapter::Adapter::compact) after the upgrade
    /// deletes them (rouchdb <= 0.4's `compact` did nothing).
    pub old_revision_bodies: u64,
    /// Documents with attachment bytes that only old revisions reference
    /// (sample): the first compaction deletes those bytes. rouchdb <= 0.4
    /// dropped the attachments of a document whose body was updated without
    /// them, so they are often only reachable from an older revision.
    pub docs_with_old_only_attachments: Vec<String>,
    /// Total number of such documents.
    pub docs_with_old_only_attachments_count: u64,
    /// Total number of documents with missing attachment bytes.
    pub docs_with_missing_attachments_count: u64,
}

impl UpgradeReport {
    fn new(path: &Path, from: StoredFormat) -> Self {
        UpgradeReport {
            path: path.to_path_buf(),
            from,
            upgraded: false,
            backup: None,
            doc_count: 0,
            doc_del_count: 0,
            attachments_rekeyed: 0,
            missing_attachment_refs: 0,
            docs_with_missing_attachments: Vec::new(),
            local_docs_moved: 0,
            local_tombstones_dropped: 0,
            local_conflicts_dropped: 0,
            local_attachments_dropped: 0,
            revs_normalized: 0,
            old_revision_bodies: 0,
            docs_with_old_only_attachments: Vec::new(),
            docs_with_old_only_attachments_count: 0,
            docs_with_missing_attachments_count: 0,
        }
    }
}

impl fmt::Display for UpgradeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let from = match self.from {
            StoredFormat::Legacy => "rouchdb 0.4 or earlier".to_string(),
            StoredFormat::PreRelease { schema } => {
                format!("a 0.5 development build (schema {})", schema)
            }
            StoredFormat::Current => "the current format".to_string(),
        };
        writeln!(f, "file: {}", self.path.display())?;
        writeln!(f, "written by: {}", from)?;
        let state = match (self.from, self.upgraded) {
            (StoredFormat::Current, _) => "already current, nothing to do",
            (_, true) => "upgraded",
            (_, false) => "not modified (dry run)",
        };
        writeln!(f, "status: {}", state)?;
        if let Some(backup) = &self.backup {
            writeln!(f, "backup: {}", backup.display())?;
        }
        writeln!(
            f,
            "documents: {} live, {} deleted",
            self.doc_count, self.doc_del_count
        )?;
        if self.from == StoredFormat::Current {
            return Ok(());
        }
        writeln!(
            f,
            "attachment byte entries re-keyed by digest: {}",
            self.attachments_rekeyed
        )?;
        writeln!(
            f,
            "_local/ documents moved to the local store: {} (tombstones dropped: {}, \
             conflicting revisions dropped: {}, attachments dropped: {})",
            self.local_docs_moved,
            self.local_tombstones_dropped,
            self.local_conflicts_dropped,
            self.local_attachments_dropped
        )?;
        writeln!(
            f,
            "upper-case revision ids normalized: {}",
            self.revs_normalized
        )?;
        writeln!(
            f,
            "attachment references whose bytes were already missing: {} (in {} documents{})",
            self.missing_attachment_refs,
            self.docs_with_missing_attachments_count,
            sample_suffix(&self.docs_with_missing_attachments)
        )?;
        writeln!(
            f,
            "old revision bodies the first compact() will delete: {}",
            self.old_revision_bodies
        )?;
        write!(
            f,
            "documents with attachment bytes only old revisions reference \
             (deleted by the first compact()): {}{}",
            self.docs_with_old_only_attachments_count,
            sample_suffix(&self.docs_with_old_only_attachments)
        )
    }
}

fn sample_suffix(ids: &[String]) -> String {
    if ids.is_empty() {
        String::new()
    } else {
        format!(": {:?}", ids)
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

enum Detected {
    /// No tables: a new file.
    Empty,
    /// Guarded 0.5 file. `missing_tables` if some table must be created.
    Current {
        meta: MetaRecord,
        missing_tables: bool,
    },
    /// Unguarded rouchdb file: legacy (`schema` 0) or pre-release.
    Unguarded { schema: u32 },
}

fn newer_error(path: &Path, schema: u32) -> RouchError {
    RouchError::DatabaseError(format!(
        "{}: on-disk schema version {} is newer than the {} this version of rouchdb \
         supports; open it with a newer rouchdb (the file was not modified)",
        path.display(),
        schema,
        SCHEMA_VERSION
    ))
}

fn detect(db: &Database, path: &Path) -> Result<Detected> {
    let txn = db_err!(db.begin_read())?;
    let names: Vec<String> = db_err!(txn.list_tables())?
        .map(|t| t.name().to_string())
        .collect();
    let multimaps: Vec<String> = db_err!(txn.list_multimap_tables())?
        .map(|t| t.name().to_string())
        .collect();
    if names.is_empty() && multimaps.is_empty() {
        return Ok(Detected::Empty);
    }

    match txn.open_table(GUARD_TABLE) {
        Ok(_) => {
            let meta = match txn.open_table(META_TABLE) {
                Ok(table) => read_meta(&table)?,
                Err(e) => {
                    return Err(RouchError::DatabaseError(format!(
                        "{}: the rouchdb metadata is missing or unreadable ({}); the file \
                         was not modified",
                        path.display(),
                        e
                    )));
                }
            };
            if meta.schema > SCHEMA_VERSION {
                return Err(newer_error(path, meta.schema));
            }
            let missing_tables = CURRENT_TABLES.iter().any(|t| !names.iter().any(|n| n == t));
            Ok(Detected::Current {
                meta,
                missing_tables,
            })
        }
        Err(TableError::TableTypeMismatch { .. }) => match txn.open_table(LEGACY_META_TABLE) {
            Ok(table) => {
                let bytes = db_err!(table.get(META_KEY))?
                    .map(|g| g.value().to_vec())
                    .ok_or_else(|| {
                        RouchError::DatabaseError(format!(
                            "{}: not a rouchdb database (its metadata table has no \
                             metadata record); the file was not modified",
                            path.display()
                        ))
                    })?;
                let meta: MetaRecord = serde_json::from_slice(&bytes).map_err(|e| {
                    RouchError::DatabaseError(format!(
                        "{}: the metadata record cannot be decoded ({}); the file was not \
                         modified",
                        path.display(),
                        e
                    ))
                })?;
                if meta.schema > SCHEMA_VERSION {
                    return Err(newer_error(path, meta.schema));
                }
                Ok(Detected::Unguarded {
                    schema: meta.schema,
                })
            }
            // Neither ours nor legacy: a file written by a later format,
            // whose guard type name says which version it needs.
            Err(e) => Err(RouchError::DatabaseError(format!(
                "{}: written in an on-disk format this version of rouchdb does not know \
                 ({}); open it with a newer rouchdb (the file was not modified)",
                path.display(),
                e
            ))),
        },
        Err(TableError::TableDoesNotExist(_)) => {
            let mut all = names;
            all.extend(multimaps);
            Err(RouchError::DatabaseError(format!(
                "{}: not a rouchdb database (tables: {}); the file was not modified",
                path.display(),
                all.join(", ")
            )))
        }
        Err(e) => Err(RouchError::DatabaseError(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// Create every table of the current format (the guard included) that does
/// not exist yet.
fn create_current_tables(txn: &WriteTransaction) -> Result<()> {
    create_tables(txn)?;
    let mut guard = db_err!(txn.open_table(GUARD_TABLE))?;
    db_err!(guard.insert(GUARD_KEY, FormatGuard))?;
    Ok(())
}

/// Open (or create) the file at `path` as `open_with` does.
pub(crate) fn open_database(
    path: &Path,
    options: &OpenOptions,
) -> Result<(Database, Option<UpgradeReport>)> {
    let db = db_err!(Database::create(path))?;
    match detect(&db, path)? {
        Detected::Empty => {
            // The tables, the metadata and the guard are created together:
            // no committed state of the file lacks the guard.
            let txn = db_err!(db.begin_write())?;
            create_current_tables(&txn)?;
            write_meta(
                &mut db_err!(txn.open_table(META_TABLE))?,
                &MetaRecord::new(),
            )?;
            db_err!(txn.commit())?;
            Ok((db, None))
        }
        Detected::Current { missing_tables, .. } => {
            // Opening a current file writes nothing unless a table is
            // missing.
            if missing_tables {
                let txn = db_err!(db.begin_write())?;
                create_current_tables(&txn)?;
                db_err!(txn.commit())?;
            }
            Ok((db, None))
        }
        Detected::Unguarded { schema } => {
            let policy = match (&options.upgrade, schema) {
                (UpgradePolicy::Refuse, 0) => {
                    return Err(RouchError::UpgradeRequired {
                        path: path.to_path_buf(),
                    });
                }
                // Development builds of 0.5 already made these files
                // unreadable by 0.4: finish their upgrade.
                (UpgradePolicy::Refuse, _) => UpgradePolicy::InPlaceNoBackup,
                (policy, _) => policy.clone(),
            };
            let report = run_upgrade(&db, path, schema, &policy, false)?;
            Ok((db, Some(report)))
        }
    }
}

/// [`RedbAdapter::upgrade`](crate::RedbAdapter::upgrade) and
/// [`RedbAdapter::inspect_upgrade`](crate::RedbAdapter::inspect_upgrade)
/// (`dry_run`).
pub(crate) fn upgrade_file(
    path: &Path,
    policy: &UpgradePolicy,
    dry_run: bool,
) -> Result<UpgradeReport> {
    if !dry_run && *policy == UpgradePolicy::Refuse {
        return Err(RouchError::BadRequest(
            "UpgradePolicy::Refuse does not upgrade: use WithBackup or InPlaceNoBackup \
             (or inspect_upgrade for a dry run)"
                .into(),
        ));
    }
    if !path.exists() {
        return Err(RouchError::NotFound(format!(
            "database file {} does not exist",
            path.display()
        )));
    }
    let db = db_err!(Database::open(path))?;
    match detect(&db, path)? {
        Detected::Empty => Err(RouchError::DatabaseError(format!(
            "{}: the file holds no database",
            path.display()
        ))),
        Detected::Current {
            meta,
            missing_tables,
        } => {
            if missing_tables && !dry_run {
                let txn = db_err!(db.begin_write())?;
                create_current_tables(&txn)?;
                db_err!(txn.commit())?;
            }
            let mut report = UpgradeReport::new(path, StoredFormat::Current);
            report.doc_count = meta.doc_count;
            report.doc_del_count = meta.doc_del_count;
            Ok(report)
        }
        Detected::Unguarded { schema } => run_upgrade(&db, path, schema, policy, dry_run),
    }
}

/// Default backup location: `<file>.rouchdb-0.4.bak` (or
/// `<file>.rouchdb-0.5-pre.bak` for a development-build file, which 0.4
/// cannot open).
pub(crate) fn default_backup_path(path: &Path, schema: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(if schema == 0 {
        ".rouchdb-0.4.bak"
    } else {
        ".rouchdb-0.5-pre.bak"
    });
    PathBuf::from(name)
}

fn run_upgrade(
    db: &Database,
    path: &Path,
    schema: u32,
    policy: &UpgradePolicy,
    dry_run: bool,
) -> Result<UpgradeReport> {
    let from = if schema == 0 {
        StoredFormat::Legacy
    } else {
        StoredFormat::PreRelease { schema }
    };

    let backup = match policy {
        UpgradePolicy::WithBackup(dest) if !dry_run => {
            let dest = dest
                .clone()
                .unwrap_or_else(|| default_backup_path(path, schema));
            backup_logical(db, &dest)?;
            Some(dest)
        }
        _ => None,
    };

    let prepared = (|| -> Result<(WriteTransaction, UpgradeReport)> {
        let mut txn = db_err!(db.begin_write())?;
        // The primary commit slot must be valid even if the machine crashes
        // while this (large) commit is written.
        txn.set_two_phase_commit(true);
        let mut report = UpgradeReport::new(path, from);
        upgrade_contents(&txn, path, schema, &mut report)?;
        fault::hit("upgrade:before_commit")?;
        Ok((txn, report))
    })();

    let (txn, mut report) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            // Nothing was committed: the file is exactly as it was, so the
            // backup is redundant (and would block a retry).
            if let Some(dest) = &backup {
                let _ = fs::remove_file(dest);
            }
            return Err(e);
        }
    };
    if dry_run {
        db_err!(txn.abort())?;
        return Ok(report);
    }
    txn.commit().map_err(|e| {
        RouchError::DatabaseError(format!(
            "committing the upgrade of {} failed: {}{}",
            path.display(),
            e,
            match &backup {
                Some(dest) => format!("; the backup at {} was kept", dest.display()),
                None => String::new(),
            }
        ))
    })?;
    report.upgraded = true;
    report.backup = backup;
    Ok(report)
}

// ---------------------------------------------------------------------------
// The upgrade itself (one write transaction)
// ---------------------------------------------------------------------------

/// An error for a record the upgrade cannot decode. The upgrade stops
/// rather than skip data; nothing is committed.
fn corrupt(path: &Path, what: String, e: impl fmt::Display) -> RouchError {
    RouchError::DatabaseError(format!(
        "cannot upgrade {}: {} cannot be decoded ({}). Nothing was changed. Repair or \
         remove that record with the rouchdb version that wrote the file, then retry",
        path.display(),
        what,
        e
    ))
}

fn upgrade_contents(
    txn: &WriteTransaction,
    path: &Path,
    schema: u32,
    report: &mut UpgradeReport,
) -> Result<()> {
    // Every entry of the old metadata table: the metadata record, and the
    // security document of development builds.
    let mut entries = Vec::new();
    {
        let table = db_err!(txn.open_table(LEGACY_META_TABLE))?;
        for entry in db_err!(table.iter())? {
            let (key, value) = db_err!(entry)?;
            entries.push((key.value().to_string(), value.value().to_vec()));
        }
    }
    let meta_bytes = entries
        .iter()
        .find(|(k, _)| k == META_KEY)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?;
    let mut meta: MetaRecord = serde_json::from_slice(&meta_bytes)
        .map_err(|e| corrupt(path, "the metadata record".into(), e))?;

    create_tables(txn)?;
    if schema < 1 {
        report.attachments_rekeyed = migrate_attachments_to_digest_keys(txn)?;
    }
    fault::hit("upgrade:after_attachments")?;
    move_local_docs(txn, path, report)?;
    normalize_revs(txn, path, report)?;
    scan(txn, path, &mut meta, report)?;

    meta.schema = SCHEMA_VERSION;
    {
        let mut table = db_err!(txn.open_table(META_TABLE))?;
        for (key, value) in &entries {
            if key != META_KEY {
                db_err!(table.insert(key.as_str(), value.as_slice()))?;
            }
        }
        write_meta(&mut table, &meta)?;
    }
    db_err!(txn.delete_table(LEGACY_META_TABLE))?;
    create_current_tables(txn)?;
    Ok(())
}

/// Schema 0 -> 1: attachment bytes were stored under `doc_id\0name` (so a
/// later write of the same name overwrote the bytes older revisions point
/// at). Re-key every entry by its digest, which is what revision metadata
/// references. Returns the number of entries re-keyed.
fn migrate_attachments_to_digest_keys(txn: &WriteTransaction) -> Result<u64> {
    let mut table = db_err!(txn.open_table(ATTACHMENT_TABLE))?;
    let mut legacy_keys = Vec::new();
    for entry in db_err!(table.iter())? {
        let (key, _) = db_err!(entry)?;
        if key.value().contains('\0') {
            legacy_keys.push(key.value().to_string());
        }
    }
    let mut count = 0;
    for key in legacy_keys {
        let bytes = db_err!(table.remove(key.as_str()))?.map(|g| g.value().to_vec());
        if let Some(bytes) = bytes {
            let digest = attachment_digest(&bytes);
            let exists = db_err!(table.get(digest.as_str()))?.is_some();
            if !exists {
                db_err!(table.insert(digest.as_str(), bytes.as_slice()))?;
            }
            count += 1;
        }
    }
    Ok(count)
}

/// rouchdb <= 0.4 stored `_local/` documents written through `bulk_docs`
/// (`put("_local/x")`) as ordinary documents; 0.5 keeps them in the local
/// store, where it looks them up. Move each one there with its current
/// body, and remove its revision tree, bodies and change entry.
fn move_local_docs(txn: &WriteTransaction, path: &Path, report: &mut UpgradeReport) -> Result<()> {
    let mut docs = db_err!(txn.open_table(DOC_TABLE))?;
    let mut revs = db_err!(txn.open_table(REV_DATA_TABLE))?;
    let mut changes = db_err!(txn.open_table(CHANGES_TABLE))?;
    let mut locals = db_err!(txn.open_table(LOCAL_TABLE))?;

    // '0' follows '/': the range holds exactly the ids starting "_local/".
    let mut found = Vec::new();
    for entry in db_err!(docs.range("_local/".."_local0"))? {
        let (key, value) = db_err!(entry)?;
        found.push((key.value().to_string(), value.value().to_vec()));
    }

    for (id, record) in found {
        let local_id = &id["_local/".len()..];
        let (tree, seq) = decode_doc_record(&record)
            .map_err(|e| corrupt(path, format!("the record of document {:?}", id), e))?;
        let body_keys = rev_data_keys(&revs, &id)?;

        match winning_rev(&tree) {
            Some(winner) if !is_deleted(&tree) => {
                if db_err!(locals.get(local_id))?.is_some() {
                    return Err(RouchError::DatabaseError(format!(
                        "cannot upgrade {}: the document {:?} and the local document {:?} \
                         (written with put_local) are distinct in rouchdb 0.4 but the same \
                         document in 0.5. Nothing was changed. Remove one of them with \
                         rouchdb 0.4, then retry",
                        path.display(),
                        id,
                        local_id
                    )));
                }
                let key = rev_data_key(&id, &winner.to_string());
                let stored: Option<RevDataRecord> = match db_err!(revs.get(key.as_str()))? {
                    Some(guard) => Some(decode_body(guard.value()).map_err(|e| {
                        corrupt(path, format!("the body of {:?} revision {}", id, winner), e)
                    })?),
                    None => None,
                };
                // rouchdb 0.4 returned an empty body when none was stored.
                let (data, attachments) = match stored {
                    Some(rd) => (rd.data, rd.attachments.len() as u64),
                    None => (serde_json::Value::Object(Default::default()), 0),
                };
                let mut body = match data {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                body.remove("_id");
                body.insert(
                    "_rev".into(),
                    serde_json::Value::String(format!("0-{}", winner.pos)),
                );
                let bytes = serde_json::to_vec(&serde_json::Value::Object(body))?;
                db_err!(locals.insert(local_id, bytes.as_slice()))?;
                report.local_docs_moved += 1;
                report.local_attachments_dropped += attachments;
                report.local_conflicts_dropped += collect_conflicts(&tree).len() as u64;
            }
            _ => report.local_tombstones_dropped += 1,
        }

        for key in body_keys {
            db_err!(revs.remove(key.as_str()))?;
        }
        let owned_change = match db_err!(changes.get(seq))? {
            Some(guard) => serde_json::from_slice::<ChangeRecord>(guard.value())
                .is_ok_and(|change| change.doc_id == id),
            None => false,
        };
        if owned_change {
            db_err!(changes.remove(seq))?;
        }
        db_err!(docs.remove(id.as_str()))?;
    }
    Ok(())
}

/// Lower-case every 32-digit hexadecimal revision id in revision trees and
/// body keys (rouchdb <= 0.4 stored ids of replicated writes as given; 0.5
/// looks revisions up in canonical, lower-case form).
fn normalize_revs(txn: &WriteTransaction, path: &Path, report: &mut UpgradeReport) -> Result<()> {
    {
        let mut docs = db_err!(txn.open_table(DOC_TABLE))?;
        let mut rewrites = Vec::new();
        for entry in db_err!(docs.iter())? {
            let (key, value) = db_err!(entry)?;
            let id = key.value();
            let (mut tree, seq) = decode_doc_record(value.value())
                .map_err(|e| corrupt(path, format!("the record of document {:?}", id), e))?;
            let changed = normalize_tree(&mut tree);
            if changed == 0 {
                continue;
            }
            let mut seen = HashSet::new();
            let mut duplicate = None;
            traverse_rev_tree(&tree, |pos, node, _| {
                if !seen.insert((pos, node.hash.clone())) && duplicate.is_none() {
                    duplicate = Some(format!("{}-{}", pos, node.hash));
                }
            });
            if let Some(rev) = duplicate {
                return Err(RouchError::DatabaseError(format!(
                    "cannot upgrade {}: document {:?} has revision {} both in upper and \
                     lower case, which rouchdb 0.5 treats as the same revision. Nothing was \
                     changed. Purge one of them with rouchdb 0.4, then retry",
                    path.display(),
                    id,
                    rev
                )));
            }
            report.revs_normalized += changed;
            rewrites.push((id.to_string(), encode_doc_record(&tree, seq)?));
        }
        for (id, bytes) in rewrites {
            db_err!(docs.insert(id.as_str(), bytes.as_slice()))?;
        }
    }

    let mut revs = db_err!(txn.open_table(REV_DATA_TABLE))?;
    let mut renames = Vec::new();
    for entry in db_err!(revs.iter())? {
        let (key, _) = db_err!(entry)?;
        let key = key.value();
        // Revision strings hold no NUL: the last one ends the document id.
        if let Some((doc_id, rev)) = key.rsplit_once('\0')
            && let Some((pos, hash)) = rev.split_once('-')
            && let std::borrow::Cow::Owned(lower) = normalize_rev_hash(hash)
        {
            renames.push((
                key.to_string(),
                rev_data_key(doc_id, &format!("{pos}-{lower}")),
            ));
        }
    }
    for (old, new) in renames {
        if db_err!(revs.get(new.as_str()))?.is_some() {
            return Err(RouchError::DatabaseError(format!(
                "cannot upgrade {}: stored bodies {:?} and {:?} differ only in the case of \
                 their revision id. Nothing was changed. Purge one of them with rouchdb 0.4, \
                 then retry",
                path.display(),
                old,
                new
            )));
        }
        let bytes = db_err!(revs.remove(old.as_str()))?.map(|g| g.value().to_vec());
        if let Some(bytes) = bytes {
            db_err!(revs.insert(new.as_str(), bytes.as_slice()))?;
        }
    }
    Ok(())
}

/// Lower-case the revision ids of `tree` (iteratively: histories can be
/// long). Returns how many changed.
fn normalize_tree(tree: &mut RevTree) -> u64 {
    let mut changed = 0;
    let mut stack: Vec<&mut RevNode> = tree.iter_mut().map(|p| &mut p.tree).collect();
    while let Some(node) = stack.pop() {
        if let std::borrow::Cow::Owned(lower) = normalize_rev_hash(&node.hash) {
            node.hash = lower;
            changed += 1;
        }
        stack.extend(node.children.iter_mut());
    }
    changed
}

/// Count the documents (the counts 0.5 maintains in its metadata) and
/// collect the facts the report gives about attachments and old revisions.
fn scan(
    txn: &WriteTransaction,
    path: &Path,
    meta: &mut MetaRecord,
    report: &mut UpgradeReport,
) -> Result<()> {
    let docs = db_err!(txn.open_table(DOC_TABLE))?;
    let revs = db_err!(txn.open_table(REV_DATA_TABLE))?;
    let atts = db_err!(txn.open_table(ATTACHMENT_TABLE))?;

    meta.doc_count = 0;
    meta.doc_del_count = 0;
    let mut leaf_digests: HashSet<String> = HashSet::new();
    let mut old_only_candidates: Vec<(String, HashSet<String>)> = Vec::new();

    for entry in db_err!(docs.iter())? {
        let (key, value) = db_err!(entry)?;
        let id = key.value();
        let (tree, _) = decode_doc_record(value.value())
            .map_err(|e| corrupt(path, format!("the record of document {:?}", id), e))?;
        meta.adjust_counts(None, Some(is_deleted(&tree)));

        let leaves: HashSet<String> = collect_leaves(&tree)
            .iter()
            .map(|l| l.rev_string())
            .collect();
        let mut old_digests = HashSet::new();
        let mut missing = false;
        for body_key in rev_data_keys(&revs, id)? {
            let rev = &body_key[id.len() + 1..];
            let guard = db_err!(revs.get(body_key.as_str()))?
                .ok_or_else(|| RouchError::DatabaseError("body vanished".into()))?;
            let record: RevAttachmentsRecord = serde_json::from_slice(guard.value())
                .map_err(|e| corrupt(path, format!("the body of {:?} revision {}", id, rev), e))?;
            let is_leaf = leaves.contains(rev);
            if !is_leaf {
                report.old_revision_bodies += 1;
            }
            for att in record.attachments.into_values() {
                if db_err!(atts.get(att.digest.as_str()))?.is_none() {
                    report.missing_attachment_refs += 1;
                    missing = true;
                } else if is_leaf {
                    leaf_digests.insert(att.digest);
                } else {
                    old_digests.insert(att.digest);
                }
            }
        }
        if missing {
            report.docs_with_missing_attachments_count += 1;
            if report.docs_with_missing_attachments.len() < REPORT_SAMPLE {
                report.docs_with_missing_attachments.push(id.to_string());
            }
        }
        if !old_digests.is_empty() {
            old_only_candidates.push((id.to_string(), old_digests));
        }
    }

    // Compaction keeps the bytes any leaf of any document references.
    for (id, digests) in old_only_candidates {
        if digests.iter().any(|d| !leaf_digests.contains(d)) {
            report.docs_with_old_only_attachments_count += 1;
            if report.docs_with_old_only_attachments.len() < REPORT_SAMPLE {
                report.docs_with_old_only_attachments.push(id);
            }
        }
    }
    report.doc_count = meta.doc_count;
    report.doc_del_count = meta.doc_del_count;
    Ok(())
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// Tables a rouchdb file older than the current format can hold, with the
/// key type of each (`true`: `u64` keys, else `&str`). All values are bytes.
const BACKUP_TABLES: [(&str, bool); 6] = [
    ("docs", false),
    ("rev_data", false),
    ("changes", true),
    ("local_docs", false),
    ("attachments", false),
    ("metadata", false),
];

/// Write a verified logical copy of the (unguarded) database `db` to
/// `dest`: every table is copied entry by entry from one read transaction
/// (the file lock is held throughout, so nothing changes meanwhile) into
/// `<dest>.partial`, committed, reopened and compared entry by entry with
/// the source, synced, then renamed to `dest`. On any error the partial copy
/// is removed; the source is only ever read.
fn backup_logical(db: &Database, dest: &Path) -> Result<()> {
    if dest.exists() {
        return Err(RouchError::DatabaseError(format!(
            "backup destination {} already exists; move it away or choose another backup \
             path (nothing was changed)",
            dest.display()
        )));
    }
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    // Exclusive creation: never overwrite anything, including a partial copy
    // left by an interrupted attempt.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .map_err(|e| {
            RouchError::DatabaseError(if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "{} exists, left by an interrupted backup; delete it and retry (nothing \
                     was changed)",
                    partial.display()
                )
            } else {
                format!(
                    "cannot create the backup file {}: {} (nothing was changed)",
                    partial.display(),
                    e
                )
            })
        })?;

    let result = copy_and_verify(db, &partial).and_then(|()| {
        fault::hit("backup:before_rename")?;
        fs::File::open(&partial)?.sync_all()?;
        if dest.exists() {
            return Err(RouchError::DatabaseError(format!(
                "backup destination {} appeared during the backup",
                dest.display()
            )));
        }
        fs::rename(&partial, dest)?;
        sync_parent_dir(dest)?;
        Ok(())
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(RouchError::DatabaseError(format!(
            "backup to {} failed: {} (nothing was changed)",
            dest.display(),
            e
        )));
    }
    Ok(())
}

fn copy_and_verify(db: &Database, partial: &Path) -> Result<()> {
    let source = db_err!(db.begin_read())?;
    let multimaps: Vec<String> = db_err!(source.list_multimap_tables())?
        .map(|t| t.name().to_string())
        .collect();
    let mut names: Vec<String> = db_err!(source.list_tables())?
        .map(|t| t.name().to_string())
        .collect();
    names.extend(multimaps.iter().cloned());
    let mut tables = Vec::new();
    for name in &names {
        match BACKUP_TABLES.iter().find(|(known, _)| known == name) {
            Some(&(known, u64_keys)) if !multimaps.contains(name) => tables.push((known, u64_keys)),
            _ => {
                return Err(RouchError::DatabaseError(format!(
                    "the file holds a table rouchdb did not create ({:?}), which the backup \
                     cannot copy. Copy the file yourself while no program has it open, then \
                     upgrade without a backup",
                    name
                )));
            }
        }
    }

    {
        let backup = db_err!(Database::create(partial))?;
        let mut txn = db_err!(backup.begin_write())?;
        txn.set_two_phase_commit(true);
        for &(name, u64_keys) in &tables {
            if u64_keys {
                copy_table(&source, &txn, TableDefinition::<u64, &[u8]>::new(name))?;
            } else {
                copy_table(&source, &txn, TableDefinition::<&str, &[u8]>::new(name))?;
            }
        }
        db_err!(txn.commit())?;
    }

    fault::hit("backup:verify")?;
    let backup = db_err!(Database::open(partial))?;
    let copy = db_err!(backup.begin_read())?;
    let mut copied: Vec<String> = db_err!(copy.list_tables())?
        .map(|t| t.name().to_string())
        .collect();
    copied.sort();
    let mut expected: Vec<String> = tables.iter().map(|(n, _)| n.to_string()).collect();
    expected.sort();
    if copied != expected || db_err!(copy.list_multimap_tables())?.next().is_some() {
        return Err(RouchError::DatabaseError(format!(
            "verification failed: the copy has tables {:?}, expected {:?}",
            copied, expected
        )));
    }
    for &(name, u64_keys) in &tables {
        if u64_keys {
            verify_table(&source, &copy, TableDefinition::<u64, &[u8]>::new(name))?;
        } else {
            verify_table(&source, &copy, TableDefinition::<&str, &[u8]>::new(name))?;
        }
    }
    Ok(())
}

fn copy_table<K: redb::Key + 'static>(
    source: &ReadTransaction,
    dest: &WriteTransaction,
    def: TableDefinition<K, &[u8]>,
) -> Result<()> {
    let from = db_err!(source.open_table(def))?;
    let mut to = db_err!(dest.open_table(def))?;
    for entry in db_err!(from.iter())? {
        let (key, value) = db_err!(entry)?;
        db_err!(to.insert(key.value(), value.value()))?;
    }
    Ok(())
}

/// Compare two tables entry by entry, key and value bytes.
fn verify_table<K: redb::Key + 'static>(
    source: &ReadTransaction,
    copy: &ReadTransaction,
    def: TableDefinition<K, &[u8]>,
) -> Result<()> {
    use redb::ReadableTableMetadata;
    let a = db_err!(source.open_table(def))?;
    let b = db_err!(copy.open_table(def))?;
    let mismatch = || {
        RouchError::DatabaseError(format!(
            "verification failed: table {} differs from the source",
            def.name()
        ))
    };
    if db_err!(a.len())? != db_err!(b.len())? {
        return Err(mismatch());
    }
    let mut left = db_err!(a.iter())?;
    let mut right = db_err!(b.iter())?;
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ok(()),
            (Some(x), Some(y)) => {
                let (xk, xv) = db_err!(x)?;
                let (yk, yv) = db_err!(y)?;
                if K::as_bytes(&xk.value()).as_ref() != K::as_bytes(&yk.value()).as_ref()
                    || xv.value() != yv.value()
                {
                    return Err(mismatch());
                }
            }
            _ => return Err(mismatch()),
        }
    }
}

/// Make a rename durable (a no-op where directories cannot be opened).
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// ---------------------------------------------------------------------------
// Fault injection (tests only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fault {
    use std::cell::RefCell;

    use rouchdb_core::error::{Result, RouchError};

    thread_local! {
        static POINT: RefCell<Option<&'static str>> = const { RefCell::new(None) };
    }

    /// Make the next pass through `point` (on this thread) fail.
    pub(crate) fn set(point: Option<&'static str>) {
        POINT.with(|p| *p.borrow_mut() = point);
    }

    pub(crate) fn hit(point: &str) -> Result<()> {
        if POINT.with(|p| *p.borrow() == Some(point)) {
            return Err(RouchError::DatabaseError(format!(
                "injected failure at {}",
                point
            )));
        }
        Ok(())
    }
}

#[cfg(not(test))]
mod fault {
    use rouchdb_core::error::Result;

    #[inline(always)]
    pub(crate) fn hit(_point: &str) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use rouchdb_core::document::{
        AllDocsOptions, ChangesOptions, GetAttachmentOptions, GetOptions,
    };
    use rouchdb_core::rev_tree::build_path_from_revs;

    /// Every table of an unguarded file: name -> (key bytes, value bytes).
    type Snapshot = BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>;

    fn dump<K: redb::Key + 'static>(
        txn: &ReadTransaction,
        def: TableDefinition<K, &[u8]>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = txn.open_table(def).unwrap();
        table
            .iter()
            .unwrap()
            .map(|e| {
                let (k, v) = e.unwrap();
                (
                    K::as_bytes(&k.value()).as_ref().to_vec(),
                    v.value().to_vec(),
                )
            })
            .collect()
    }

    /// The logical contents of a file without the format guard (a 0.4 file
    /// or a backup), compared instead of bytes: redb rewrites its header on
    /// open.
    fn snapshot(path: &Path) -> Snapshot {
        let db = Database::open(path).unwrap();
        let txn = db.begin_read().unwrap();
        let names: Vec<String> = txn
            .list_tables()
            .unwrap()
            .map(|t| t.name().to_string())
            .collect();
        names
            .into_iter()
            .map(|name| {
                let rows = if name == "changes" {
                    dump(&txn, TableDefinition::<u64, &[u8]>::new(&name))
                } else {
                    dump(&txn, TableDefinition::<&str, &[u8]>::new(&name))
                };
                (name, rows)
            })
            .collect()
    }

    fn legacy_record(tree: &RevTree, seq: u64) -> Vec<u8> {
        serde_json::to_vec(&LegacyDocRecord {
            rev_tree: legacy_tree_to_serialized(tree),
            seq,
        })
        .unwrap()
    }

    fn hex(i: u64) -> String {
        format!("{:032x}", i)
    }

    fn body(data: serde_json::Value, atts: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(
            &serde_json::json!({"data": data, "deleted": false, "attachments": atts}),
        )
        .unwrap()
    }

    fn att(bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({"content_type": "text/plain", "digest": attachment_digest(bytes), "length": bytes.len()})
    }

    /// Write a file laid out exactly as rouchdb <= 0.4 does (its six tables,
    /// `metadata` as `Table<&str, &[u8]>`, a metadata record without schema),
    /// then let `fill` add records.
    fn legacy_file(path: &Path, fill: impl FnOnce(&WriteTransaction)) {
        let db = Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(DOC_TABLE).unwrap();
        txn.open_table(REV_DATA_TABLE).unwrap();
        txn.open_table(CHANGES_TABLE).unwrap();
        txn.open_table(LOCAL_TABLE).unwrap();
        txn.open_table(ATTACHMENT_TABLE).unwrap();
        txn.open_table(LEGACY_META_TABLE)
            .unwrap()
            .insert(
                META_KEY,
                &br#"{"update_seq":9,"db_uuid":"legacy-uuid"}"#[..],
            )
            .unwrap();
        fill(&txn);
        txn.commit().unwrap();
    }

    fn put(txn: &WriteTransaction, table: TableDefinition<&str, &[u8]>, key: &str, value: &[u8]) {
        txn.open_table(table).unwrap().insert(key, value).unwrap();
    }

    fn change(txn: &WriteTransaction, seq: u64, id: &str) {
        let bytes =
            serde_json::to_vec(&serde_json::json!({"doc_id": id, "deleted": false})).unwrap();
        txn.open_table(CHANGES_TABLE)
            .unwrap()
            .insert(seq, bytes.as_slice())
            .unwrap();
    }

    /// A small but varied 0.4 database:
    /// - `a`: 3 revisions, only the leaf has a body.
    /// - `d`: attachment `f` re-attached (rev 2 -> bytes overwritten by rev 3):
    ///   rev 2's bytes are lost, as in 0.4.
    /// - `e`: attachment only on the old rev 2 (0.4 dropped it on the body
    ///   update of rev 3).
    /// - `_local/cfg`: a local doc written through bulk_docs (2 revisions).
    /// - `_local/gone`: deleted.
    /// - local store: `ck` (a replication checkpoint).
    fn sample_legacy(path: &Path) {
        legacy_file(path, |txn| {
            let tree = |len| {
                vec![build_path_from_revs(
                    len,
                    &(1..=len).rev().map(hex).collect::<Vec<_>>(),
                    NodeOpts::default(),
                    RevStatus::Available,
                )]
            };
            put(txn, DOC_TABLE, "a", &legacy_record(&tree(3), 1));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("a", &format!("3-{}", hex(3))),
                &body(serde_json::json!({"v": 3}), serde_json::json!({})),
            );
            change(txn, 1, "a");

            put(txn, DOC_TABLE, "d", &legacy_record(&tree(3), 2));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("d", &format!("2-{}", hex(2))),
                &body(
                    serde_json::json!({}),
                    serde_json::json!({"f": att(b"old bytes")}),
                ),
            );
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("d", &format!("3-{}", hex(3))),
                &body(
                    serde_json::json!({}),
                    serde_json::json!({"f": att(b"new bytes")}),
                ),
            );
            put(txn, ATTACHMENT_TABLE, "d\0f", b"new bytes");
            change(txn, 2, "d");

            put(txn, DOC_TABLE, "e", &legacy_record(&tree(3), 3));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("e", &format!("2-{}", hex(2))),
                &body(
                    serde_json::json!({"v": 2}),
                    serde_json::json!({"g": att(b"only old")}),
                ),
            );
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("e", &format!("3-{}", hex(3))),
                &body(serde_json::json!({"v": 3}), serde_json::json!({})),
            );
            put(txn, ATTACHMENT_TABLE, "e\0g", b"only old");
            change(txn, 3, "e");

            put(txn, DOC_TABLE, "_local/cfg", &legacy_record(&tree(2), 4));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("_local/cfg", &format!("1-{}", hex(1))),
                &body(serde_json::json!({"theme": "light"}), serde_json::json!({})),
            );
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("_local/cfg", &format!("2-{}", hex(2))),
                &body(serde_json::json!({"theme": "dark"}), serde_json::json!({})),
            );
            change(txn, 4, "_local/cfg");

            let mut deleted = tree(1);
            deleted[0].tree.opts.deleted = true;
            put(txn, DOC_TABLE, "_local/gone", &legacy_record(&deleted, 5));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("_local/gone", &format!("1-{}", hex(1))),
                &serde_json::to_vec(&serde_json::json!({"data": {}, "deleted": true})).unwrap(),
            );
            change(txn, 5, "_local/gone");

            put(txn, LOCAL_TABLE, "ck", br#"{"last_seq":"42"}"#);
        });
    }

    fn with_backup(dest: Option<PathBuf>) -> OpenOptions {
        OpenOptions::new().upgrade(UpgradePolicy::WithBackup(dest))
    }

    fn assert_guarded(path: &Path) {
        let db = Database::open(path).unwrap();
        let txn = db.begin_read().unwrap();
        assert!(txn.open_table(GUARD_TABLE).is_ok());
        // What rouchdb <= 0.4 does first: open `metadata` as bytes. It fails,
        // naming the version the file needs.
        let err = txn
            .open_table(LEGACY_META_TABLE)
            .expect_err("guarded")
            .to_string();
        assert!(err.contains("this file requires rouchdb >= 0.5"), "{err}");
        let meta = read_meta(&txn.open_table(META_TABLE).unwrap()).unwrap();
        assert_eq!(meta.schema, SCHEMA_VERSION);
    }

    #[test]
    fn guard_type_name_never_changes() {
        assert_eq!(
            <FormatGuard as redb::Value>::type_name(),
            redb::TypeName::new("rouchdb-format-2 (this file requires rouchdb >= 0.5)")
        );
    }

    #[tokio::test]
    async fn fresh_file_has_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.redb");
        let db = RedbAdapter::open(&path, "new").unwrap();
        assert!(db.upgrade_report().is_none());
        drop(db);
        assert_guarded(&path);
        // Opening a current file writes nothing: not even a transaction.
        let bytes = std::fs::read(&path).unwrap();
        let db = RedbAdapter::open(&path, "new").unwrap();
        assert_eq!(db.info().await.unwrap().doc_count, 0);
        drop(db);
        assert!(
            std::fs::read(&path).unwrap() == bytes,
            "a current open modified the file"
        );
    }

    #[tokio::test]
    async fn legacy_file_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);
        let raw = std::fs::read(&path).unwrap();

        let err = RedbAdapter::open(&path, "old").err().expect("refused");
        assert!(
            matches!(err, RouchError::UpgradeRequired { ref path } if path.ends_with("old.redb")),
            "{err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("rouchdb migrate"), "{msg}");
        assert!(msg.contains("not modified"), "{msg}");
        assert!(matches!(
            RedbAdapter::open_with(&path, "old", OpenOptions::new()),
            Err(RouchError::UpgradeRequired { .. })
        ));
        assert_eq!(snapshot(&path), before);
        // Not even redb's header changed.
        assert!(
            std::fs::read(&path).unwrap() == raw,
            "refusing modified the file"
        );
        assert!(!default_backup_path(&path, 0).exists());
    }

    #[tokio::test]
    async fn upgrade_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);

        let db = RedbAdapter::open_with(&path, "old", with_backup(None)).unwrap();
        let backup = default_backup_path(&path, 0);
        assert!(backup.ends_with("old.redb.rouchdb-0.4.bak"));
        let report = db.upgrade_report().unwrap().clone();
        assert_eq!(report.from, StoredFormat::Legacy);
        assert!(report.upgraded);
        assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
        assert_eq!((report.doc_count, report.doc_del_count), (3, 0));
        assert_eq!(report.attachments_rekeyed, 2);
        assert_eq!(report.missing_attachment_refs, 1);
        assert_eq!(report.docs_with_missing_attachments, ["d"]);
        assert_eq!(report.local_docs_moved, 1);
        assert_eq!(report.local_tombstones_dropped, 1);
        assert_eq!(report.revs_normalized, 0);
        // d rev 2, e rev 2 (a's old revisions have no body).
        assert_eq!(report.old_revision_bodies, 2);
        assert_eq!(report.docs_with_old_only_attachments, ["e"]);
        let text = report.to_string();
        assert!(text.contains("first compact()"), "{text}");

        // The backup is the file as it was, and no partial copy is left.
        assert_eq!(snapshot(&backup), before);
        let mut partial = backup.as_os_str().to_owned();
        partial.push(".partial");
        assert!(!Path::new(&partial).exists());

        // The upgraded file reads correctly.
        let a = db.get("a", GetOptions::default()).await.unwrap();
        assert_eq!(a.rev.unwrap().to_string(), format!("3-{}", hex(3)));
        assert_eq!(a.data["v"], 3);
        let bytes = db
            .get_attachment("d", "f", GetAttachmentOptions::default())
            .await
            .unwrap();
        assert_eq!(bytes, b"new bytes");
        let old = db
            .get_attachment(
                "e",
                "g",
                GetAttachmentOptions {
                    rev: Some(format!("2-{}", hex(2))),
                },
            )
            .await
            .unwrap();
        assert_eq!(old, b"only old");
        let info = db.info().await.unwrap();
        assert_eq!((info.doc_count, info.update_seq), (3, Seq::Num(9)));
        let local = db.get("_local/cfg", GetOptions::default()).await.unwrap();
        assert_eq!(local.data["theme"], "dark");
        assert_eq!(local.rev.unwrap().to_string(), "0-2");
        assert_eq!(db.get_local("ck").await.unwrap()["last_seq"], "42");
        drop(db);
        assert_guarded(&path);

        // Upgrading again is a no-op; the file is current.
        let again = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
        assert_eq!(again.from, StoredFormat::Current);
        assert!(!again.upgraded);
    }

    #[tokio::test]
    async fn existing_backup_or_unwritable_destination_refuses_the_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);

        let taken = dir.path().join("taken.bak");
        std::fs::write(&taken, b"someone's file").unwrap();
        let err = RedbAdapter::open_with(&path, "old", with_backup(Some(taken.clone())))
            .err()
            .expect("refused");
        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(std::fs::read(&taken).unwrap(), b"someone's file");

        // A partial copy left by an interrupted backup is never overwritten.
        let interrupted = dir.path().join("i.bak");
        std::fs::write(dir.path().join("i.bak.partial"), b"half").unwrap();
        let err = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(Some(interrupted.clone())))
            .expect_err("refused");
        assert!(err.to_string().contains("interrupted backup"), "{err}");
        assert_eq!(
            std::fs::read(dir.path().join("i.bak.partial")).unwrap(),
            b"half"
        );
        assert!(!interrupted.exists());

        let nowhere = dir.path().join("missing-dir").join("x.bak");
        let err = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(Some(nowhere.clone())))
            .expect_err("refused");
        assert!(
            err.to_string().contains("cannot create the backup file"),
            "{err}"
        );
        assert!(!nowhere.exists());

        assert_eq!(snapshot(&path), before);
        assert!(matches!(
            RedbAdapter::open(&path, "old"),
            Err(RouchError::UpgradeRequired { .. })
        ));
    }

    #[tokio::test]
    async fn failures_leave_the_file_legacy_and_no_backup_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);
        let backup = default_backup_path(&path, 0);
        let mut partial = backup.as_os_str().to_owned();
        partial.push(".partial");

        for point in [
            "backup:verify",
            "backup:before_rename",
            "upgrade:after_attachments",
            "upgrade:before_commit",
        ] {
            fault::set(Some(point));
            let result = RedbAdapter::open_with(&path, "old", with_backup(None));
            fault::set(None);
            let err = result.err().unwrap_or_else(|| panic!("{point}: must fail"));
            assert!(
                err.to_string().contains("injected failure"),
                "{point}: {err}"
            );
            assert_eq!(snapshot(&path), before, "{point}");
            assert!(!backup.exists(), "{point}: backup left behind");
            assert!(
                !Path::new(&partial).exists(),
                "{point}: partial left behind"
            );
        }

        // Without a backup as well.
        fault::set(Some("upgrade:before_commit"));
        let result = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup);
        fault::set(None);
        assert!(result.is_err());
        assert_eq!(snapshot(&path), before);

        // And the upgrade still works afterwards.
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
        assert!(report.upgraded);
        assert_eq!(snapshot(&backup), before);
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);
        let report = RedbAdapter::inspect_upgrade(&path).unwrap();
        assert!(!report.upgraded);
        assert_eq!(report.local_docs_moved, 1);
        assert_eq!(report.doc_count, 3);
        assert!(report.to_string().contains("dry run"));
        assert_eq!(snapshot(&path), before);
        assert!(!default_backup_path(&path, 0).exists());
        // Refuse is not an upgrade policy for upgrade().
        assert!(matches!(
            RedbAdapter::upgrade(&path, UpgradePolicy::Refuse),
            Err(RouchError::BadRequest(_))
        ));
        assert!(matches!(
            RedbAdapter::upgrade(dir.path().join("nope.redb"), UpgradePolicy::InPlaceNoBackup),
            Err(RouchError::NotFound(_))
        ));
        assert!(!dir.path().join("nope.redb").exists());
    }

    #[tokio::test]
    async fn local_docs_move_out_of_the_document_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let db = RedbAdapter::open_with(
            &path,
            "old",
            OpenOptions::new().upgrade(UpgradePolicy::InPlaceNoBackup),
        )
        .unwrap();

        let ids = |rows: Vec<String>| {
            rows.into_iter()
                .filter(|id| id.starts_with("_local/"))
                .count()
        };
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(ids(all.rows.into_iter().filter_map(|r| r.id).collect()), 0);
        assert_eq!(all.total_rows, 3);
        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert_eq!(ids(changes.results.into_iter().map(|r| r.id).collect()), 0);
        assert!(matches!(
            db.get("_local/gone", GetOptions::default()).await,
            Err(RouchError::NotFound(_))
        ));
        // No revision data of the moved documents is left behind.
        {
            let txn = db.inner.db.begin_read().unwrap();
            let revs = txn.open_table(REV_DATA_TABLE).unwrap();
            assert!(rev_data_keys(&revs, "_local/cfg").unwrap().is_empty());
            assert!(rev_data_keys(&revs, "_local/gone").unwrap().is_empty());
        }

        // The moved document is updated like any 0.5 local document.
        let doc = Document {
            id: "_local/cfg".into(),
            rev: Some("0-2".parse().unwrap()),
            deleted: false,
            data: serde_json::json!({"theme": "blue"}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert_eq!(r[0].rev.as_deref(), Some("0-3"), "{:?}", r[0]);
        assert_eq!(db.get_local("cfg").await.unwrap()["theme"], "blue");
    }

    #[tokio::test]
    async fn local_id_collision_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        {
            let db = Database::open(&path).unwrap();
            let txn = db.begin_write().unwrap();
            put(&txn, LOCAL_TABLE, "cfg", br#"{"other":true}"#);
            txn.commit().unwrap();
        }
        let before = snapshot(&path);
        let err =
            RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).expect_err("refused");
        let msg = err.to_string();
        assert!(
            msg.contains("\"_local/cfg\"") && msg.contains("Nothing was changed"),
            "{msg}"
        );
        assert_eq!(snapshot(&path), before);
        assert!(!default_backup_path(&path, 0).exists());
    }

    #[tokio::test]
    async fn upper_case_revisions_are_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        let upper = "ABCDEF0123456789ABCDEF0123456789";
        legacy_file(&path, |txn| {
            let tree = vec![build_path_from_revs(
                2,
                &[upper.to_string(), hex(1)],
                NodeOpts::default(),
                RevStatus::Available,
            )];
            put(txn, DOC_TABLE, "u", &legacy_record(&tree, 1));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("u", &format!("2-{upper}")),
                &body(serde_json::json!({"up": true}), serde_json::json!({})),
            );
            change(txn, 1, "u");
        });
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).unwrap();
        assert_eq!(report.revs_normalized, 1);
        let db = RedbAdapter::open(&path, "old").unwrap();
        let doc = db.get("u", GetOptions::default()).await.unwrap();
        let lower = upper.to_ascii_lowercase();
        assert_eq!(doc.rev.unwrap().to_string(), format!("2-{lower}"));
        assert_eq!(doc.data["up"], true);
        // Replicating the same revision again is recognized as known.
        let diff = db
            .revs_diff(HashMap::from([(
                "u".to_string(),
                vec![format!("2-{upper}")],
            )]))
            .await
            .unwrap();
        assert!(diff.results.is_empty(), "{:?}", diff.results);
    }

    #[tokio::test]
    async fn case_duplicates_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        let upper = "ABCDEF0123456789ABCDEF0123456789";
        legacy_file(&path, |txn| {
            let mut tree = vec![build_path_from_revs(
                1,
                &[upper.to_string()],
                NodeOpts::default(),
                RevStatus::Available,
            )];
            tree.push(build_path_from_revs(
                1,
                &[upper.to_ascii_lowercase()],
                NodeOpts::default(),
                RevStatus::Available,
            ));
            put(txn, DOC_TABLE, "dup", &legacy_record(&tree, 1));
        });
        let before = snapshot(&path);
        let err = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).expect_err("refused");
        assert!(err.to_string().contains("\"dup\""), "{err}");
        assert_eq!(snapshot(&path), before);
    }

    #[tokio::test]
    async fn corrupt_record_names_the_document_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        {
            let db = Database::open(&path).unwrap();
            let txn = db.begin_write().unwrap();
            put(&txn, DOC_TABLE, "zzz", b"{garbage");
            txn.commit().unwrap();
        }
        let before = snapshot(&path);
        let err = RedbAdapter::open_with(&path, "old", with_backup(None))
            .err()
            .expect("refused");
        let msg = err.to_string();
        assert!(
            msg.contains("\"zzz\"") && msg.contains("Nothing was changed"),
            "{msg}"
        );
        assert_eq!(snapshot(&path), before);
        assert!(!default_backup_path(&path, 0).exists());
    }

    /// Files of unreleased 0.5 builds (schema 1 or 2 in the unguarded
    /// `metadata` table, maybe with a security document) are upgraded on a
    /// plain open, keeping the security document.
    #[tokio::test]
    async fn pre_release_files_are_relocated_with_their_security() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.redb");
        {
            let db = RedbAdapter::open(&path, "dev").unwrap();
            let doc = Document {
                id: "x".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"v": 1}),
                attachments: HashMap::new(),
            };
            db.bulk_docs(vec![doc], BulkDocsOptions::new())
                .await
                .unwrap();
            db.put_security(SecurityDocument {
                admins: rouchdb_core::document::SecurityGroup {
                    names: vec!["alice".into()],
                    roles: vec![],
                },
                ..Default::default()
            })
            .await
            .unwrap();
            // Back to the development layout: everything in `metadata`.
            let txn = db.inner.db.begin_write().unwrap();
            let entries: Vec<(String, Vec<u8>)> = {
                let meta = txn.open_table(META_TABLE).unwrap();
                meta.iter()
                    .unwrap()
                    .map(|e| {
                        let (k, v) = e.unwrap();
                        (k.value().to_string(), v.value().to_vec())
                    })
                    .collect()
            };
            txn.delete_table(GUARD_TABLE).unwrap();
            txn.delete_table(META_TABLE).unwrap();
            {
                let mut legacy = txn.open_table(LEGACY_META_TABLE).unwrap();
                for (k, v) in &entries {
                    legacy.insert(k.as_str(), v.as_slice()).unwrap();
                }
            }
            txn.commit().unwrap();
        }
        let db = RedbAdapter::open(&path, "dev").unwrap();
        let report = db.upgrade_report().unwrap();
        assert_eq!(report.from, StoredFormat::PreRelease { schema: 2 });
        assert!(report.upgraded && report.backup.is_none());
        assert_eq!(db.get_security().await.unwrap().admins.names, ["alice"]);
        assert_eq!(db.info().await.unwrap().doc_count, 1);
        drop(db);
        assert_guarded(&path);
    }

    #[tokio::test]
    async fn newer_legacy_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("odd.redb");
        legacy_file(&path, |txn| {
            let meta =
                serde_json::json!({"update_seq": 1, "db_uuid": "u", "schema": SCHEMA_VERSION + 1});
            put(
                txn,
                LEGACY_META_TABLE,
                META_KEY,
                &serde_json::to_vec(&meta).unwrap(),
            );
        });
        let before = snapshot(&path);
        let err = RedbAdapter::open_with(&path, "odd", with_backup(None))
            .err()
            .expect("refused");
        assert!(err.to_string().contains("newer"), "{err}");
        assert_eq!(snapshot(&path), before);
    }

    #[tokio::test]
    async fn foreign_redb_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.redb");
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            put(&txn, TableDefinition::new("mine"), "k", b"v");
            txn.commit().unwrap();
        }
        let err = RedbAdapter::open(&path, "other").err().expect("refused");
        assert!(err.to_string().contains("not a rouchdb database"), "{err}");
        let before = snapshot(&path);
        assert_eq!(before.keys().collect::<Vec<_>>(), ["mine"]);
    }

    /// A later format replaces the guard with another type: its name is
    /// reported, and the file is not touched.
    #[tokio::test]
    async fn unknown_future_guard_is_reported() {
        #[derive(Debug)]
        struct FutureGuard;
        impl redb::Value for FutureGuard {
            type SelfType<'a>
                = FutureGuard
            where
                Self: 'a;
            type AsBytes<'a>
                = [u8; 0]
            where
                Self: 'a;
            fn fixed_width() -> Option<usize> {
                Some(0)
            }
            fn from_bytes<'a>(_: &'a [u8]) -> FutureGuard
            where
                Self: 'a,
            {
                FutureGuard
            }
            fn as_bytes<'a, 'b: 'a>(_: &'a FutureGuard) -> [u8; 0]
            where
                Self: 'b,
            {
                []
            }
            fn type_name() -> redb::TypeName {
                redb::TypeName::new("rouchdb-format-3 (this file requires rouchdb >= 9.9)")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.redb");
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(TableDefinition::<&str, FutureGuard>::new("metadata"))
                .unwrap();
            txn.commit().unwrap();
        }
        let err = RedbAdapter::open(&path, "future").err().expect("refused");
        assert!(err.to_string().contains("requires rouchdb >= 9.9"), "{err}");
    }
}
