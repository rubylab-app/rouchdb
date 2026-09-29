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
//!
//! # How an upgrade runs
//!
//! 1. Everything the upgrade will do is worked out from a read transaction
//!    (`analyze`): the report, and a plan of the writes. A file the upgrade
//!    would refuse is refused here, before any backup is written. A dry run
//!    stops here, so it never writes to the file.
//! 2. The backup, if any, is written and verified (`backup_logical`).
//! 3. The plan is applied in one write transaction with two-phase commit
//!    (`apply`): the file is upgraded completely or not at all.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use redb::{
    MultimapTableHandle, ReadOnlyTable, ReadTransaction, TableError, TableHandle, WriteTransaction,
};
use rouchdb_core::merge::merge_tree;
use rouchdb_core::rev_tree::root_to_leaf;

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

/// redb's page cache for the databases the upgrade and the backup open.
/// redb's default (1 GiB) would let the upgrade of a large file hold about
/// that much memory: every attachment is read once to be re-keyed and once
/// to be copied to the backup. The upgrade reads each record once or twice,
/// so a large cache buys nothing.
const UPGRADE_CACHE_BYTES: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// What to do when a file written by an older rouchdb is opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpgradePolicy {
    /// Refuse files written by rouchdb <= 0.4 with
    /// [`RouchError::UpgradeRequired`], without modifying them (the
    /// default). Files written by unreleased 0.5 development builds, which
    /// rouchdb 0.4 cannot open anyway, are upgraded as with
    /// `WithBackup(None)`.
    #[default]
    Refuse,
    /// Upgrade after writing a verified backup of the file: to the given
    /// path, or next to the file, to `<file>.rouchdb-0.4.bak` (a complete
    /// copy that rouchdb 0.4 can still open) or, for a file written by a 0.5
    /// development build, to `<file>.rouchdb-0.5-pre.bak` (a copy of that
    /// file, which rouchdb 0.4 cannot open). The upgrade is refused if the
    /// backup path already exists.
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
    /// 2 without the format guard). Upgraded automatically on open, after a
    /// backup to `<file>.rouchdb-0.5-pre.bak` unless
    /// [`UpgradePolicy::InPlaceNoBackup`] is chosen.
    PreRelease {
        /// The schema number recorded in the file.
        schema: u32,
    },
    /// Already in the current format.
    Current,
}

/// A stored revision body the upgrade discarded: the file held the same
/// revision under two spellings of its id that differ only in case (see
/// [`UpgradeReport::case_duplicate_bodies_discarded`]), with different
/// bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscardedRevision {
    /// The document.
    pub doc_id: String,
    /// The revision id as it was stored, whose body was discarded.
    pub rev: String,
    /// The revision id as it was stored, whose body was kept (under its
    /// lower-case id).
    pub kept: String,
}

/// What an upgrade found and did. Counts describe the file after the
/// upgrade; each `docs_with_…` list holds at most [`REPORT_SAMPLE`] ids (the
/// matching `…_count` has the total).
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
    /// Size of the file before the upgrade, in bytes. The upgrade needs
    /// about twice this much free disk space, and about three times with a
    /// backup (see the [`fmt::Display`] output).
    pub file_size: u64,
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
    /// Revisions the file held under more than one spelling of their id
    /// (differing only in case, such as `2-ABC…` and `2-abc…`), which 0.5
    /// treats as one revision: each was merged into one lower-case revision.
    /// When both spellings had a stored body, the bodies were compared: an
    /// identical copy was dropped, a different one is listed in
    /// [`case_duplicate_bodies_discarded`](Self::case_duplicate_bodies_discarded).
    pub case_duplicate_revs_merged: u64,
    /// Bodies of case-duplicate revisions that differed from the body kept,
    /// and were discarded (all of them, not a sample; they remain in the
    /// backup). The body kept is that of the spelling rouchdb 0.4 ranked
    /// first: a leaf before an inner revision, a live leaf before a deleted
    /// one, then the greater id in byte order (lower case sorts after upper
    /// case), which is the order in which 0.4 picked the winning revision.
    /// So if one spelling was the document's winning revision in 0.4, its
    /// body is the one kept.
    pub case_duplicate_bodies_discarded: Vec<DiscardedRevision>,
    /// Documents whose winning revision is a different revision after the
    /// upgrade (sample). Revision ids are compared by lower case: 0.4
    /// compared upper-case ids as written (`B` sorts before `a`), 0.5
    /// compares the lower-case ids, so the winner among conflicting
    /// revisions can change.
    pub docs_with_changed_winner: Vec<String>,
    /// Total number of such documents.
    pub docs_with_changed_winner_count: u64,
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
    /// Problems that did not stop the upgrade (for instance, the directory
    /// holding the backup could not be synced after the backup was renamed).
    pub warnings: Vec<String>,
}

impl UpgradeReport {
    fn new(path: &Path, from: StoredFormat) -> Self {
        UpgradeReport {
            path: path.to_path_buf(),
            from,
            upgraded: false,
            backup: None,
            file_size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
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
            case_duplicate_revs_merged: 0,
            case_duplicate_bodies_discarded: Vec::new(),
            docs_with_changed_winner: Vec::new(),
            docs_with_changed_winner_count: 0,
            old_revision_bodies: 0,
            docs_with_old_only_attachments: Vec::new(),
            docs_with_old_only_attachments_count: 0,
            docs_with_missing_attachments_count: 0,
            warnings: Vec::new(),
        }
    }
}

/// `bytes` for people: `270.0 MB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} bytes", bytes)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
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
            (_, false) => "not modified (dry run: the file was only read)",
        };
        writeln!(f, "status: {}", state)?;
        if let Some(backup) = &self.backup {
            writeln!(f, "backup: {}", backup.display())?;
        }
        write!(
            f,
            "documents: {} live, {} deleted",
            self.doc_count, self.doc_del_count
        )?;
        if self.from == StoredFormat::Current {
            return Ok(());
        }
        writeln!(f)?;
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
            "revisions stored under two spellings differing only in case, merged: {}",
            self.case_duplicate_revs_merged
        )?;
        writeln!(
            f,
            "differing bodies of such revisions discarded (kept in the backup): {}",
            self.case_duplicate_bodies_discarded.len()
        )?;
        for d in &self.case_duplicate_bodies_discarded {
            writeln!(
                f,
                "  document {:?}: discarded the body of {}, kept the body of {}",
                d.doc_id, d.rev, d.kept
            )?;
        }
        writeln!(
            f,
            "documents whose winning revision changes (0.5 compares revision ids in lower \
             case): {}{}",
            self.docs_with_changed_winner_count,
            sample_suffix(&self.docs_with_changed_winner)
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
        )?;
        for warning in &self.warnings {
            write!(f, "\nwarning: {}", warning)?;
        }
        self.write_notes(f)
    }
}

impl UpgradeReport {
    /// The advice after the facts, which depends on whether this was a dry
    /// run, on the backup and on who wrote the file.
    fn write_notes(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let legacy = self.from == StoredFormat::Legacy;
        writeln!(f)?;
        if !self.upgraded {
            let schema = match self.from {
                StoredFormat::PreRelease { schema } => schema,
                _ => 0,
            };
            writeln!(f)?;
            writeln!(
                f,
                "Disk space: the upgrade needs about twice the file size free (about {}): \
                 redb copies what it changes and commits in two phases. The backup, written \
                 by default, needs about the file size again: about three times the file \
                 size (about {}) free when backing up.",
                human_size(self.file_size.saturating_mul(2)),
                human_size(self.file_size.saturating_mul(3))
            )?;
            write!(
                f,
                "Backup: unless told otherwise the upgrade first writes a verified backup to {}",
                default_backup_path(&self.path, schema).display()
            )?;
            if legacy {
                writeln!(
                    f,
                    ", which rouchdb 0.4 can still open. After the upgrade rouchdb 0.4 can no \
                     longer open this file."
                )?;
            } else {
                writeln!(
                    f,
                    " (a copy of the file as the development build left it; rouchdb 0.4 cannot \
                     open it)."
                )?;
            }
            write!(
                f,
                "WARNING: rouchdb 0.4 never compacted. The first compact() after the upgrade \
                 will permanently delete the old revision bodies and the attachment bytes \
                 counted above."
            )
        } else {
            writeln!(f)?;
            write!(
                f,
                "WARNING: rouchdb 0.4 never compacted. The first compact() after this upgrade \
                 permanently deletes the old revision bodies and the attachment bytes counted \
                 above. "
            )?;
            match (&self.backup, legacy) {
                (Some(backup), true) => write!(
                    f,
                    "Keep the backup ({}) until you have checked that you do not need them: it \
                     is a complete copy of the file as rouchdb 0.4 left it, and still opens in \
                     rouchdb 0.4. This file no longer does.",
                    backup.display()
                ),
                (Some(backup), false) => write!(
                    f,
                    "Keep the backup ({}) until you have checked that you do not need them: it \
                     is a copy of the file as the 0.5 development build left it (rouchdb 0.4 \
                     cannot open it; this version upgrades it again).",
                    backup.display()
                ),
                (None, true) => write!(
                    f,
                    "No backup was written: make sure you have another copy before compacting \
                     if you may need them. rouchdb 0.4 can no longer open this file."
                ),
                (None, false) => write!(
                    f,
                    "No backup was written: make sure you have another copy before compacting \
                     if you may need them."
                ),
            }
        }
    }
}

fn sample_suffix(ids: &[String]) -> String {
    if ids.is_empty() {
        String::new()
    } else {
        format!(": {:?}", ids)
    }
}

fn push_sample(sample: &mut Vec<String>, id: &str) {
    if sample.len() < REPORT_SAMPLE {
        sample.push(id.to_string());
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

/// Open a database for the upgrade (or the backup) with a small cache: see
/// [`UPGRADE_CACHE_BYTES`].
fn open_for_upgrade(path: &Path) -> Result<Database> {
    db_err!(
        redb::Builder::new()
            .set_cache_size(UPGRADE_CACHE_BYTES)
            .open(path)
    )
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
                // unreadable by 0.4: finish their upgrade, keeping a backup.
                (UpgradePolicy::Refuse, _) => UpgradePolicy::WithBackup(None),
                (policy, _) => policy.clone(),
            };
            // Upgrade through a handle with a small cache (see
            // UPGRADE_CACHE_BYTES), then reopen with the default one.
            drop(db);
            let report = upgrade_file(path, &policy, false)?;
            let (db, _) = open_database(path, &OpenOptions::new())?;
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
    let db = open_for_upgrade(path)?;
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

    // Read only: a refusal happens here, before any backup, and a dry run
    // ends here without writing anything.
    let plan = analyze(db, path, schema, from)?;
    if dry_run {
        return Ok(plan.report);
    }

    let mut warnings = Vec::new();
    let backup = match policy {
        UpgradePolicy::WithBackup(dest) => {
            let dest = dest
                .clone()
                .unwrap_or_else(|| default_backup_path(path, schema));
            warnings.extend(backup_logical(db, &dest)?);
            Some(dest)
        }
        _ => None,
    };

    let prepared = (|| -> Result<WriteTransaction> {
        let mut txn = db_err!(db.begin_write())?;
        // The primary commit slot must be valid even if the machine crashes
        // while this (large) commit is written.
        txn.set_two_phase_commit(true);
        apply(&txn, &plan)?;
        fault::hit("upgrade:before_commit")?;
        Ok(txn)
    })();

    let txn = match prepared {
        Ok(txn) => txn,
        Err(e) => {
            // Nothing was committed: the file is exactly as it was, so the
            // backup is redundant (and would block a retry).
            if let Some(dest) = &backup {
                let _ = fs::remove_file(dest);
            }
            return Err(e);
        }
    };
    txn.commit().map_err(|e| {
        RouchError::DatabaseError(format!(
            "committing the upgrade of {} failed: {}{}",
            path.display(),
            e,
            match &backup {
                Some(dest) => format!(
                    ". The backup {} was kept: it is complete (it was verified before it got \
                     that name). If rouchdb still reports that the file must be upgraded, the \
                     file was not changed; move the backup elsewhere (or choose another \
                     backup path) before retrying",
                    dest.display()
                ),
                None => String::new(),
            }
        ))
    })?;
    let mut report = plan.report;
    report.upgraded = true;
    report.backup = backup;
    report.warnings.extend(warnings);
    Ok(report)
}

// ---------------------------------------------------------------------------
// Analysis (read only): the report and the plan of the writes
// ---------------------------------------------------------------------------

/// Everything the upgrade writes, worked out by [`analyze`].
struct Plan {
    report: UpgradeReport,
    /// The new metadata record (counts, schema).
    meta: MetaRecord,
    /// The other entries of the old metadata table (the security document
    /// of development builds).
    meta_entries: Vec<(String, Vec<u8>)>,
    /// Legacy attachment keys (`doc_id\0name`) and the digest of their bytes.
    rekey: Vec<(String, String)>,
    locals: Vec<LocalMove>,
    /// Documents whose revision tree changes (upper-case ids), re-encoded.
    doc_rewrites: Vec<(String, Vec<u8>)>,
    bodies: Vec<BodyMove>,
}

/// A `_local/` document stored as an ordinary document.
struct LocalMove {
    id: String,
    /// Its entry in the local store, unless it was deleted.
    local: Option<(String, Vec<u8>)>,
    body_keys: Vec<String>,
    /// Its change entry, if it still owns that sequence.
    change_seq: Option<u64>,
}

/// Stored bodies whose key holds an upper-case revision id: the bytes of
/// `keep` are stored under `to` (the lower-case key) and every key in
/// `remove` is deleted.
struct BodyMove {
    keep: String,
    to: String,
    remove: Vec<String>,
}

/// Open a table of an older file for reading; a table the file lacks reads
/// as empty (`None`). The upgrade creates it.
fn open_old<K: redb::Key + 'static, V: redb::Value + 'static>(
    txn: &ReadTransaction,
    def: TableDefinition<K, V>,
) -> Result<Option<ReadOnlyTable<K, V>>> {
    match txn.open_table(def) {
        Ok(table) => Ok(Some(table)),
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(RouchError::DatabaseError(e.to_string())),
    }
}

/// An error for a record the upgrade cannot decode. The upgrade stops
/// rather than skip data; nothing is changed.
fn corrupt(path: &Path, what: String, e: impl fmt::Display) -> RouchError {
    RouchError::DatabaseError(format!(
        "cannot upgrade {}: {} cannot be decoded ({}). Nothing was changed. Repair or \
         remove that record with the rouchdb version that wrote the file, then retry",
        path.display(),
        what,
        e
    ))
}

/// The version to use to fix a file the upgrade refuses.
fn writer(from: StoredFormat) -> &'static str {
    match from {
        StoredFormat::Legacy => "rouchdb 0.4",
        _ => "the rouchdb development build that wrote the file",
    }
}

/// `rev` (`pos-hash`) with a 32-digit hexadecimal hash in lower case.
fn normalize_rev_str(rev: &str) -> Cow<'_, str> {
    match rev.split_once('-') {
        Some((pos, hash)) => match normalize_rev_hash(hash) {
            Cow::Owned(lower) => Cow::Owned(format!("{pos}-{lower}")),
            Cow::Borrowed(_) => Cow::Borrowed(rev),
        },
        None => Cow::Borrowed(rev),
    }
}

/// Work out the upgrade from a read transaction: nothing is written.
fn analyze(db: &Database, path: &Path, schema: u32, from: StoredFormat) -> Result<Plan> {
    let txn = db_err!(db.begin_read())?;
    let mut report = UpgradeReport::new(path, from);

    // Every entry of the old metadata table: the metadata record, and the
    // security document of development builds.
    let mut meta_entries = Vec::new();
    let mut meta_bytes = None;
    {
        let table = db_err!(txn.open_table(LEGACY_META_TABLE))?;
        for entry in db_err!(table.iter())? {
            let (key, value) = db_err!(entry)?;
            if key.value() == META_KEY {
                meta_bytes = Some(value.value().to_vec());
            } else {
                meta_entries.push((key.value().to_string(), value.value().to_vec()));
            }
        }
    }
    let meta_bytes =
        meta_bytes.ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?;
    let mut meta: MetaRecord = serde_json::from_slice(&meta_bytes)
        .map_err(|e| corrupt(path, "the metadata record".into(), e))?;

    let docs = open_old(&txn, DOC_TABLE)?;
    let revs = open_old(&txn, REV_DATA_TABLE)?;
    let changes = open_old(&txn, CHANGES_TABLE)?;
    let locals_table = open_old(&txn, LOCAL_TABLE)?;
    let atts = open_old(&txn, ATTACHMENT_TABLE)?;

    // Schema 0 -> 1: attachment bytes were stored under `doc_id\0name` (so a
    // later write of the same name overwrote the bytes older revisions point
    // at). Each entry is re-keyed by its digest, which is what revision
    // metadata references. The bytes are read one entry at a time.
    let mut rekey = Vec::new();
    let mut new_digests = HashSet::new();
    if schema < 1
        && let Some(atts) = &atts
    {
        for entry in db_err!(atts.iter())? {
            let (key, value) = db_err!(entry)?;
            if key.value().contains('\0') {
                let digest = attachment_digest(value.value());
                new_digests.insert(digest.clone());
                rekey.push((key.value().to_string(), digest));
            }
        }
    }
    report.attachments_rekeyed = rekey.len() as u64;
    let has_bytes = |digest: &str| -> Result<bool> {
        Ok(new_digests.contains(digest)
            || match &atts {
                Some(atts) => db_err!(atts.get(digest))?.is_some(),
                None => false,
            })
    };

    let (locals, moved_bodies) = plan_local_moves(
        path,
        from,
        docs.as_ref(),
        revs.as_ref(),
        changes.as_ref(),
        locals_table.as_ref(),
        &mut report,
    )?;

    let (bodies, mut duplicates) = plan_body_moves(
        path,
        docs.as_ref(),
        revs.as_ref(),
        &moved_bodies,
        &mut report,
    )?;
    // Where each post-upgrade body key reads its bytes from.
    let body_source: HashMap<&str, &str> = bodies
        .iter()
        .map(|m| (m.to.as_str(), m.keep.as_str()))
        .collect();

    // Every document: its revision tree after the upgrade, then the counts
    // and the facts about attachments and old revisions.
    meta.doc_count = 0;
    meta.doc_del_count = 0;
    let mut doc_rewrites = Vec::new();
    let mut leaf_digests: HashSet<String> = HashSet::new();
    let mut old_only_candidates: Vec<(String, HashSet<String>)> = Vec::new();
    if let Some(docs) = &docs {
        for entry in db_err!(docs.iter())? {
            let (key, value) = db_err!(entry)?;
            let id = key.value();
            if id.starts_with("_local/") {
                continue; // moved above
            }
            let (tree, seq) = decode_doc_record(value.value())
                .map_err(|e| corrupt(path, format!("the record of document {:?}", id), e))?;
            let tree = match normalize_doc_tree(&tree, id, &mut duplicates, &mut report) {
                Some(normalized) => {
                    doc_rewrites.push((id.to_string(), encode_doc_record(&normalized, seq)?));
                    normalized
                }
                None => tree,
            };
            meta.adjust_counts(None, Some(is_deleted(&tree)));

            let leaves: HashSet<String> = collect_leaves(&tree)
                .iter()
                .map(|l| l.rev_string())
                .collect();
            let mut old_digests = HashSet::new();
            let mut missing = false;
            let mut seen = HashSet::new();
            let body_keys = match &revs {
                Some(revs) => rev_data_keys(revs, id)?,
                None => Vec::new(),
            };
            for body_key in body_keys {
                let rev = normalize_rev_str(&body_key[id.len() + 1..]).into_owned();
                if !seen.insert(rev.clone()) {
                    continue; // another spelling of a revision already read
                }
                let target = rev_data_key(id, &rev);
                let source = body_source
                    .get(target.as_str())
                    .copied()
                    .unwrap_or(body_key.as_str());
                let guard = match &revs {
                    Some(revs) => db_err!(revs.get(source))?,
                    None => None,
                }
                .ok_or_else(|| RouchError::DatabaseError("body vanished".into()))?;
                let record: RevAttachmentsRecord =
                    serde_json::from_slice(guard.value()).map_err(|e| {
                        corrupt(path, format!("the body of {:?} revision {}", id, rev), e)
                    })?;
                let is_leaf = leaves.contains(&rev);
                if !is_leaf {
                    report.old_revision_bodies += 1;
                }
                for att in record.attachments.into_values() {
                    if !has_bytes(&att.digest)? {
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
                push_sample(&mut report.docs_with_missing_attachments, id);
            }
            if !old_digests.is_empty() {
                old_only_candidates.push((id.to_string(), old_digests));
            }
        }
    }

    // Compaction keeps the bytes any leaf of any document references.
    for (id, digests) in old_only_candidates {
        if digests.iter().any(|d| !leaf_digests.contains(d)) {
            report.docs_with_old_only_attachments_count += 1;
            push_sample(&mut report.docs_with_old_only_attachments, &id);
        }
    }
    report.case_duplicate_revs_merged = duplicates.len() as u64;
    report.doc_count = meta.doc_count;
    report.doc_del_count = meta.doc_del_count;
    meta.schema = SCHEMA_VERSION;

    Ok(Plan {
        report,
        meta,
        meta_entries,
        rekey,
        locals,
        doc_rewrites,
        bodies,
    })
}

type StrTable = ReadOnlyTable<&'static str, &'static [u8]>;

/// rouchdb <= 0.4 stored `_local/` documents written through `bulk_docs`
/// (`put("_local/x")`) as ordinary documents; 0.5 keeps them in the local
/// store, where it looks them up. Each one moves there with its current
/// body; its revision tree, bodies and change entry are removed. Returns
/// the moves and the body keys they remove.
fn plan_local_moves(
    path: &Path,
    from: StoredFormat,
    docs: Option<&StrTable>,
    revs: Option<&StrTable>,
    changes: Option<&ReadOnlyTable<u64, &'static [u8]>>,
    locals: Option<&StrTable>,
    report: &mut UpgradeReport,
) -> Result<(Vec<LocalMove>, HashSet<String>)> {
    let mut moves = Vec::new();
    let mut moved_bodies = HashSet::new();
    let Some(docs) = docs else {
        return Ok((moves, moved_bodies));
    };
    // '0' follows '/': the range holds exactly the ids starting "_local/".
    for entry in db_err!(docs.range("_local/".."_local0"))? {
        let (key, value) = db_err!(entry)?;
        let id = key.value().to_string();
        let local_id = &id["_local/".len()..];
        let (tree, seq) = decode_doc_record(value.value())
            .map_err(|e| corrupt(path, format!("the record of document {:?}", id), e))?;
        let body_keys = match revs {
            Some(revs) => rev_data_keys(revs, &id)?,
            None => Vec::new(),
        };

        let local = match winning_rev(&tree) {
            Some(winner) if !is_deleted(&tree) => {
                if local_id.is_empty() {
                    return Err(RouchError::DatabaseError(format!(
                        "cannot upgrade {}: it holds a document with the id \"_local/\", a \
                         local document with an empty name, which rouchdb 0.5 cannot read or \
                         write. Nothing was changed. Copy its contents if you need them, delete \
                         it with {}, then retry",
                        path.display(),
                        writer(from)
                    )));
                }
                if let Some(locals) = locals
                    && db_err!(locals.get(local_id))?.is_some()
                {
                    return Err(RouchError::DatabaseError(format!(
                        "cannot upgrade {}: the document {:?} and the local document {:?} \
                         (written with put_local) are distinct in {} but the same document in \
                         0.5. Nothing was changed. Remove one of them with {}, then retry",
                        path.display(),
                        id,
                        local_id,
                        match from {
                            StoredFormat::Legacy => "rouchdb 0.4",
                            _ => "the development build that wrote the file",
                        },
                        writer(from)
                    )));
                }
                let key = rev_data_key(&id, &winner.to_string());
                let stored: Option<RevDataRecord> = match revs {
                    Some(revs) => match db_err!(revs.get(key.as_str()))? {
                        Some(guard) => Some(decode_body(guard.value()).map_err(|e| {
                            corrupt(path, format!("the body of {:?} revision {}", id, winner), e)
                        })?),
                        None => None,
                    },
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
                report.local_docs_moved += 1;
                report.local_attachments_dropped += attachments;
                report.local_conflicts_dropped += collect_conflicts(&tree).len() as u64;
                Some((local_id.to_string(), bytes))
            }
            _ => {
                report.local_tombstones_dropped += 1;
                None
            }
        };

        let owned_change = match changes {
            Some(changes) => match db_err!(changes.get(seq))? {
                Some(guard) => serde_json::from_slice::<ChangeRecord>(guard.value())
                    .is_ok_and(|change| change.doc_id == id),
                None => false,
            },
            None => false,
        };
        moved_bodies.extend(body_keys.iter().cloned());
        moves.push(LocalMove {
            id,
            local,
            body_keys,
            change_seq: owned_change.then_some(seq),
        });
    }
    Ok((moves, moved_bodies))
}

/// How rouchdb 0.4 ranked a spelling of a revision id in a document's tree:
/// greater is preferred (see [`UpgradeReport::case_duplicate_bodies_discarded`]).
/// `(is a leaf, is a live leaf, is in the tree, the id as stored)`.
type Rank = (bool, bool, bool, String);

/// `(is a leaf, is deleted)` of every node of `tree`, by revision id as
/// stored.
fn node_info(tree: &RevTree) -> HashMap<String, (bool, bool)> {
    let mut info = HashMap::new();
    traverse_rev_tree(tree, |pos, node, _| {
        info.insert(
            format!("{}-{}", pos, node.hash),
            (node.children.is_empty(), node.opts.deleted),
        );
    });
    info
}

fn rank(info: &HashMap<String, (bool, bool)>, rev: &str) -> Rank {
    match info.get(rev) {
        Some(&(leaf, deleted)) => (leaf, leaf && !deleted, true, rev.to_string()),
        None => (false, false, false, rev.to_string()),
    }
}

/// Stored bodies keyed by a revision id with upper-case digits get the
/// lower-case key. When several spellings of one revision have a body, the
/// best-ranked one is kept (see
/// [`UpgradeReport::case_duplicate_bodies_discarded`]) and the others are
/// dropped: reported if their body differs. Returns the moves, and the
/// merged revisions (as lower-case body keys).
fn plan_body_moves(
    path: &Path,
    docs: Option<&StrTable>,
    revs: Option<&StrTable>,
    moved_bodies: &HashSet<String>,
    report: &mut UpgradeReport,
) -> Result<(Vec<BodyMove>, HashSet<String>)> {
    let mut moves = Vec::new();
    let mut duplicates = HashSet::new();
    let Some(revs) = revs else {
        return Ok((moves, duplicates));
    };
    // Lower-case key -> the keys spelling it with upper case.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in db_err!(revs.iter())? {
        let (key, _) = db_err!(entry)?;
        let key = key.value();
        if moved_bodies.contains(key) {
            continue;
        }
        // Revision strings hold no NUL: the last one ends the document id.
        if let Some((doc_id, rev)) = key.rsplit_once('\0')
            && let Cow::Owned(lower) = normalize_rev_str(rev)
        {
            groups
                .entry(rev_data_key(doc_id, &lower))
                .or_default()
                .push(key.to_string());
        }
    }

    for (to, mut spellings) in groups {
        if db_err!(revs.get(to.as_str()))?.is_some() {
            spellings.push(to.clone());
        }
        if spellings.len() == 1 {
            let keep = spellings.pop().expect("one spelling");
            moves.push(BodyMove {
                keep: keep.clone(),
                to,
                remove: vec![keep],
            });
            continue;
        }

        // The same revision under several spellings.
        duplicates.insert(to.clone());
        let (doc_id, _) = to.rsplit_once('\0').expect("a body key");
        let ranks = match docs {
            Some(docs) => match load_doc_record(docs, doc_id)
                .map_err(|e| corrupt(path, format!("the record of document {:?}", doc_id), e))?
            {
                Some((tree, _)) => node_info(&tree),
                None => HashMap::new(),
            },
            None => HashMap::new(),
        };
        let rev_of = |key: &str| key[doc_id.len() + 1..].to_string();
        let keep = spellings
            .iter()
            .max_by_key(|key| rank(&ranks, &rev_of(key)))
            .expect("spellings")
            .clone();
        let kept_bytes = db_err!(revs.get(keep.as_str()))?
            .map(|g| g.value().to_vec())
            .unwrap_or_default();
        for other in spellings.iter().filter(|k| **k != keep) {
            let bytes = db_err!(revs.get(other.as_str()))?
                .map(|g| g.value().to_vec())
                .unwrap_or_default();
            if !same_body(&bytes, &kept_bytes) {
                report
                    .case_duplicate_bodies_discarded
                    .push(DiscardedRevision {
                        doc_id: doc_id.to_string(),
                        rev: rev_of(other),
                        kept: rev_of(&keep),
                    });
            }
        }
        let remove = spellings.into_iter().filter(|k| *k != to).collect();
        moves.push(BodyMove { keep, to, remove });
    }
    Ok((moves, duplicates))
}

/// Whether two stored bodies hold the same revision (same bytes, or the
/// same JSON written differently).
fn same_body(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    match (
        decode_body::<serde_json::Value>(a),
        decode_body::<serde_json::Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The revision tree of document `id` with lower-case revision ids, or
/// `None` if it has no upper-case id. A revision present under several
/// spellings becomes one node (its merged revisions are added to
/// `duplicates`). Counts the normalized ids and a change of winner in
/// `report`.
fn normalize_doc_tree(
    tree: &RevTree,
    id: &str,
    duplicates: &mut HashSet<String>,
    report: &mut UpgradeReport,
) -> Option<RevTree> {
    let mut normalized = tree.clone();
    let changed = normalize_tree(&mut normalized);
    if changed == 0 {
        return None;
    }
    report.revs_normalized += changed;

    let mut spellings: HashMap<String, Vec<String>> = HashMap::new();
    traverse_rev_tree(tree, |pos, node, _| {
        spellings
            .entry(format!("{}-{}", pos, normalize_rev_hash(&node.hash)))
            .or_default()
            .push(format!("{}-{}", pos, node.hash));
    });
    let merged: Vec<(String, Vec<String>)> =
        spellings.into_iter().filter(|(_, s)| s.len() > 1).collect();
    if !merged.is_empty() {
        normalized = collapse(&normalized);
        // A merged revision is deleted if the spelling 0.4 ranked first was.
        let info = node_info(tree);
        let mut deleted = HashMap::new();
        for (rev, spelled) in &merged {
            duplicates.insert(rev_data_key(id, rev));
            let best = spelled
                .iter()
                .max_by_key(|s| rank(&info, s))
                .expect("spellings");
            deleted.insert(rev.clone(), info.get(best).is_some_and(|&(_, d)| d));
        }
        set_deleted(&mut normalized, &deleted);
    }

    let before = winning_rev(tree).map(|r| normalize_rev_str(&r.to_string()).into_owned());
    let after = winning_rev(&normalized).map(|r| r.to_string());
    if before != after {
        report.docs_with_changed_winner_count += 1;
        push_sample(&mut report.docs_with_changed_winner, id);
    }
    Some(normalized)
}

/// Lower-case the revision ids of `tree` (iteratively: histories can be
/// long). Returns how many changed.
fn normalize_tree(tree: &mut RevTree) -> u64 {
    let mut changed = 0;
    let mut stack: Vec<&mut RevNode> = tree.iter_mut().map(|p| &mut p.tree).collect();
    while let Some(node) = stack.pop() {
        if let Cow::Owned(lower) = normalize_rev_hash(&node.hash) {
            node.hash = lower;
            changed += 1;
        }
        stack.extend(node.children.iter_mut());
    }
    changed
}

/// Rebuild `tree`, in which some revisions appear more than once, by
/// merging its root-to-leaf paths: each revision becomes one node, with
/// the children of all its copies, available if any copy was.
fn collapse(tree: &RevTree) -> RevTree {
    let mut result: RevTree = Vec::new();
    for (pos, nodes) in root_to_leaf(tree) {
        let mut path: Option<RevNode> = None;
        for (hash, opts, status) in nodes.into_iter().rev() {
            path = Some(RevNode {
                hash,
                status,
                opts,
                children: path.into_iter().collect(),
            });
        }
        if let Some(node) = path {
            // A rev_limit of 0 stems nothing.
            result = merge_tree(&result, &RevPath { pos, tree: node }, 0).0;
        }
    }
    result
}

/// Set the deleted flag of the nodes named in `deleted` (`pos-hash`).
fn set_deleted(tree: &mut RevTree, deleted: &HashMap<String, bool>) {
    let mut stack: Vec<(&mut RevNode, u64)> =
        tree.iter_mut().map(|p| (&mut p.tree, p.pos)).collect();
    while let Some((node, pos)) = stack.pop() {
        if let Some(&flag) = deleted.get(&format!("{}-{}", pos, node.hash)) {
            node.opts.deleted = flag;
        }
        stack.extend(node.children.iter_mut().map(|c| (c, pos + 1)));
    }
}

// ---------------------------------------------------------------------------
// Applying the plan (one write transaction)
// ---------------------------------------------------------------------------

fn apply(txn: &WriteTransaction, plan: &Plan) -> Result<()> {
    create_tables(txn)?;
    {
        // One attachment in memory at a time.
        let mut atts = db_err!(txn.open_table(ATTACHMENT_TABLE))?;
        for (key, digest) in &plan.rekey {
            let bytes = db_err!(atts.remove(key.as_str()))?.map(|g| g.value().to_vec());
            if let Some(bytes) = bytes
                && db_err!(atts.get(digest.as_str()))?.is_none()
            {
                db_err!(atts.insert(digest.as_str(), bytes.as_slice()))?;
            }
        }
    }
    fault::hit("upgrade:after_attachments")?;

    let mut docs = db_err!(txn.open_table(DOC_TABLE))?;
    let mut revs = db_err!(txn.open_table(REV_DATA_TABLE))?;
    {
        let mut changes = db_err!(txn.open_table(CHANGES_TABLE))?;
        let mut locals = db_err!(txn.open_table(LOCAL_TABLE))?;
        for m in &plan.locals {
            if let Some((local_id, bytes)) = &m.local {
                db_err!(locals.insert(local_id.as_str(), bytes.as_slice()))?;
            }
            for key in &m.body_keys {
                db_err!(revs.remove(key.as_str()))?;
            }
            if let Some(seq) = m.change_seq {
                db_err!(changes.remove(seq))?;
            }
            db_err!(docs.remove(m.id.as_str()))?;
        }
    }
    for (id, bytes) in &plan.doc_rewrites {
        db_err!(docs.insert(id.as_str(), bytes.as_slice()))?;
    }
    for m in &plan.bodies {
        let bytes = db_err!(revs.get(m.keep.as_str()))?.map(|g| g.value().to_vec());
        for key in &m.remove {
            db_err!(revs.remove(key.as_str()))?;
        }
        if let Some(bytes) = bytes {
            db_err!(revs.insert(m.to.as_str(), bytes.as_slice()))?;
        }
    }
    drop((docs, revs));

    {
        let mut table = db_err!(txn.open_table(META_TABLE))?;
        for (key, value) in &plan.meta_entries {
            db_err!(table.insert(key.as_str(), value.as_slice()))?;
        }
        write_meta(&mut table, &plan.meta)?;
    }
    db_err!(txn.delete_table(LEGACY_META_TABLE))?;
    create_current_tables(txn)?;
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
/// is removed; the source is only ever read. Returns a warning if the
/// directory could not be synced after the rename (the backup is complete
/// then, but a crash soon after could undo the rename).
fn backup_logical(db: &Database, dest: &Path) -> Result<Option<String>> {
    if dest.exists() {
        return Err(RouchError::DatabaseError(format!(
            "backup destination {} already exists; nothing was changed. If an earlier \
             upgrade attempt of this file left it there, it is a complete backup (a backup \
             gets its final name only after it has been verified): move it elsewhere to keep \
             it, or choose another backup path, then retry",
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
                    "{} exists: it is left over from an upgrade attempt that was interrupted \
                     while writing the backup. It is not a complete backup, and the database \
                     file was not changed by that attempt: delete it and retry (nothing was \
                     changed)",
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
        // redb's commit already made the copy durable. Opened for writing:
        // Windows cannot flush a file opened read-only.
        fs::OpenOptions::new()
            .write(true)
            .open(&partial)?
            .sync_all()?;
        if dest.exists() {
            return Err(RouchError::DatabaseError(format!(
                "backup destination {} appeared during the backup",
                dest.display()
            )));
        }
        fs::rename(&partial, dest)?;
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
    // The backup is complete under its final name. Only the durability of
    // the rename depends on the directory sync.
    let synced = fault::hit("backup:sync_dir")
        .map_err(|e| std::io::Error::other(e.to_string()))
        .and_then(|()| sync_parent_dir(dest));
    Ok(synced.err().map(|e| {
        format!(
            "the backup {} is complete, but its directory could not be synced after the \
             rename ({}): if the machine crashes soon, the backup may reappear as {}",
            dest.display(),
            e,
            partial.display()
        )
    }))
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
        let backup = db_err!(
            redb::Builder::new()
                .set_cache_size(UPGRADE_CACHE_BYTES)
                .create(partial)
        )?;
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
    let backup = open_for_upgrade(partial)?;
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
        assert!(
            err.to_string()
                .contains("left over from an upgrade attempt that was interrupted"),
            "{err}"
        );
        assert!(err.to_string().contains("delete it"), "{err}");
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

    /// The dry run reports exactly what the upgrade then does.
    #[tokio::test]
    async fn dry_run_reports_what_the_upgrade_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let mut dry = RedbAdapter::inspect_upgrade(&path).unwrap();
        let real = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
        dry.upgraded = true;
        dry.backup = real.backup.clone();
        assert_eq!(dry, real);
    }

    /// A directory that cannot be synced after the backup's rename is a
    /// warning: the backup is complete, the upgrade goes on.
    #[tokio::test]
    async fn a_failed_directory_sync_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);
        fault::set(Some("backup:sync_dir"));
        let result = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None));
        fault::set(None);
        let report = result.unwrap();
        assert!(report.upgraded);
        let backup = default_backup_path(&path, 0);
        assert_eq!(snapshot(&backup), before);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        let text = report.to_string();
        assert!(text.contains("warning: the backup"), "{text}");
        assert!(text.contains("injected failure"), "{text}");
        assert_guarded(&path);
    }

    /// Without a backup, the advice does not mention one.
    #[tokio::test]
    async fn the_advice_matches_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).unwrap();
        let text = report.to_string();
        assert!(text.contains("No backup was written"), "{text}");
        assert!(
            text.contains("rouchdb 0.4 can no longer open this file"),
            "{text}"
        );
        assert!(!text.contains("Keep the backup"), "{text}");

        let path = dir.path().join("old2.redb");
        sample_legacy(&path);
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
        let text = report.to_string();
        assert!(text.contains("Keep the backup"), "{text}");
        assert!(text.contains("still opens in rouchdb 0.4"), "{text}");
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        let before = snapshot(&path);
        let raw = std::fs::read(&path).unwrap();
        let report = RedbAdapter::inspect_upgrade(&path).unwrap();
        assert!(!report.upgraded);
        assert_eq!(report.local_docs_moved, 1);
        assert_eq!(report.doc_count, 3);
        assert_eq!(report.file_size, raw.len() as u64);
        let text = report.to_string();
        assert!(text.contains("dry run"), "{text}");
        assert!(text.contains("three times the file size"), "{text}");
        assert!(text.contains("old.redb.rouchdb-0.4.bak"), "{text}");
        assert!(text.contains("will permanently delete"), "{text}");
        // Only read: not a byte of the file changed (not even its size).
        assert!(
            std::fs::read(&path).unwrap() == raw,
            "the dry run modified the file"
        );
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

    fn one_rev_tree() -> RevTree {
        vec![build_path_from_revs(
            1,
            &[hex(1)],
            NodeOpts::default(),
            RevStatus::Available,
        )]
    }

    /// A tree of single-revision roots (conflicting leaves) `(pos-hash,
    /// deleted)`.
    fn leaves_tree(leaves: &[(&str, bool)]) -> RevTree {
        leaves
            .iter()
            .map(|(rev, deleted)| {
                let (pos, hash) = rev.split_once('-').unwrap();
                build_path_from_revs(
                    pos.parse().unwrap(),
                    &[hash.to_string()],
                    NodeOpts { deleted: *deleted },
                    RevStatus::Available,
                )
            })
            .collect()
    }

    fn deleted_body(data: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"data": data, "deleted": true})).unwrap()
    }

    /// The same revision stored under two spellings of its id: merged into
    /// one lower-case revision; an identical body is dropped silently, a
    /// different one is reported, and the body kept is the one of the
    /// spelling 0.4 ranked first.
    #[tokio::test]
    async fn case_duplicates_are_merged_and_differing_bodies_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        let upper = "ABCDEF0123456789ABCDEF0123456789";
        let lower = upper.to_ascii_lowercase();
        let up = format!("1-{upper}");
        let low = format!("1-{lower}");
        legacy_file(&path, |txn| {
            // Identical bodies: nothing to report.
            put(
                txn,
                DOC_TABLE,
                "same",
                &legacy_record(&leaves_tree(&[(&up, false), (&low, false)]), 1),
            );
            // Different bodies, both live leaves: 0.4's winner is the
            // greater id, the lower-case one.
            put(
                txn,
                DOC_TABLE,
                "differ",
                &legacy_record(&leaves_tree(&[(&up, false), (&low, false)]), 2),
            );
            // Different bodies, the lower-case spelling deleted: 0.4's
            // winner is the live upper-case one.
            put(
                txn,
                DOC_TABLE,
                "live-upper",
                &legacy_record(&leaves_tree(&[(&up, false), (&low, true)]), 3),
            );
            // Only the upper-case spelling is in the tree, but a body is
            // stored under both.
            put(
                txn,
                DOC_TABLE,
                "orphan",
                &legacy_record(&leaves_tree(&[(&up, false)]), 4),
            );
            for id in ["same", "differ", "live-upper", "orphan"] {
                let same = id == "same";
                put(
                    txn,
                    REV_DATA_TABLE,
                    &rev_data_key(id, &up),
                    &body(
                        serde_json::json!({"spelling": "upper"}),
                        serde_json::json!({}),
                    ),
                );
                let lower_body = if id == "live-upper" {
                    deleted_body(serde_json::json!({"spelling": "lower"}))
                } else if same {
                    body(
                        serde_json::json!({"spelling": "upper"}),
                        serde_json::json!({}),
                    )
                } else {
                    body(
                        serde_json::json!({"spelling": "lower"}),
                        serde_json::json!({}),
                    )
                };
                put(txn, REV_DATA_TABLE, &rev_data_key(id, &low), &lower_body);
            }
            for (seq, id) in ["same", "differ", "live-upper", "orphan"]
                .iter()
                .enumerate()
            {
                change(txn, seq as u64 + 1, id);
            }
        });
        let before = snapshot(&path);

        let dry = RedbAdapter::inspect_upgrade(&path).unwrap();
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)).unwrap();
        assert_eq!(report.case_duplicate_revs_merged, 4);
        let discarded = |id: &str, rev: &str, kept: &str| DiscardedRevision {
            doc_id: id.into(),
            rev: rev.into(),
            kept: kept.into(),
        };
        assert_eq!(
            report.case_duplicate_bodies_discarded,
            [
                discarded("differ", &up, &low),
                discarded("live-upper", &low, &up),
                discarded("orphan", &low, &up),
            ]
        );
        let text = report.to_string();
        assert!(
            text.contains(&format!(
                "document \"differ\": discarded the body of {up}, kept the body of {low}"
            )),
            "{text}"
        );
        // The dry run found the same.
        assert_eq!(
            dry.case_duplicate_bodies_discarded,
            report.case_duplicate_bodies_discarded
        );
        // The backup has every body.
        let backup = report.backup.clone().unwrap();
        assert_eq!(snapshot(&backup), before);

        let db = RedbAdapter::open(&path, "old").unwrap();
        for (id, spelling, deleted) in [
            ("same", "upper", false),
            ("differ", "lower", false),
            ("live-upper", "upper", false),
            ("orphan", "upper", false),
        ] {
            let doc = db.get(id, GetOptions::default()).await.unwrap();
            assert_eq!(doc.rev.unwrap().to_string(), low, "{id}");
            assert_eq!(doc.data["spelling"], spelling, "{id}");
            assert_eq!(doc.deleted, deleted, "{id}");
            // One revision, no conflict with itself.
            let opened = db
                .get(
                    id,
                    GetOptions {
                        conflicts: true,
                        revs: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(opened.data.get("_conflicts").is_none(), "{id}: {opened:?}");
        }
        assert_eq!(db.info().await.unwrap().doc_count, 4);
        // No upper-case key is left.
        let txn = db.inner.db.begin_read().unwrap();
        let revs = txn.open_table(REV_DATA_TABLE).unwrap();
        for entry in revs.iter().unwrap() {
            let (key, _) = entry.unwrap();
            assert!(!key.value().contains(upper), "{:?}", key.value());
        }
    }

    /// The same revision in two places of one tree (a stemmed branch under
    /// one spelling, the full branch under the other) becomes one node that
    /// keeps both histories.
    #[tokio::test]
    async fn case_duplicates_across_branches_are_joined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        let upper = "ABCDEF0123456789ABCDEF0123456789";
        legacy_file(&path, |txn| {
            let mut tree = vec![build_path_from_revs(
                2,
                &[upper.to_string(), hex(1)],
                NodeOpts::default(),
                RevStatus::Available,
            )];
            // 2-abc.. -> 3-..., stored as its own root.
            tree.push(build_path_from_revs(
                3,
                &[hex(3), upper.to_ascii_lowercase()],
                NodeOpts::default(),
                RevStatus::Available,
            ));
            put(txn, DOC_TABLE, "j", &legacy_record(&tree, 1));
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("j", &format!("3-{}", hex(3))),
                &body(serde_json::json!({"v": 3}), serde_json::json!({})),
            );
            change(txn, 1, "j");
        });
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).unwrap();
        assert_eq!(report.case_duplicate_revs_merged, 1);
        assert!(report.case_duplicate_bodies_discarded.is_empty());
        let db = RedbAdapter::open(&path, "old").unwrap();
        let doc = db
            .get(
                "j",
                GetOptions {
                    revs: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), format!("3-{}", hex(3)));
        // One branch: 3 -> 2 -> 1.
        assert_eq!(
            doc.data["_revisions"]["ids"],
            serde_json::json!([hex(3), upper.to_ascii_lowercase(), hex(1)])
        );
    }

    /// 0.4 compared upper-case ids as written ('B' < 'a'), 0.5 compares the
    /// lower-case ids: the winner of a conflict can change, and is reported.
    #[tokio::test]
    async fn winner_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        let b_upper = format!("2-B{}", "0".repeat(31));
        let a_lower = format!("2-a{}", "0".repeat(31));
        legacy_file(&path, |txn| {
            put(
                txn,
                DOC_TABLE,
                "w",
                &legacy_record(&leaves_tree(&[(&b_upper, false), (&a_lower, false)]), 1),
            );
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("w", &b_upper),
                &body(serde_json::json!({"side": "b"}), serde_json::json!({})),
            );
            put(
                txn,
                REV_DATA_TABLE,
                &rev_data_key("w", &a_lower),
                &body(serde_json::json!({"side": "a"}), serde_json::json!({})),
            );
            change(txn, 1, "w");
            // Upper case, but the same winner.
            put(
                txn,
                DOC_TABLE,
                "u",
                &legacy_record(
                    &leaves_tree(&[(&format!("1-{}", "F".repeat(32)), false)]),
                    2,
                ),
            );
            change(txn, 2, "u");
        });
        let dry = RedbAdapter::inspect_upgrade(&path).unwrap();
        assert_eq!(dry.docs_with_changed_winner, ["w"]);
        assert_eq!(dry.docs_with_changed_winner_count, 1);
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).unwrap();
        assert_eq!(report.docs_with_changed_winner, ["w"]);
        assert!(
            report
                .to_string()
                .contains("documents whose winning revision changes (0.5 compares revision ids in lower case): 1: [\"w\"]"),
            "{report}"
        );
        let db = RedbAdapter::open(&path, "old").unwrap();
        let doc = db.get("w", GetOptions::default()).await.unwrap();
        assert_eq!(doc.data["side"], "b");
    }

    /// `_local/` (an empty local name) has no place in 0.5: refused before
    /// anything is written, unless it was deleted.
    #[tokio::test]
    async fn an_empty_local_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.redb");
        sample_legacy(&path);
        {
            let db = Database::open(&path).unwrap();
            let txn = db.begin_write().unwrap();
            put(
                &txn,
                DOC_TABLE,
                "_local/",
                &legacy_record(&one_rev_tree(), 10),
            );
            put(
                &txn,
                REV_DATA_TABLE,
                &rev_data_key("_local/", &format!("1-{}", hex(1))),
                &body(serde_json::json!({"x": 1}), serde_json::json!({})),
            );
            txn.commit().unwrap();
        }
        let before = snapshot(&path);
        for result in [
            RedbAdapter::inspect_upgrade(&path),
            RedbAdapter::upgrade(&path, UpgradePolicy::WithBackup(None)),
        ] {
            let msg = result.expect_err("refused").to_string();
            assert!(
                msg.contains("\"_local/\"")
                    && msg.contains("Nothing was changed")
                    && msg.contains("delete it with rouchdb 0.4"),
                "{msg}"
            );
        }
        assert_eq!(snapshot(&path), before);
        assert!(!default_backup_path(&path, 0).exists());

        // Once deleted in 0.4 it is a tombstone, which is dropped.
        {
            let db = Database::open(&path).unwrap();
            let txn = db.begin_write().unwrap();
            let mut tree = one_rev_tree();
            tree[0].tree.opts.deleted = true;
            put(&txn, DOC_TABLE, "_local/", &legacy_record(&tree, 10));
            txn.commit().unwrap();
        }
        let report = RedbAdapter::upgrade(&path, UpgradePolicy::InPlaceNoBackup).unwrap();
        assert_eq!(report.local_tombstones_dropped, 2);
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
        let before = snapshot(&path);
        let db = RedbAdapter::open(&path, "dev").unwrap();
        let report = db.upgrade_report().unwrap().clone();
        assert_eq!(report.from, StoredFormat::PreRelease { schema: 2 });
        // A plain open backs the file up first, under a name that says 0.4
        // cannot open it.
        let backup = default_backup_path(&path, 2);
        assert!(backup.ends_with("dev.redb.rouchdb-0.5-pre.bak"));
        assert!(report.upgraded);
        assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
        assert_eq!(snapshot(&backup), before);
        let text = report.to_string();
        assert!(text.contains("development build"), "{text}");
        assert!(text.contains("rouchdb 0.4 cannot open it"), "{text}");
        assert!(!text.contains("still opens in rouchdb 0.4"), "{text}");
        assert_eq!(db.get_security().await.unwrap().admins.names, ["alice"]);
        assert_eq!(db.info().await.unwrap().doc_count, 1);
        drop(db);
        assert_guarded(&path);
    }

    /// Turn a current file back into the layout of a development build.
    fn make_pre_release(path: &Path) {
        let db = Database::open(path).unwrap();
        let txn = db.begin_write().unwrap();
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

    #[tokio::test]
    async fn pre_release_files_can_be_upgraded_without_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.redb");
        drop(RedbAdapter::open(&path, "dev").unwrap());
        make_pre_release(&path);
        let db = RedbAdapter::open_with(
            &path,
            "dev",
            OpenOptions::new().upgrade(UpgradePolicy::InPlaceNoBackup),
        )
        .unwrap();
        let report = db.upgrade_report().unwrap();
        assert!(report.upgraded && report.backup.is_none());
        let text = report.to_string();
        assert!(text.contains("No backup was written"), "{text}");
        assert!(!text.contains("rouchdb 0.4 can no longer"), "{text}");
        assert!(!default_backup_path(&path, 2).exists());

        // A refusal names the development build, not 0.4.
        drop(db);
        make_pre_release(&path);
        {
            let db = Database::open(&path).unwrap();
            let txn = db.begin_write().unwrap();
            put(
                &txn,
                DOC_TABLE,
                "_local/",
                &legacy_record(&one_rev_tree(), 1),
            );
            txn.commit().unwrap();
        }
        let err = RedbAdapter::open(&path, "dev").err().expect("refused");
        let msg = err.to_string();
        assert!(msg.contains("development build"), "{msg}");
        assert!(!msg.contains("rouchdb 0.4,"), "{msg}");
        assert!(!default_backup_path(&path, 2).exists());
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
