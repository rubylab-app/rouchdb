use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::{Result, RouchError};
use rouchdb_core::json::MAX_NESTING_DEPTH;
use rouchdb_core::merge::{
    collect_conflicts, is_deleted, latest_leaf, remove_leaves, revs_diff_one, winning_rev,
};
use rouchdb_core::rev_tree::{
    NodeOpts, RevNode, RevPath, RevStatus, RevTree, collect_leaves, find_rev_ancestry, rev_exists,
    revs_info, traverse_rev_tree,
};
use rouchdb_core::write::{
    LocalWrite, PlannedWrite, ReplicatedWrite, edit_parent, error_result, local_doc_id,
    local_document, ok_result, plan_local_write, plan_new_edit, plan_replicated_edit,
};

/// Revisions kept per branch unless configured otherwise (CouchDB's
/// default `_revs_limit`).
pub const DEFAULT_REV_LIMIT: u64 = 1000;

macro_rules! db_err {
    ($e:expr) => {
        $e.map_err(|e| RouchError::DatabaseError(e.to_string()))
    };
}

// ---------------------------------------------------------------------------
// Table definitions for redb
// ---------------------------------------------------------------------------

/// Document metadata table: doc_id -> serialized DocRecord
const DOC_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("docs");

/// Document revision data: "doc_id\0rev_str" -> serialized JSON bytes
const REV_DATA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("rev_data");

/// Changes table: sequence_number -> serialized ChangeRecord
const CHANGES_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("changes");

/// Local documents: local_id -> serialized JSON
const LOCAL_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("local_docs");

/// Attachments: digest -> raw bytes (content-addressed, shared by every
/// revision and document that references the same content)
const ATTACHMENT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("attachments");

/// Metadata table: key -> value
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

// ---------------------------------------------------------------------------
// Serializable records
// ---------------------------------------------------------------------------

/// Document metadata record: the revision tree stored as a flat node list
/// (pre-order, each node pointing at its parent's index) plus the doc's
/// current sequence. Flat storage keeps (de)serialization depth constant no
/// matter how long the history is.
#[derive(Debug, Serialize, Deserialize)]
struct DocRecord {
    revs: Vec<FlatRevNode>,
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FlatRevNode {
    pos: u64,
    hash: String,
    /// Index of the parent node; `None` for a root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    missing: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    deleted: bool,
}

/// Document record written by rouchdb <= 0.4: a nested tree with two JSON
/// levels per generation. Still read (and rewritten flat on the next write
/// of the document); never written.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyDocRecord {
    rev_tree: Vec<SerializedRevPath>,
    seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedRevPath {
    pos: u64,
    tree: SerializedRevNode,
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedRevNode {
    hash: String,
    status: String,
    deleted: bool,
    children: Vec<SerializedRevNode>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevDataRecord {
    data: serde_json::Value,
    deleted: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    attachments: HashMap<String, AttachmentRecord>,
}

/// The attachment metadata of a stored revision, decoded without its body
/// (which is skipped without recursion, whatever its depth).
#[derive(Debug, Deserialize)]
struct RevAttachmentsRecord {
    #[serde(default)]
    attachments: HashMap<String, AttachmentRecord>,
}

/// Stored attachment metadata. The members added in 0.5 default when a
/// record written by an earlier version is read.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct AttachmentRecord {
    content_type: String,
    digest: String,
    length: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    revpos: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encoded_length: Option<u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Serialize, Deserialize)]
struct ChangeRecord {
    doc_id: String,
    deleted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct MetaRecord {
    update_seq: u64,
    db_uuid: String,
    /// On-disk layout version (absent in files written by rouchdb <= 0.4).
    #[serde(default)]
    schema: u32,
    /// Number of purge requests applied.
    #[serde(default)]
    purge_seq: u64,
    /// Live / deleted document counts, maintained on every write so `info()`
    /// and `total_rows` do not scan the database (schema >= 2).
    #[serde(default)]
    doc_count: u64,
    #[serde(default)]
    doc_del_count: u64,
}

// ---------------------------------------------------------------------------
// Conversion helpers (RevTree <-> Serializable)
// ---------------------------------------------------------------------------

/// Legacy nested encoding (only used by tests to fabricate old records).
#[cfg(test)]
fn legacy_tree_to_serialized(tree: &RevTree) -> Vec<SerializedRevPath> {
    tree.iter()
        .map(|path| SerializedRevPath {
            pos: path.pos,
            tree: rev_node_to_serialized(&path.tree),
        })
        .collect()
}

#[cfg(test)]
fn rev_node_to_serialized(node: &RevNode) -> SerializedRevNode {
    SerializedRevNode {
        hash: node.hash.clone(),
        status: match node.status {
            RevStatus::Available => "available".into(),
            RevStatus::Missing => "missing".into(),
        },
        deleted: node.opts.deleted,
        children: node.children.iter().map(rev_node_to_serialized).collect(),
    }
}

fn serialized_to_rev_tree(paths: &[SerializedRevPath]) -> RevTree {
    paths
        .iter()
        .map(|p| RevPath {
            pos: p.pos,
            tree: serialized_to_rev_node(&p.tree),
        })
        .collect()
}

fn serialized_to_rev_node(node: &SerializedRevNode) -> RevNode {
    RevNode {
        hash: node.hash.clone(),
        status: if node.status == "available" {
            RevStatus::Available
        } else {
            RevStatus::Missing
        },
        opts: NodeOpts {
            deleted: node.deleted,
        },
        children: node.children.iter().map(serialized_to_rev_node).collect(),
    }
}

/// Serialize a document's revision tree and sequence (flat format).
fn encode_doc_record(tree: &RevTree, seq: u64) -> Result<Vec<u8>> {
    let mut revs = Vec::new();
    for path in tree {
        // Iterative pre-order walk; children are pushed in reverse so they
        // are emitted in order.
        let mut stack: Vec<(&RevNode, u64, Option<u32>)> = vec![(&path.tree, path.pos, None)];
        while let Some((node, pos, parent)) = stack.pop() {
            let idx = revs.len() as u32;
            revs.push(FlatRevNode {
                pos,
                hash: node.hash.clone(),
                parent,
                missing: node.status == RevStatus::Missing,
                deleted: node.opts.deleted,
            });
            for child in node.children.iter().rev() {
                stack.push((child, pos + 1, Some(idx)));
            }
        }
    }
    Ok(serde_json::to_vec(&DocRecord { revs, seq })?)
}

/// Deserialize a document record, accepting both the current flat format
/// and the legacy nested one. Errors are always propagated: a record that
/// cannot be decoded must never be mistaken for a missing document.
fn decode_doc_record(bytes: &[u8]) -> Result<(RevTree, u64)> {
    if bytes.starts_with(b"{\"rev_tree\"") {
        return decode_legacy_doc_record(bytes);
    }
    let record: DocRecord = serde_json::from_slice(bytes)
        .map_err(|e| RouchError::DatabaseError(format!("corrupt document record: {}", e)))?;

    // Nodes are in pre-order, so every child comes after its parent: attach
    // them back to front, then restore each node's child order.
    let mut built: Vec<Option<RevNode>> = record
        .revs
        .iter()
        .map(|n| {
            Some(RevNode {
                hash: n.hash.clone(),
                status: if n.missing {
                    RevStatus::Missing
                } else {
                    RevStatus::Available
                },
                opts: NodeOpts { deleted: n.deleted },
                children: Vec::new(),
            })
        })
        .collect();
    let corrupt = || RouchError::DatabaseError("corrupt document record: bad parent index".into());
    let mut roots = Vec::new();
    for (i, flat) in record.revs.iter().enumerate().rev() {
        let mut node = built[i].take().ok_or_else(corrupt)?;
        node.children.reverse();
        match flat.parent {
            None => roots.push(RevPath {
                pos: flat.pos,
                tree: node,
            }),
            Some(p) if (p as usize) < i => built[p as usize]
                .as_mut()
                .ok_or_else(corrupt)?
                .children
                .push(node),
            Some(_) => return Err(corrupt()),
        }
    }
    roots.reverse();
    Ok((roots, record.seq))
}

/// Decode a legacy nested record. Histories longer than ~60 revisions
/// exceed serde_json's recursion limit, so those are decoded on a thread
/// with a large stack and the limit disabled.
fn decode_legacy_doc_record(bytes: &[u8]) -> Result<(RevTree, u64)> {
    fn convert(record: LegacyDocRecord) -> (RevTree, u64) {
        (serialized_to_rev_tree(&record.rev_tree), record.seq)
    }

    if let Ok(record) = serde_json::from_slice::<LegacyDocRecord>(bytes) {
        return Ok(convert(record));
    }

    let owned = bytes.to_vec();
    std::thread::Builder::new()
        .name("rouchdb-legacy-decode".into())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || -> Result<(RevTree, u64)> {
            use serde::Deserialize as _;
            let mut de = serde_json::Deserializer::from_slice(&owned);
            de.disable_recursion_limit();
            let record = LegacyDocRecord::deserialize(&mut de).map_err(|e| {
                RouchError::DatabaseError(format!("corrupt document record: {}", e))
            })?;
            Ok(convert(record))
        })
        .map_err(|e| RouchError::DatabaseError(e.to_string()))?
        .join()
        .map_err(|_| RouchError::DatabaseError("legacy record decoding panicked".into()))?
}

/// Load and decode a document's record, if it exists.
fn load_doc_record<T>(table: &T, doc_id: &str) -> Result<Option<(RevTree, u64)>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    match db_err!(table.get(doc_id))? {
        Some(guard) => decode_doc_record(guard.value()).map(Some),
        None => Ok(None),
    }
}

/// Key of a stored revision body: `doc_id\0rev`. Revision ids never
/// contain NUL (`Revision` rejects it), so the key is unambiguous even for
/// ids that contain NUL; see [`rev_data_keys`].
fn rev_data_key(doc_id: &str, rev_str: &str) -> String {
    format!("{}\0{}", doc_id, rev_str)
}

/// Decode a stored document body (or local document), which may be nested
/// up to [`MAX_NESTING_DEPTH`] levels (one more for the record around it).
fn decode_body<T>(bytes: &[u8]) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    Ok(rouchdb_core::json::from_slice(
        bytes,
        MAX_NESTING_DEPTH + 1,
    )?)
}

/// The canonical form of a revision string (`InvalidRev` if malformed).
fn canonical_rev(rev_str: &str) -> Result<String> {
    Ok(rev_str.parse::<Revision>()?.to_string())
}

/// Whether `rev` (canonical) names a revision of `tree`: a stored body of a
/// revision that is not in the tree (stemmed by an older version) is gone.
fn in_tree(tree: &RevTree, rev: &str) -> bool {
    parse_rev(rev).is_ok_and(|(pos, hash)| rev_exists(tree, pos, &hash))
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Persistent adapter backed by `redb`.
///
/// redb is a synchronous engine (commits fsync), so every operation runs on
/// Tokio's blocking thread pool instead of an async worker thread.
pub struct RedbAdapter {
    inner: Arc<Inner>,
    rev_limit: u64,
}

struct Inner {
    db: Database,
    name: String,
    /// Serializes writers before they reach redb (which would otherwise park
    /// one blocking thread per waiting writer).
    write_lock: Mutex<()>,
}

impl RedbAdapter {
    /// Open or create a database at the given path.
    ///
    /// Databases written by older versions are upgraded in place: attachment
    /// bytes stored per `(document, name)` are re-keyed by digest, and legacy
    /// revision-tree records are rewritten on their next write. A database
    /// written by a newer version (a schema this one does not know) is
    /// refused rather than misread.
    pub fn open(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let db = Database::create(path.as_ref())
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        // Initialize tables
        {
            let write_txn = db
                .begin_write()
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
            // Opening tables in a write transaction creates them if they don't exist
            create_tables(&write_txn)?;
            {
                let mut meta_table = db_err!(write_txn.open_table(META_TABLE))?;
                let existing = match db_err!(meta_table.get(META_KEY))? {
                    Some(guard) => Some(serde_json::from_slice::<MetaRecord>(guard.value())?),
                    None => None,
                };
                match existing {
                    None => write_meta(&mut meta_table, &MetaRecord::new())?,
                    Some(meta) if meta.schema > SCHEMA_VERSION => {
                        return Err(RouchError::DatabaseError(format!(
                            "{}: on-disk schema version {} is newer than the {} this version \
                             of rouchdb supports; open it with a newer rouchdb",
                            path.as_ref().display(),
                            meta.schema,
                            SCHEMA_VERSION
                        )));
                    }
                    Some(mut meta) if meta.schema < SCHEMA_VERSION => {
                        if meta.schema < 1 {
                            migrate_attachments_to_digest_keys(&write_txn)?;
                        }
                        if meta.schema < 2 {
                            count_documents(&write_txn, &mut meta)?;
                        }
                        meta.schema = SCHEMA_VERSION;
                        write_meta(&mut meta_table, &meta)?;
                    }
                    Some(_) => {}
                }
            }
            write_txn
                .commit()
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        }

        Ok(Self {
            inner: Arc::new(Inner {
                db,
                name: name.to_string(),
                write_lock: Mutex::new(()),
            }),
            rev_limit: DEFAULT_REV_LIMIT,
        })
    }

    /// Keep at most `limit` revisions per branch of a document's history
    /// (PouchDB's `revs_limit`, CouchDB's `_revs_limit`; 0 means no limit).
    /// Older revisions are stemmed away on the next write of the document
    /// and are no longer readable. The limit is a property of this handle,
    /// not of the file.
    pub fn with_rev_limit(mut self, limit: u64) -> Self {
        self.rev_limit = limit;
        self
    }

    /// Run storage work on the blocking thread pool (or inline when called
    /// outside a Tokio runtime).
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle
                .spawn_blocking(move || f(&inner))
                .await
                .map_err(|e| RouchError::DatabaseError(format!("storage task failed: {}", e)))?,
            Err(_) => f(&inner),
        }
    }

    /// Like `run`, for operations that write: writers queue on an async
    /// lock first.
    async fn run_write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Inner) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _guard = self.inner.write_lock.lock().await;
        self.run(f).await
    }
}

/// Current on-disk layout version stored in `MetaRecord::schema`.
///
/// - 0: rouchdb <= 0.4 (attachment bytes keyed by `doc_id\0name`).
/// - 1: attachment bytes keyed by digest (content-addressed, shared).
/// - 2: document counts maintained in the metadata record.
const SCHEMA_VERSION: u32 = 2;

const META_KEY: &str = "meta";
const SECURITY_KEY: &str = "security";

impl MetaRecord {
    fn new() -> Self {
        MetaRecord {
            update_seq: 0,
            db_uuid: Uuid::new_v4().to_string(),
            schema: SCHEMA_VERSION,
            purge_seq: 0,
            doc_count: 0,
            doc_del_count: 0,
        }
    }

    /// Account for a document going from `before` to `after`
    /// (`Some(deleted)`, or `None` when it does not exist).
    fn adjust_counts(&mut self, before: Option<bool>, after: Option<bool>) {
        match before {
            Some(true) => self.doc_del_count = self.doc_del_count.saturating_sub(1),
            Some(false) => self.doc_count = self.doc_count.saturating_sub(1),
            None => {}
        }
        match after {
            Some(true) => self.doc_del_count += 1,
            Some(false) => self.doc_count += 1,
            None => {}
        }
    }
}

fn create_tables(txn: &redb::WriteTransaction) -> Result<()> {
    db_err!(txn.open_table(DOC_TABLE))?;
    db_err!(txn.open_table(REV_DATA_TABLE))?;
    db_err!(txn.open_table(CHANGES_TABLE))?;
    db_err!(txn.open_table(LOCAL_TABLE))?;
    db_err!(txn.open_table(ATTACHMENT_TABLE))?;
    db_err!(txn.open_table(META_TABLE))?;
    Ok(())
}

/// Schema 0 -> 1: attachment bytes were stored under `doc_id\0name` (so a
/// later write of the same name overwrote the bytes older revisions point
/// at). Re-key every entry by its digest, which is what revision metadata
/// references.
fn migrate_attachments_to_digest_keys(txn: &redb::WriteTransaction) -> Result<()> {
    let mut table = db_err!(txn.open_table(ATTACHMENT_TABLE))?;
    let mut legacy_keys = Vec::new();
    for entry in db_err!(table.iter())? {
        let (key, _) = db_err!(entry)?;
        if key.value().contains('\0') {
            legacy_keys.push(key.value().to_string());
        }
    }
    for key in legacy_keys {
        let bytes = db_err!(table.remove(key.as_str()))?.map(|g| g.value().to_vec());
        if let Some(bytes) = bytes {
            let digest = attachment_digest(&bytes);
            let exists = db_err!(table.get(digest.as_str()))?.is_some();
            if !exists {
                db_err!(table.insert(digest.as_str(), bytes.as_slice()))?;
            }
        }
    }
    Ok(())
}

/// Schema 1 -> 2: compute the document counts once.
fn count_documents(txn: &redb::WriteTransaction, meta: &mut MetaRecord) -> Result<()> {
    let table = db_err!(txn.open_table(DOC_TABLE))?;
    meta.doc_count = 0;
    meta.doc_del_count = 0;
    for entry in db_err!(table.iter())? {
        let (_, value) = db_err!(entry)?;
        let (tree, _) = decode_doc_record(value.value())?;
        meta.adjust_counts(None, Some(is_deleted(&tree)));
    }
    Ok(())
}

fn read_meta<T>(table: &T) -> Result<MetaRecord>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    let guard = db_err!(table.get(META_KEY))?
        .ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?;
    Ok(serde_json::from_slice(guard.value())?)
}

fn write_meta(table: &mut redb::Table<&str, &[u8]>, meta: &MetaRecord) -> Result<()> {
    let bytes = serde_json::to_vec(meta)?;
    db_err!(table.insert(META_KEY, bytes.as_slice()))?;
    Ok(())
}

/// Parse (and normalize) a revision string.
fn parse_rev(rev_str: &str) -> Result<(u64, String)> {
    let rev: Revision = rev_str.parse()?;
    Ok((rev.pos, rev.hash))
}

/// Load one revision's stored body and attachment metadata.
fn load_rev_data<T>(table: &T, doc_id: &str, rev: &str) -> Result<Option<RevDataRecord>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    match db_err!(table.get(rev_data_key(doc_id, rev).as_str()))? {
        Some(guard) => Ok(Some(decode_body(guard.value())?)),
        None => Ok(None),
    }
}

/// Load one revision's attachment metadata only (not its body).
fn load_rev_attachments<T>(
    table: &T,
    doc_id: &str,
    rev: &str,
) -> Result<Option<HashMap<String, AttachmentRecord>>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    match db_err!(table.get(rev_data_key(doc_id, rev).as_str()))? {
        Some(guard) => {
            let record: RevAttachmentsRecord = serde_json::from_slice(guard.value())?;
            Ok(Some(record.attachments))
        }
        None => Ok(None),
    }
}

/// Load attachment bytes by digest.
fn load_blob<T>(table: &T, digest: &str) -> Result<Option<Vec<u8>>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    Ok(db_err!(table.get(digest))?.map(|g| g.value().to_vec()))
}

fn records_to_meta(records: &HashMap<String, AttachmentRecord>) -> HashMap<String, AttachmentMeta> {
    records
        .iter()
        .map(|(name, r)| {
            (
                name.clone(),
                AttachmentMeta {
                    content_type: r.content_type.clone(),
                    revpos: r.revpos,
                    digest: r.digest.clone(),
                    length: r.length,
                    stub: true,
                    encoding: r.encoding.clone(),
                    encoded_length: r.encoded_length,
                    data: None,
                },
            )
        })
        .collect()
}

fn meta_to_records(atts: &HashMap<String, AttachmentMeta>) -> HashMap<String, AttachmentRecord> {
    atts.iter()
        .map(|(name, m)| {
            (
                name.clone(),
                AttachmentRecord {
                    content_type: m.content_type.clone(),
                    digest: m.digest.clone(),
                    length: m.length,
                    revpos: m.revpos,
                    encoding: m.encoding.clone(),
                    encoded_length: m.encoded_length,
                },
            )
        })
        .collect()
}

/// Keys of every stored revision body of `doc_id` (`doc_id\0rev`).
///
/// The key range of `doc_id` also holds the keys of longer ids that start
/// with `doc_id\0` (`a\0b\0rev` sorts among the keys of `a`). Revision ids
/// contain no NUL, so a key whose remainder does belongs to such an id and
/// is skipped: compacting or purging `a` must never touch `a\0b`.
fn rev_data_keys<T>(table: &T, doc_id: &str) -> Result<Vec<String>>
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    let start = format!("{}\0", doc_id);
    let end = format!("{}\u{1}", doc_id);
    let mut keys = Vec::new();
    for entry in db_err!(table.range(start.as_str()..end.as_str()))? {
        let (key, _) = db_err!(entry)?;
        let key = key.value();
        if !key[start.len()..].contains('\0') {
            keys.push(key.to_string());
        }
    }
    Ok(keys)
}

/// Delete stored bodies of `doc_id` whose revision is not in `keep`.
fn drop_rev_data_except(
    table: &mut redb::Table<&str, &[u8]>,
    doc_id: &str,
    keep: &std::collections::HashSet<String>,
) -> Result<()> {
    let prefix_len = doc_id.len() + 1;
    for key in rev_data_keys(table, doc_id)? {
        if !keep.contains(&key[prefix_len..]) {
            db_err!(table.remove(key.as_str()))?;
        }
    }
    Ok(())
}

/// All revisions (`pos-hash`) present in a tree.
fn tree_revs(tree: &RevTree) -> std::collections::HashSet<String> {
    let mut revs = std::collections::HashSet::new();
    traverse_rev_tree(tree, |pos, node, _| {
        revs.insert(format!("{}-{}", pos, node.hash));
    });
    revs
}

/// Mark every non-leaf node as `Missing`. Returns whether anything changed.
fn mark_non_leaves_missing(tree: &mut RevTree) -> bool {
    fn walk(node: &mut RevNode, changed: &mut bool) {
        if !node.children.is_empty() {
            if node.status != RevStatus::Missing {
                node.status = RevStatus::Missing;
                *changed = true;
            }
            for child in node.children.iter_mut() {
                walk(child, changed);
            }
        }
    }
    let mut changed = false;
    for path in tree.iter_mut() {
        walk(&mut path.tree, &mut changed);
    }
    changed
}

/// Map a failed attachment edit to the error the attachment APIs return.
fn attachment_edit_error(result: DocResult) -> RouchError {
    match result.error.as_deref() {
        Some("conflict") => RouchError::Conflict,
        Some("not_found") => RouchError::NotFound(result.reason.unwrap_or_default()),
        _ => RouchError::BadRequest(result.reason.unwrap_or_default()),
    }
}

/// Build a document JSON body (`_id`, `_rev`, ...) from a stored revision.
fn stored_doc_json(
    doc_id: &str,
    rev_str: &str,
    rd: RevDataRecord,
    winner_deleted: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let mut obj = match rd.data {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("_id".into(), serde_json::Value::String(doc_id.to_string()));
    obj.insert(
        "_rev".into(),
        serde_json::Value::String(rev_str.to_string()),
    );
    if winner_deleted {
        obj.insert("_deleted".into(), serde_json::Value::Bool(true));
    }
    obj
}

#[async_trait]
impl Adapter for RedbAdapter {
    async fn info(&self) -> Result<DbInfo> {
        self.run(|db| db.info()).await
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        let id = id.to_string();
        self.run(move |db| db.get(&id, opts)).await
    }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        let rev_limit = self.rev_limit;
        self.run_write(move |db| db.bulk_docs(docs, opts, rev_limit))
            .await
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        self.run(move |db| db.all_docs(opts)).await
    }

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        self.run(move |db| db.changes(opts)).await
    }

    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        self.run(move |db| db.revs_diff(revs)).await
    }

    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
        self.run(move |db| db.bulk_get(docs)).await
    }

    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult> {
        let (doc_id, att_id, rev, content_type) = (
            doc_id.to_string(),
            att_id.to_string(),
            rev.to_string(),
            content_type.to_string(),
        );
        let rev_limit = self.rev_limit;
        self.run_write(move |db| {
            db.put_attachment(&doc_id, &att_id, &rev, data, &content_type, rev_limit)
        })
        .await
    }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        let (doc_id, att_id) = (doc_id.to_string(), att_id.to_string());
        self.run(move |db| db.get_attachment(&doc_id, &att_id, opts))
            .await
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        let (doc_id, att_id, rev) = (doc_id.to_string(), att_id.to_string(), rev.to_string());
        let rev_limit = self.rev_limit;
        self.run_write(move |db| db.remove_attachment(&doc_id, &att_id, &rev, rev_limit))
            .await
    }

    async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
        let id = id.to_string();
        self.run(move |db| db.get_local(&id)).await
    }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        let id = id.to_string();
        self.run_write(move |db| db.put_local(&id, doc)).await
    }

    async fn remove_local(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.run_write(move |db| db.remove_local(&id)).await
    }

    async fn compact(&self) -> Result<()> {
        self.run_write(|db| db.compact()).await
    }

    async fn destroy(&self) -> Result<()> {
        self.run_write(|db| db.destroy()).await
    }

    async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        self.run_write(move |db| db.purge(req)).await
    }

    async fn get_security(&self) -> Result<SecurityDocument> {
        self.run(|db| db.get_security()).await
    }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        self.run_write(move |db| db.put_security(doc)).await
    }
}

/// The storage operations, run synchronously (see `RedbAdapter::run`).
impl Inner {
    fn info(&self) -> Result<DbInfo> {
        // Counts and update_seq live in one metadata record, so they always
        // reflect the same committed state without scanning documents.
        let read_txn = db_err!(self.db.begin_read())?;
        let meta = read_meta(&db_err!(read_txn.open_table(META_TABLE))?)?;

        Ok(DbInfo {
            db_name: self.name.clone(),
            doc_count: meta.doc_count,
            doc_del_count: meta.doc_del_count,
            update_seq: Seq::Num(meta.update_seq),
        })
    }

    fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        if opts.open_revs.is_some() {
            return Err(RouchError::BadRequest(
                "open_revs is not supported by get(); use bulk_get".into(),
            ));
        }

        let requested = opts.rev.as_deref().map(canonical_rev).transpose()?;
        let read_txn = db_err!(self.db.begin_read())?;
        if let Some(local) = local_doc_id(id) {
            let table = db_err!(read_txn.open_table(LOCAL_TABLE))?;
            return match db_err!(table.get(local))? {
                Some(guard) => Ok(local_document(local, decode_body(guard.value())?)),
                None => Err(RouchError::NotFound("missing".into())),
            };
        }
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let (tree, _) =
            load_doc_record(&doc_table, id)?.ok_or_else(|| RouchError::NotFound(id.to_string()))?;

        let mut target_rev = if let Some(rev_str) = requested {
            rev_str
        } else {
            winning_rev(&tree)
                .ok_or_else(|| RouchError::NotFound(id.to_string()))?
                .to_string()
        };

        // latest: walk the requested rev's own branch down to its leaf.
        if opts.latest
            && opts.rev.is_some()
            && let Ok((pos, hash)) = parse_rev(&target_rev)
            && let Some(rev) = latest_leaf(&tree, pos, &hash)
        {
            target_rev = rev.to_string();
        }

        // An unknown, stemmed, compacted or otherwise body-less revision is
        // missing, never an empty document.
        if !in_tree(&tree, &target_rev) {
            return Err(RouchError::NotFound("missing".into()));
        }
        let rd = load_rev_data(&rev_table, id, &target_rev)?
            .ok_or_else(|| RouchError::NotFound("missing".into()))?;

        if rd.deleted && opts.rev.is_none() {
            return Err(RouchError::NotFound(id.to_string()));
        }

        let (pos, hash) = parse_rev(&target_rev)?;

        let mut doc = Document {
            id: id.to_string(),
            rev: Some(Revision::new(pos, hash.clone())),
            deleted: rd.deleted,
            data: rd.data,
            attachments: records_to_meta(&rd.attachments),
        };

        // Inline the attachment bytes only when explicitly requested.
        if opts.attachments && !doc.attachments.is_empty() {
            let att_table = db_err!(read_txn.open_table(ATTACHMENT_TABLE))?;
            for meta in doc.attachments.values_mut() {
                meta.data = load_blob(&att_table, &meta.digest)?;
                meta.stub = meta.data.is_none();
            }
        }

        if let serde_json::Value::Object(ref mut map) = doc.data {
            if opts.conflicts {
                let conflicts = collect_conflicts(&tree);
                if !conflicts.is_empty() {
                    let conflict_list: Vec<serde_json::Value> = conflicts
                        .iter()
                        .map(|c| serde_json::Value::String(c.to_string()))
                        .collect();
                    map.insert("_conflicts".into(), serde_json::Value::Array(conflict_list));
                }
            }
            if opts.revs
                && let Some(ids) = find_rev_ancestry(&tree, pos, &hash)
            {
                map.insert(
                    "_revisions".into(),
                    serde_json::json!({"start": pos, "ids": ids}),
                );
            }
            if opts.revs_info
                && let Some(info) = revs_info(&tree, pos, &hash)
            {
                map.insert("_revs_info".into(), serde_json::to_value(&info)?);
            }
        }

        Ok(doc)
    }

    fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
        rev_limit: u64,
    ) -> Result<Vec<DocResult>> {
        let write_txn = db_err!(self.db.begin_write())?;

        let mut results = Vec::with_capacity(docs.len());

        // Read current metadata
        let mut meta = read_meta(&db_err!(write_txn.open_table(META_TABLE))?)?;

        {
            let mut tables = WriteTables::open(&write_txn)?;
            for doc in docs {
                let result = if opts.new_edits {
                    write_new_edit(&mut tables, &mut meta, doc, rev_limit)?
                } else {
                    write_replicated(&mut tables, &mut meta, doc, rev_limit)?
                };
                results.push(result);
            }
        }

        // Write updated metadata
        write_meta(&mut db_err!(write_txn.open_table(META_TABLE))?, &meta)?;

        db_err!(write_txn.commit())?;

        Ok(results)
    }

    fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let mut rows = Vec::new();
        let meta = read_meta(&db_err!(read_txn.open_table(META_TABLE))?)?;
        let skip = opts.skip as usize;
        let limit = opts.limit.map(|l| l as usize).unwrap_or(usize::MAX);

        // Build one row for a document, or None for a deleted one that is not
        // explicitly requested.
        let make_row = |doc_id: &str, tree: &RevTree, by_key: bool| -> Result<Option<AllDocsRow>> {
            let winner = match winning_rev(tree) {
                Some(w) => w,
                None => return Ok(None),
            };
            let deleted = is_deleted(tree);
            if deleted && !by_key {
                return Ok(None);
            }
            let doc_json = if opts.include_docs && !deleted {
                let rev_str = winner.to_string();
                match load_rev_data(&rev_table, doc_id, &rev_str)? {
                    Some(rd) => {
                        let mut obj = stored_doc_json(doc_id, &rev_str, rd, false);
                        // Embed _conflicts when requested, matching the memory
                        // adapter and CouchDB.
                        if opts.conflicts {
                            let conflicts = collect_conflicts(tree);
                            if !conflicts.is_empty() {
                                let conflict_list: Vec<serde_json::Value> = conflicts
                                    .iter()
                                    .map(|c| serde_json::Value::String(c.to_string()))
                                    .collect();
                                obj.insert(
                                    "_conflicts".into(),
                                    serde_json::Value::Array(conflict_list),
                                );
                            }
                        }
                        Some(serde_json::Value::Object(obj))
                    }
                    None => None,
                }
            } else {
                None
            };
            Ok(Some(AllDocsRow {
                doc: doc_json,
                ..AllDocsRow::document(
                    doc_id,
                    AllDocsRowValue {
                        rev: winner.to_string(),
                        deleted: deleted.then_some(true),
                    },
                )
            }))
        };

        if let Some(ref keys) = opts.keys {
            // One row per requested key, in request order (reversed for
            // descending), duplicates included; an unknown key gets a
            // `not_found` row (CouchDB).
            let ordered: Vec<&String> = if opts.descending {
                keys.iter().rev().collect()
            } else {
                keys.iter().collect()
            };
            for key in ordered {
                let row = match load_doc_record(&doc_table, key)? {
                    Some((tree, _)) => make_row(key, &tree, true)?,
                    None => None,
                };
                rows.push(row.unwrap_or_else(|| AllDocsRow::not_found(key.as_str())));
            }
            rows = rows.into_iter().skip(skip).take(limit).collect();
        } else if let Some(ref key) = opts.key {
            // A single key is a direct lookup; a deleted doc yields no row.
            if skip == 0
                && limit > 0
                && let Some((tree, _)) = load_doc_record(&doc_table, key)?
                && let Some(row) = make_row(key, &tree, false)?
            {
                rows.push(row);
            }
        } else {
            // Key range scan in the requested direction, stopping as soon as
            // the page is full. Descending swaps the meaning of start/end.
            use std::ops::Bound;
            let (low, high) = if opts.descending {
                (opts.end_key.as_deref(), opts.start_key.as_deref())
            } else {
                (opts.start_key.as_deref(), opts.end_key.as_deref())
            };
            let lower = match low {
                None => Bound::Unbounded,
                Some(k) if opts.descending && !opts.inclusive_end => Bound::Excluded(k),
                Some(k) => Bound::Included(k),
            };
            let upper = match high {
                None => Bound::Unbounded,
                Some(k) if !opts.descending && !opts.inclusive_end => Bound::Excluded(k),
                Some(k) => Bound::Included(k),
            };
            // An inverted range matches nothing.
            let inverted = matches!((low, high), (Some(l), Some(h)) if l > h);
            if !inverted && limit > 0 {
                let range = db_err!(doc_table.range::<&str>((lower, upper)))?;
                let iter: Box<dyn Iterator<Item = _>> = if opts.descending {
                    Box::new(range.rev())
                } else {
                    Box::new(range)
                };
                let mut skipped = 0usize;
                for entry in iter {
                    let (key, value) = db_err!(entry)?;
                    let (tree, _) = decode_doc_record(value.value())?;
                    if let Some(row) = make_row(key.value(), &tree, false)? {
                        if skipped < skip {
                            skipped += 1;
                            continue;
                        }
                        rows.push(row);
                        if rows.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }

        // total_rows is the count of non-deleted documents in the whole
        // database, independent of any range / key / skip / limit filters.
        let total_rows = meta.doc_count;

        // Read update_seq from the SAME read transaction as the doc snapshot
        // to avoid a TOCTOU inconsistency with a concurrent committed write.
        let update_seq = if opts.update_seq {
            Some(Seq::Num(meta.update_seq))
        } else {
            None
        };

        Ok(AllDocsResponse {
            total_rows,
            offset: opts.skip,
            rows,
            update_seq,
        })
    }

    fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        // `limit: 0` is no change at all (CouchDB): the feed stays at `since`,
        // or at the current sequence when descending.
        if opts.limit == Some(0) {
            let last_seq = if opts.descending {
                Seq::Num(read_meta(&db_err!(read_txn.open_table(META_TABLE))?)?.update_seq)
            } else {
                opts.since.clone()
            };
            return Ok(ChangesResponse {
                results: Vec::new(),
                last_seq,
            });
        }
        let changes_table = db_err!(read_txn.open_table(CHANGES_TABLE))?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let mut results = Vec::new();

        let start = opts.since.as_num().saturating_add(1);
        let range = db_err!(changes_table.range(start..))?;
        // Iterate lazily (in either direction) so `limit` stops the scan.
        let iter: Box<dyn Iterator<Item = _>> = if opts.descending {
            Box::new(range.rev())
        } else {
            Box::new(range)
        };

        // Highest sequence inspected, so last_seq advances past a fully
        // filtered range instead of sticking at `since`.
        let mut max_scanned: Option<u64> = None;

        for entry in iter {
            // Propagate deserialization errors instead of panicking on a
            // corrupt or truncated change record.
            let entry = db_err!(entry)?;
            let seq = &entry.0.value();
            let change: ChangeRecord = serde_json::from_slice(entry.1.value())?;
            max_scanned = Some(max_scanned.map_or(*seq, |m| m.max(*seq)));

            if let Some(ref doc_ids) = opts.doc_ids
                && !doc_ids.contains(&change.doc_id)
            {
                continue;
            }

            let tree = load_doc_record(&doc_table, change.doc_id.as_str())?.map(|(t, _)| t);
            let rev_str = tree
                .as_ref()
                .and_then(winning_rev)
                .map(|r| r.to_string())
                .unwrap_or_default();

            let doc = if opts.include_docs && !rev_str.is_empty() {
                load_rev_data(&rev_table, &change.doc_id, &rev_str)?.map(|rd| {
                    serde_json::Value::Object(stored_doc_json(
                        &change.doc_id,
                        &rev_str,
                        rd,
                        change.deleted,
                    ))
                })
            } else {
                None
            };

            // Build changes list based on style
            let changes_list = match (&opts.style, &tree) {
                // All leaf revisions, including deleted ones.
                (ChangesStyle::AllDocs, Some(tree)) => collect_leaves(tree)
                    .iter()
                    .map(|l| ChangeRev {
                        rev: l.rev_string(),
                    })
                    .collect(),
                _ => vec![ChangeRev { rev: rev_str }],
            };

            // Collect conflicts if requested
            let conflicts = match (&tree, opts.conflicts) {
                (Some(tree), true) => {
                    let c = collect_conflicts(tree);
                    if c.is_empty() {
                        None
                    } else {
                        Some(c.iter().map(|r| r.to_string()).collect())
                    }
                }
                _ => None,
            };

            results.push(ChangeEvent {
                seq: Seq::Num(*seq),
                id: change.doc_id.clone(),
                changes: changes_list,
                deleted: change.deleted,
                doc,
                conflicts,
            });

            if let Some(limit) = opts.limit
                && results.len() >= limit as usize
            {
                break;
            }
        }

        let last_seq = results
            .last()
            .map(|r| r.seq.clone())
            .or_else(|| max_scanned.map(Seq::Num))
            .unwrap_or(opts.since.clone());

        Ok(ChangesResponse { results, last_seq })
    }

    fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;

        let mut results = HashMap::new();

        for (doc_id, rev_list) in revs {
            let tree = load_doc_record(&doc_table, doc_id.as_str())?.map(|(t, _)| t);
            if let Some(diff) = revs_diff_one(tree.as_ref(), &rev_list)? {
                results.insert(doc_id, diff);
            }
        }

        Ok(RevsDiffResponse { results })
    }

    fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;
        let att_table = db_err!(read_txn.open_table(ATTACHMENT_TABLE))?;

        let mut results = Vec::new();

        for item in docs {
            let not_found = |rev: String| BulkGetDoc {
                ok: None,
                error: Some(BulkGetError {
                    id: item.id.clone(),
                    rev,
                    error: "not_found".into(),
                    reason: "missing".into(),
                }),
            };

            let tree = load_doc_record(&doc_table, item.id.as_str())?.map(|(t, _)| t);
            let rev_str = match (&item.rev, &tree) {
                (Some(rev), _) => Some(canonical_rev(rev).unwrap_or_else(|_| rev.clone())),
                (None, Some(tree)) => winning_rev(tree).map(|w| w.to_string()),
                (None, None) => None,
            };

            let found = match (&tree, &rev_str) {
                (Some(tree), Some(rev_str)) if in_tree(tree, rev_str) => {
                    load_rev_data(&rev_table, &item.id, rev_str)?
                        .map(|rd| (tree, rev_str.clone(), rd))
                }
                _ => None,
            };

            let bulk_doc = match found {
                Some((tree, rev_str, rd)) => {
                    let deleted = rd.deleted;
                    let atts = rd.attachments.clone();
                    let mut obj = stored_doc_json(&item.id, &rev_str, rd, deleted);

                    // Include _revisions for replication
                    if let Ok((pos, ref hash)) = parse_rev(&rev_str)
                        && let Some(ancestry) = find_rev_ancestry(tree, pos, hash)
                    {
                        obj.insert(
                            "_revisions".into(),
                            serde_json::json!({
                                "start": pos,
                                "ids": ancestry
                            }),
                        );
                    }

                    // Include inline attachments so replication carries
                    // their bytes end-to-end.
                    if !atts.is_empty() {
                        let mut att_map = serde_json::Map::new();
                        for (name, meta) in records_to_meta(&atts) {
                            let bytes = load_blob(&att_table, &meta.digest)?;
                            att_map.insert(name, meta.to_json(bytes.as_deref()));
                        }
                        obj.insert("_attachments".into(), serde_json::Value::Object(att_map));
                    }

                    BulkGetDoc {
                        ok: Some(serde_json::Value::Object(obj)),
                        error: None,
                    }
                }
                None => not_found(rev_str.unwrap_or_default()),
            };

            results.push(BulkGetResult {
                id: item.id.clone(),
                docs: vec![bulk_doc],
            });
        }

        Ok(BulkGetResponse { results })
    }

    fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
        rev_limit: u64,
    ) -> Result<DocResult> {
        let write_txn = db_err!(self.db.begin_write())?;
        let mut meta = read_meta(&db_err!(write_txn.open_table(META_TABLE))?)?;

        let result = {
            let mut tables = WriteTables::open(&write_txn)?;
            let (tree, seq) = load_doc_record(&tables.docs, doc_id)?
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            let parent: Revision = rev.parse()?;
            let rev = parent.to_string();

            // The new revision builds on `rev` (any leaf, not only the
            // winner): its body plus its attachments with this one added.
            let rd = load_rev_data(&tables.revs, doc_id, &rev)?.ok_or(RouchError::Conflict)?;
            let parent_atts = records_to_meta(&rd.attachments);
            let mut attachments = parent_atts.clone();
            attachments.insert(att_id.to_string(), AttachmentMeta::new(content_type, data));
            let doc = Document {
                id: doc_id.to_string(),
                rev: Some(parent),
                deleted: false,
                data: rd.data,
                attachments,
            };
            let plan = plan_new_edit(Some(&tree), doc, Some(&parent_atts), false, rev_limit)
                .map_err(attachment_edit_error)?;
            apply_write(&mut tables, &mut meta, Some((&tree, seq)), plan)?
        };

        write_meta(&mut db_err!(write_txn.open_table(META_TABLE))?, &meta)?;
        db_err!(write_txn.commit())?;
        Ok(result)
    }

    fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;
        let att_table = db_err!(read_txn.open_table(ATTACHMENT_TABLE))?;

        let (tree, _) = load_doc_record(&doc_table, doc_id)?
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        let rev_str = if let Some(ref rev) = opts.rev {
            canonical_rev(rev)?
        } else {
            winning_rev(&tree)
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?
                .to_string()
        };

        // Resolve this revision's attachment metadata, then return the bytes
        // stored under its digest.
        let not_found = || RouchError::NotFound(format!("attachment {}/{}", doc_id, att_id));
        if !in_tree(&tree, &rev_str) {
            return Err(not_found());
        }
        let atts = load_rev_attachments(&rev_table, doc_id, &rev_str)?.ok_or_else(not_found)?;
        let rec = atts.get(att_id).ok_or_else(not_found)?;
        load_blob(&att_table, &rec.digest)?.ok_or_else(not_found)
    }

    fn remove_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        rev_limit: u64,
    ) -> Result<DocResult> {
        let write_txn = db_err!(self.db.begin_write())?;
        let mut meta = read_meta(&db_err!(write_txn.open_table(META_TABLE))?)?;

        let result = {
            let mut tables = WriteTables::open(&write_txn)?;
            let (tree, seq) = load_doc_record(&tables.docs, doc_id)?
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            let parent: Revision = rev.parse()?;
            let rev = parent.to_string();

            let rd = load_rev_data(&tables.revs, doc_id, &rev)?.ok_or(RouchError::Conflict)?;
            if !rd.attachments.contains_key(att_id) {
                return Err(RouchError::NotFound(format!(
                    "attachment {}/{}",
                    doc_id, att_id
                )));
            }
            let parent_atts = records_to_meta(&rd.attachments);
            let mut attachments = parent_atts.clone();
            attachments.remove(att_id);

            // A new revision whose attachment set is exactly the remaining
            // ones. The bytes stay stored for older revisions (compaction
            // drops them once unreferenced).
            let doc = Document {
                id: doc_id.to_string(),
                rev: Some(parent),
                deleted: false,
                data: rd.data,
                attachments,
            };
            let plan = plan_new_edit(Some(&tree), doc, Some(&parent_atts), false, rev_limit)
                .map_err(attachment_edit_error)?;
            apply_write(&mut tables, &mut meta, Some((&tree, seq)), plan)?
        };

        write_meta(&mut db_err!(write_txn.open_table(META_TABLE))?, &meta)?;
        db_err!(write_txn.commit())?;
        Ok(result)
    }

    fn get_local(&self, id: &str) -> Result<serde_json::Value> {
        let read_txn = db_err!(self.db.begin_read())?;
        let table = db_err!(read_txn.open_table(LOCAL_TABLE))?;
        let guard = db_err!(table.get(id))?
            .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))?;
        decode_body(guard.value())
    }

    fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        rouchdb_core::json::check_document_depth(&doc)?;
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut table = db_err!(write_txn.open_table(LOCAL_TABLE))?;
            let bytes = serde_json::to_vec(&doc)?;
            db_err!(table.insert(id, bytes.as_slice()))?;
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }

    fn remove_local(&self, id: &str) -> Result<()> {
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut table = db_err!(write_txn.open_table(LOCAL_TABLE))?;
            db_err!(table.remove(id))?
                .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))?;
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }

    fn compact(&self) -> Result<()> {
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut doc_table = db_err!(write_txn.open_table(DOC_TABLE))?;
            let mut rev_table = db_err!(write_txn.open_table(REV_DATA_TABLE))?;
            let mut att_table = db_err!(write_txn.open_table(ATTACHMENT_TABLE))?;

            let mut docs = Vec::new();
            for entry in db_err!(doc_table.iter())? {
                let (key, value) = db_err!(entry)?;
                let (tree, seq) = decode_doc_record(value.value())?;
                docs.push((key.value().to_string(), tree, seq));
            }

            // Keep only leaf bodies (this also drops bodies of revisions that
            // were stemmed out of the tree) and mark the rest missing.
            for (doc_id, mut tree, seq) in docs {
                let leaves: std::collections::HashSet<String> = collect_leaves(&tree)
                    .iter()
                    .map(|l| l.rev_string())
                    .collect();
                drop_rev_data_except(&mut rev_table, &doc_id, &leaves)?;
                if mark_non_leaves_missing(&mut tree) {
                    let bytes = encode_doc_record(&tree, seq)?;
                    db_err!(doc_table.insert(doc_id.as_str(), bytes.as_slice()))?;
                }
            }

            // Drop attachment bytes no remaining revision references.
            let mut referenced = std::collections::HashSet::new();
            for entry in db_err!(rev_table.iter())? {
                let (_, value) = db_err!(entry)?;
                let rd: RevAttachmentsRecord = serde_json::from_slice(value.value())?;
                referenced.extend(rd.attachments.into_values().map(|a| a.digest));
            }
            let mut unreferenced = Vec::new();
            for entry in db_err!(att_table.iter())? {
                let (key, _) = db_err!(entry)?;
                if !referenced.contains(key.value()) {
                    unreferenced.push(key.value().to_string());
                }
            }
            for digest in unreferenced {
                db_err!(att_table.remove(digest.as_str()))?;
            }
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }

    fn destroy(&self) -> Result<()> {
        let write_txn = db_err!(self.db.begin_write())?;

        // Delete all tables in O(1) instead of draining entries one by one.
        let _ = db_err!(write_txn.delete_table(DOC_TABLE))?;
        let _ = db_err!(write_txn.delete_table(REV_DATA_TABLE))?;
        let _ = db_err!(write_txn.delete_table(CHANGES_TABLE))?;
        let _ = db_err!(write_txn.delete_table(LOCAL_TABLE))?;
        let _ = db_err!(write_txn.delete_table(ATTACHMENT_TABLE))?;
        let _ = db_err!(write_txn.delete_table(META_TABLE))?;

        // Recreate empty tables so subsequent operations don't fail, and
        // reset the metadata (including the security document).
        create_tables(&write_txn)?;
        write_meta(
            &mut db_err!(write_txn.open_table(META_TABLE))?,
            &MetaRecord::new(),
        )?;

        db_err!(write_txn.commit())?;
        Ok(())
    }

    fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        let write_txn = db_err!(self.db.begin_write())?;
        let mut meta = read_meta(&db_err!(write_txn.open_table(META_TABLE))?)?;
        let mut purged = HashMap::new();
        let mut bumped = false;

        {
            let mut tables = WriteTables::open(&write_txn)?;
            for (doc_id, revs) in req {
                let revs = revs
                    .iter()
                    .map(|r| canonical_rev(r))
                    .collect::<Result<Vec<_>>>()?;
                let Some((tree, old_seq)) = load_doc_record(&tables.docs, &doc_id)? else {
                    continue;
                };
                // Only leaves can be purged; their ancestors go too unless
                // another leaf still needs them. Nothing older is resurrected.
                let (new_tree, removed) = remove_leaves(&tree, &revs);
                if removed.is_empty() {
                    purged.insert(doc_id, removed);
                    continue;
                }
                db_err!(tables.changes.remove(old_seq))?;
                drop_rev_data_except(&mut tables.revs, &doc_id, &tree_revs(&new_tree))?;
                meta.adjust_counts(
                    Some(is_deleted(&tree)),
                    (!new_tree.is_empty()).then(|| is_deleted(&new_tree)),
                );

                if new_tree.is_empty() {
                    db_err!(tables.docs.remove(doc_id.as_str()))?;
                } else {
                    // The winner may have changed: record the document again.
                    meta.update_seq += 1;
                    bumped = true;
                    let seq = meta.update_seq;
                    let bytes = encode_doc_record(&new_tree, seq)?;
                    db_err!(tables.docs.insert(doc_id.as_str(), bytes.as_slice()))?;
                    let change = serde_json::to_vec(&ChangeRecord {
                        doc_id: doc_id.clone(),
                        deleted: is_deleted(&new_tree),
                    })?;
                    db_err!(tables.changes.insert(seq, change.as_slice()))?;
                }
                purged.insert(doc_id, removed);
            }
        }

        // A purge is a database update even when no document keeps a change
        // entry (CouchDB bumps update_seq per purge request).
        if !bumped {
            meta.update_seq += 1;
        }
        meta.purge_seq += 1;
        write_meta(&mut db_err!(write_txn.open_table(META_TABLE))?, &meta)?;
        db_err!(write_txn.commit())?;

        Ok(PurgeResponse {
            purge_seq: Some(meta.purge_seq),
            purged,
        })
    }

    fn get_security(&self) -> Result<SecurityDocument> {
        let read_txn = db_err!(self.db.begin_read())?;
        let table = db_err!(read_txn.open_table(META_TABLE))?;
        match db_err!(table.get(SECURITY_KEY))? {
            Some(guard) => Ok(serde_json::from_slice(guard.value())?),
            None => Ok(SecurityDocument::default()),
        }
    }

    fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut table = db_err!(write_txn.open_table(META_TABLE))?;
            let bytes = serde_json::to_vec(&doc)?;
            db_err!(table.insert(SECURITY_KEY, bytes.as_slice()))?;
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Document processing (shared by bulk_docs and the attachment APIs)
// ---------------------------------------------------------------------------

/// The tables a document write touches, opened once per write transaction.
struct WriteTables<'txn> {
    docs: redb::Table<'txn, &'static str, &'static [u8]>,
    revs: redb::Table<'txn, &'static str, &'static [u8]>,
    changes: redb::Table<'txn, u64, &'static [u8]>,
    atts: redb::Table<'txn, &'static str, &'static [u8]>,
    locals: redb::Table<'txn, &'static str, &'static [u8]>,
}

impl<'txn> WriteTables<'txn> {
    fn open(txn: &'txn redb::WriteTransaction) -> Result<Self> {
        Ok(WriteTables {
            docs: db_err!(txn.open_table(DOC_TABLE))?,
            revs: db_err!(txn.open_table(REV_DATA_TABLE))?,
            changes: db_err!(txn.open_table(CHANGES_TABLE))?,
            atts: db_err!(txn.open_table(ATTACHMENT_TABLE))?,
            locals: db_err!(txn.open_table(LOCAL_TABLE))?,
        })
    }
}

/// Apply one `new_edits=true` write. The edit rules (conflicts, attachment
/// inheritance, revision hashing) live in `rouchdb_core::write` so this
/// adapter behaves exactly like the others; this only loads the inputs and
/// stores the plan.
fn write_new_edit(
    tables: &mut WriteTables,
    meta: &mut MetaRecord,
    mut doc: Document,
    rev_limit: u64,
) -> Result<DocResult> {
    if let Err(e) = doc.prepare_for_write() {
        return Ok(error_result(&doc.id, "bad_request", &e.to_string()));
    }
    if doc.id.is_empty() {
        doc.id = Uuid::new_v4().to_string();
    }
    if local_doc_id(&doc.id).is_some() {
        return match plan_local_write(doc) {
            Ok(LocalWrite::Put { id, body, result }) => {
                let bytes = serde_json::to_vec(&body)?;
                db_err!(tables.locals.insert(id.as_str(), bytes.as_slice()))?;
                Ok(result)
            }
            Ok(LocalWrite::Delete { id, result }) => {
                db_err!(tables.locals.remove(id.as_str()))?;
                Ok(result)
            }
            Err(result) => Ok(result),
        };
    }

    // A decoding error aborts the batch instead of being treated as a
    // missing document.
    let existing = load_doc_record(&tables.docs, &doc.id)?;
    let tree = existing.as_ref().map(|(t, _)| t);
    let parent_atts = match edit_parent(tree, &doc) {
        Some(parent) => load_rev_attachments(&tables.revs, &doc.id, &parent.to_string())?
            .map(|atts| records_to_meta(&atts)),
        None => None,
    };

    match plan_new_edit(tree, doc, parent_atts.as_ref(), true, rev_limit) {
        Ok(plan) => apply_write(tables, meta, existing.as_ref().map(|(t, s)| (t, *s)), plan),
        Err(result) => Ok(result),
    }
}

/// Apply one replicated (`new_edits=false`) write.
fn write_replicated(
    tables: &mut WriteTables,
    meta: &mut MetaRecord,
    doc: Document,
    rev_limit: u64,
) -> Result<DocResult> {
    // CouchDB ignores `_local/` documents in replicated writes (they are
    // never replicated): nothing is stored.
    if local_doc_id(&doc.id).is_some() {
        return Ok(DocResult {
            ok: true,
            id: doc.id,
            rev: doc.rev.map(|r| r.to_string()),
            error: None,
            reason: None,
        });
    }
    let existing = load_doc_record(&tables.docs, &doc.id)?;
    let has_body = match (&existing, &doc.rev) {
        (Some(_), Some(rev)) => {
            let key = rev_data_key(&doc.id, &rev.clone().normalized().to_string());
            db_err!(tables.revs.get(key.as_str()))?.is_some()
        }
        _ => false,
    };

    let plan =
        match plan_replicated_edit(existing.as_ref().map(|(t, _)| t), doc, has_body, rev_limit) {
            Ok(ReplicatedWrite::Write(plan)) => *plan,
            Ok(ReplicatedWrite::AlreadyStored(result)) => return Ok(result),
            Err(result) => return Ok(result),
        };

    // Stubs must refer to bytes we already hold.
    for digest in &plan.required_blobs {
        if db_err!(tables.atts.get(digest.as_str()))?.is_none() {
            return Ok(error_result(
                &plan.id,
                "missing_stub",
                &format!("Invalid attachment stub in {} for {}", plan.id, digest),
            ));
        }
    }

    apply_write(tables, meta, existing.as_ref().map(|(t, s)| (t, *s)), plan)
}

/// Persist a planned write: attachment bytes (by digest), the revision tree,
/// the revision's body and attachment metadata, and a new sequence entry.
fn apply_write(
    tables: &mut WriteTables,
    meta: &mut MetaRecord,
    existing: Option<(&RevTree, u64)>,
    plan: PlannedWrite,
) -> Result<DocResult> {
    let old_seq = existing.map(|(_, seq)| seq);
    meta.adjust_counts(
        existing.map(|(tree, _)| is_deleted(tree)),
        Some(plan.doc_deleted),
    );

    for (digest, bytes) in &plan.new_blobs {
        if db_err!(tables.atts.get(digest.as_str()))?.is_none() {
            db_err!(tables.atts.insert(digest.as_str(), bytes.as_slice()))?;
        }
    }

    meta.update_seq += 1;
    let seq = meta.update_seq;

    // Each document keeps a single entry in the changes table.
    if let Some(old_seq) = old_seq {
        db_err!(tables.changes.remove(old_seq))?;
    }

    let doc_bytes = encode_doc_record(&plan.tree, seq)?;
    db_err!(tables.docs.insert(plan.id.as_str(), doc_bytes.as_slice()))?;

    // Stemmed revisions no longer exist: drop their bodies.
    for rev in &plan.stemmed {
        let key = rev_data_key(&plan.id, &rev.to_string());
        db_err!(tables.revs.remove(key.as_str()))?;
    }

    let rd = RevDataRecord {
        data: plan.data,
        deleted: plan.deleted,
        attachments: meta_to_records(&plan.attachments),
    };
    let rev_bytes = serde_json::to_vec(&rd)?;
    let key = rev_data_key(&plan.id, &plan.rev.to_string());
    db_err!(tables.revs.insert(key.as_str(), rev_bytes.as_slice()))?;

    // The feed reports whether the document (its winner) is deleted, not
    // whether this particular edit was a deletion.
    let change = ChangeRecord {
        doc_id: plan.id.clone(),
        deleted: plan.doc_deleted,
    };
    let change_bytes = serde_json::to_vec(&change)?;
    db_err!(tables.changes.insert(seq, change_bytes.as_slice()))?;

    Ok(ok_result(&plan.id, &plan.rev))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_core::document::{AllDocsOptions, BulkDocsOptions, ChangesOptions, GetOptions};
    use rouchdb_core::rev_tree::build_path_from_revs;

    fn temp_db() -> (tempfile::TempDir, RedbAdapter) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let adapter = RedbAdapter::open(&path, "test").unwrap();
        (dir, adapter)
    }

    #[tokio::test]
    async fn info_empty() {
        let (_dir, db) = temp_db();
        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 0);
        assert_eq!(info.update_seq, Seq::Num(0));
    }

    #[tokio::test]
    async fn all_docs_descending_with_range() {
        let (_dir, db) = temp_db();
        for name in ["a", "b", "c", "d"] {
            db.bulk_docs(
                vec![Document {
                    id: name.into(),
                    rev: None,
                    deleted: false,
                    data: serde_json::json!({}),
                    attachments: HashMap::new(),
                }],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        }

        // descending flips startkey/endkey: startkey="c" is the upper bound,
        // endkey="b" the lower bound -> expect ["c", "b"].
        let opts = AllDocsOptions {
            descending: true,
            start_key: Some("c".into()),
            end_key: Some("b".into()),
            ..AllDocsOptions::new()
        };
        let result = db.all_docs(opts).await.unwrap();
        let ids: Vec<&str> = result.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(ids, vec!["c", "b"]);
        assert_eq!(result.total_rows, 4);
    }

    #[tokio::test]
    async fn all_docs_includes_conflicts() {
        let (_dir, db) = temp_db();
        // Two conflicting leaves on the same document via replication.
        for hash in ["bbb", "ccc"] {
            let mut data = serde_json::json!({ "v": hash });
            data.as_object_mut().unwrap().insert(
                "_revisions".into(),
                serde_json::json!({"start": 2, "ids": [hash, "aaa"]}),
            );
            db.bulk_docs(
                vec![Document {
                    id: "doc1".into(),
                    rev: Some(Revision::new(2, hash.into())),
                    deleted: false,
                    data,
                    attachments: HashMap::new(),
                }],
                BulkDocsOptions::replication(),
            )
            .await
            .unwrap();
        }

        let opts = AllDocsOptions {
            include_docs: true,
            conflicts: true,
            ..AllDocsOptions::new()
        };
        let result = db.all_docs(opts).await.unwrap();
        let doc = result.rows[0].doc.as_ref().unwrap();
        let conflicts = doc["_conflicts"].as_array().expect("_conflicts present");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0], "2-bbb"); // loser (ccc wins by higher hash)
    }

    #[tokio::test]
    async fn local_docs() {
        let (_dir, db) = temp_db();

        db.put_local("ck1", serde_json::json!({"seq": 5}))
            .await
            .unwrap();
        let fetched = db.get_local("ck1").await.unwrap();
        assert_eq!(fetched["seq"], 5);

        db.remove_local("ck1").await.unwrap();
        assert!(matches!(
            db.get_local("ck1").await,
            Err(RouchError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn destroy_clears_all() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let rev = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();
        db.put_attachment("doc1", "a", &rev, b"bytes".to_vec(), "text/plain")
            .await
            .unwrap();
        db.put_local("x", serde_json::json!({})).await.unwrap();
        db.put_security(SecurityDocument {
            admins: rouchdb_core::document::SecurityGroup {
                names: vec!["bob".into()],
                roles: vec![],
            },
            ..Default::default()
        })
        .await
        .unwrap();

        db.destroy().await.unwrap();
        let info = db.info().await.unwrap();
        assert_eq!((info.doc_count, info.doc_del_count), (0, 0));
        assert_eq!(info.update_seq, Seq::Num(0));
        assert!(matches!(
            db.get("doc1", GetOptions::default()).await,
            Err(RouchError::NotFound(_))
        ));
        assert!(matches!(
            db.get_local("x").await,
            Err(RouchError::NotFound(_))
        ));
        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert!(changes.results.is_empty());
        assert!(db.get_security().await.unwrap().admins.names.is_empty());
        // Every table is empty again, including the attachment store.
        use redb::ReadableTableMetadata;
        let txn = db.inner.db.begin_read().unwrap();
        for table in [DOC_TABLE, REV_DATA_TABLE, LOCAL_TABLE, ATTACHMENT_TABLE] {
            assert!(txn.open_table(table).unwrap().is_empty().unwrap());
        }
        assert!(txn.open_table(CHANGES_TABLE).unwrap().is_empty().unwrap());
    }

    #[tokio::test]
    async fn all_docs_include_docs() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "Alice"}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        let result = db
            .all_docs(AllDocsOptions {
                include_docs: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        let doc_json = result.rows[0].doc.as_ref().unwrap();
        assert_eq!(doc_json["name"], "Alice");
        assert_eq!(doc_json["_id"], "doc1");
    }

    #[tokio::test]
    async fn changes_include_docs() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"val": 42}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        let changes = db
            .changes(ChangesOptions {
                include_docs: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(changes.results.len(), 1);
        let doc_json = changes.results[0].doc.as_ref().unwrap();
        assert_eq!(doc_json["val"], 42);
        assert_eq!(doc_json["_id"], "doc1");
    }

    #[tokio::test]
    async fn changes_include_docs_deleted() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        };
        let r1 = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        let rev1: Revision = r1[0].rev.clone().unwrap().parse().unwrap();

        let del = Document {
            id: "doc1".into(),
            rev: Some(rev1),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();

        let changes = db
            .changes(ChangesOptions {
                include_docs: true,
                ..Default::default()
            })
            .await
            .unwrap();
        // Only the latest change entry should remain (update replaces)
        let last = changes.results.last().unwrap();
        assert!(last.deleted);
        let doc_json = last.doc.as_ref().unwrap();
        assert_eq!(doc_json["_deleted"], true);
    }

    #[tokio::test]
    async fn revs_diff_identifies_missing() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "abc".into())),
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::replication())
            .await
            .unwrap();

        let mut revs = HashMap::new();
        revs.insert("doc1".into(), vec!["1-abc".into(), "2-def".into()]);
        revs.insert("doc2".into(), vec!["1-xyz".into()]);

        let diff = db.revs_diff(revs).await.unwrap();
        assert_eq!(diff.results.len(), 2);
        // doc1: 2-def missing (1-abc exists and may be its ancestor)
        let d1 = &diff.results["doc1"];
        assert_eq!(d1.missing, ["2-def"]);
        assert_eq!(d1.possible_ancestors, ["1-abc"]);
        // doc2: entirely missing
        let d2 = &diff.results["doc2"];
        assert_eq!(d2.missing, ["1-xyz"]);
        assert!(d2.possible_ancestors.is_empty());
    }

    #[tokio::test]
    async fn bulk_get_basic() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "Alice"}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        let response = db
            .bulk_get(vec![
                BulkGetItem {
                    id: "doc1".into(),
                    rev: None,
                },
                BulkGetItem {
                    id: "nonexistent".into(),
                    rev: None,
                },
            ])
            .await
            .unwrap();

        assert_eq!(response.results.len(), 2);
        // doc1 should be found, with its ancestry
        let ok_doc = response.results[0].docs[0].ok.as_ref().unwrap();
        let rev: Revision = ok_doc["_rev"].as_str().unwrap().parse().unwrap();
        assert_eq!(rev.pos, 1);
        assert_eq!(
            ok_doc,
            &serde_json::json!({
                "_id": "doc1", "_rev": rev.to_string(), "name": "Alice",
                "_revisions": {"start": 1, "ids": [rev.hash]}
            })
        );
        // nonexistent should error
        let err = response.results[1].docs[0].error.as_ref().unwrap();
        assert_eq!(
            (err.id.as_str(), err.error.as_str()),
            ("nonexistent", "not_found")
        );
        assert!(response.results[1].docs[0].ok.is_none());
    }

    #[tokio::test]
    async fn recreate_deleted_doc_with_same_content() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        let rev1: Revision = r[0].rev.clone().unwrap().parse().unwrap();

        let del = Document {
            id: "doc1".into(),
            rev: Some(rev1),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(r[0].ok);

        // Re-create with the identical content and no rev. The deterministic
        // rev hash would reproduce rev 1 exactly, so the new edit must extend
        // the tombstone instead of starting a fresh pos-1 branch.
        let recreated = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![recreated], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(r[0].ok);
        let rev3 = r[0].rev.clone().unwrap();
        assert!(rev3.starts_with("3-"), "expected pos 3, got {rev3}");

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["v"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn put_with_rev_on_nonexistent() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "abc".into())),
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        // CouchDB and PouchDB: a revision of a missing document conflicts.
        assert!(!r[0].ok);
        assert_eq!(r[0].error.as_deref(), Some("conflict"));
    }

    #[tokio::test]
    async fn replication_with_revisions_ancestry() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(3, "ccc".into())),
            deleted: false,
            data: serde_json::json!({
                "hello": "world",
                "_revisions": {
                    "start": 3,
                    "ids": ["ccc", "bbb", "aaa"]
                }
            }),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.rev.unwrap().to_string(), "3-ccc");
        assert_eq!(fetched.data["hello"], "world");
        // _revisions should be stripped from stored data
        assert!(fetched.data.get("_revisions").is_none());
    }

    // --- F01: on-disk revision tree format ---

    fn put_doc(id: &str, rev: Option<&str>, v: serde_json::Value) -> Document {
        Document {
            id: id.into(),
            rev: rev.map(|r| r.parse().unwrap()),
            deleted: false,
            data: v,
            attachments: HashMap::new(),
        }
    }

    /// Write a DocRecord in the pre-0.5 nested format, exactly as older
    /// versions serialized it.
    fn write_legacy_record(db: &RedbAdapter, id: &str, tree: &RevTree, seq: u64) {
        let bytes = serde_json::to_vec(&LegacyDocRecord {
            rev_tree: legacy_tree_to_serialized(tree),
            seq,
        })
        .unwrap();
        let txn = db.inner.db.begin_write().unwrap();
        {
            let mut t = txn.open_table(DOC_TABLE).unwrap();
            t.insert(id, bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    fn raw_record(db: &RedbAdapter, id: &str) -> Vec<u8> {
        let txn = db.inner.db.begin_read().unwrap();
        let t = txn.open_table(DOC_TABLE).unwrap();
        t.get(id).unwrap().unwrap().value().to_vec()
    }

    fn linear_tree(len: u64) -> RevTree {
        let ids: Vec<String> = (0..len).rev().map(|i| format!("{:032x}", i + 1)).collect();
        vec![build_path_from_revs(
            len,
            &ids,
            NodeOpts::default(),
            RevStatus::Available,
        )]
    }

    #[tokio::test]
    async fn legacy_nested_records_still_load() {
        // A file as written by rouchdb <= 0.4: nested records, one shallow
        // and one far deeper than serde_json's default recursion limit (which
        // is what bricked F01 databases), and a metadata record without the
        // newer fields.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.redb");
        let docs = [("shallow", 3u64), ("deep", 400u64)];
        {
            let db = RedbAdapter::open(&path, "legacy").unwrap();
            for (id, len) in docs {
                write_legacy_record(&db, id, &linear_tree(len), len);
                let txn = db.inner.db.begin_write().unwrap();
                {
                    let mut t = txn.open_table(REV_DATA_TABLE).unwrap();
                    let rd = serde_json::to_vec(
                        &serde_json::json!({"data": {"id": id}, "deleted": false}),
                    )
                    .unwrap();
                    let leaf = format!("{}-{:032x}", len, len);
                    t.insert(rev_data_key(id, &leaf).as_str(), rd.as_slice())
                        .unwrap();
                }
                txn.commit().unwrap();
            }
            let txn = db.inner.db.begin_write().unwrap();
            {
                let mut meta = txn.open_table(META_TABLE).unwrap();
                meta.insert(META_KEY, &br#"{"update_seq":400,"db_uuid":"x"}"#[..])
                    .unwrap();
            }
            txn.commit().unwrap();
        }

        let db = RedbAdapter::open(&path, "legacy").unwrap();
        assert_eq!(db.info().await.unwrap().doc_count, 2);
        // Node status survives the legacy decoding: only the leaf has a body.
        let shallow = db
            .get(
                "shallow",
                GetOptions {
                    revs_info: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let statuses: Vec<&str> = shallow.data["_revs_info"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["available", "missing", "missing"]);
        for (id, len) in docs {
            let leaf = format!("{}-{:032x}", len, len);
            let got = db.get(id, GetOptions::default()).await.unwrap();
            assert_eq!(got.rev.unwrap().to_string(), leaf);
            assert_eq!(got.data["id"], id);

            // The next write rewrites the record in the flat format.
            let r = db
                .bulk_docs(
                    vec![put_doc(id, Some(&leaf), serde_json::json!({"v": 2}))],
                    BulkDocsOptions::new(),
                )
                .await
                .unwrap();
            assert!(r[0].ok, "{:?}", r[0]);
            assert!(!raw_record(&db, id).starts_with(b"{\"rev_tree\""));
            let got = db.get(id, GetOptions::default()).await.unwrap();
            assert_eq!(got.rev.unwrap().pos, len + 1);
        }
        assert_eq!(db.info().await.unwrap().doc_count, 2);
    }

    #[tokio::test]
    async fn corrupt_record_is_an_error_not_a_missing_doc() {
        let (_dir, db) = temp_db();
        db.bulk_docs(
            vec![put_doc("d", None, serde_json::json!({}))],
            BulkDocsOptions::new(),
        )
        .await
        .unwrap();
        {
            let txn = db.inner.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(DOC_TABLE).unwrap();
                t.insert("d", &b"{not json"[..]).unwrap();
            }
            txn.commit().unwrap();
        }
        // Writing without a rev must not silently replace the history.
        let res = db
            .bulk_docs(
                vec![put_doc("d", None, serde_json::json!({}))],
                BulkDocsOptions::new(),
            )
            .await;
        assert!(
            matches!(res, Err(RouchError::DatabaseError(_))),
            "{:?}",
            res
        );
        assert!(matches!(
            db.get("d", GetOptions::default()).await,
            Err(RouchError::DatabaseError(_))
        ));
        let diff = db
            .revs_diff(HashMap::from([(
                "d".to_string(),
                vec!["1-abc".to_string()],
            )]))
            .await;
        assert!(matches!(diff, Err(RouchError::DatabaseError(_))));
    }

    #[test]
    fn record_with_bad_parent_index_is_an_error() {
        // A parent index must point at an EARLIER node; anything else (self,
        // later, or out of range) is corruption, reported as an error rather
        // than a panic or a silently different tree.
        for parent in [1u32, 2, 99] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "revs": [
                    {"pos": 1, "hash": "a"},
                    {"pos": 2, "hash": "b", "parent": parent},
                ],
                "seq": 1
            }))
            .unwrap();
            assert!(
                matches!(decode_doc_record(&bytes), Err(RouchError::DatabaseError(_))),
                "parent {}",
                parent
            );
        }
        let ok = serde_json::to_vec(&serde_json::json!({
            "revs": [{"pos": 1, "hash": "a"}, {"pos": 2, "hash": "b", "parent": 0}],
            "seq": 1
        }))
        .unwrap();
        let (tree, seq) = decode_doc_record(&ok).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].tree.children[0].hash, "b");
    }

    #[test]
    fn flat_record_roundtrip_preserves_tree() {
        // 1-a -> 2-b (deleted) ; 1-a -> 2-c -> 3-d ; plus a second root 5-x
        let tree = vec![
            RevPath {
                pos: 1,
                tree: RevNode {
                    hash: "a".into(),
                    status: RevStatus::Missing,
                    opts: NodeOpts::default(),
                    children: vec![
                        RevNode {
                            hash: "b".into(),
                            status: RevStatus::Available,
                            opts: NodeOpts { deleted: true },
                            children: vec![],
                        },
                        RevNode {
                            hash: "c".into(),
                            status: RevStatus::Available,
                            opts: NodeOpts::default(),
                            children: vec![RevNode {
                                hash: "d".into(),
                                status: RevStatus::Available,
                                opts: NodeOpts::default(),
                                children: vec![],
                            }],
                        },
                    ],
                },
            },
            RevPath {
                pos: 5,
                tree: RevNode {
                    hash: "x".into(),
                    status: RevStatus::Available,
                    opts: NodeOpts::default(),
                    children: vec![],
                },
            },
        ];
        let bytes = encode_doc_record(&tree, 7).unwrap();
        let (back, seq) = decode_doc_record(&bytes).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(format!("{:?}", back), format!("{:?}", tree));
    }

    /// F68: bounded queries must not scan (and decode) the whole database.
    /// A corrupt record placed outside the requested range proves it.
    #[tokio::test]
    async fn bounded_queries_do_not_scan_everything() {
        let (_dir, db) = temp_db();
        for id in ["a", "b", "c", "zzz"] {
            db.bulk_docs(
                vec![put_doc(id, None, serde_json::json!({"id": id}))],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        }
        {
            let txn = db.inner.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(DOC_TABLE).unwrap();
                t.insert("zzz", &b"{corrupt"[..]).unwrap();
            }
            txn.commit().unwrap();
        }
        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 4);
        let page = db
            .all_docs(AllDocsOptions {
                limit: Some(2),
                include_docs: true,
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        let ids = |r: &AllDocsResponse| r.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&page), ["a", "b"]);
        assert_eq!(page.total_rows, 4);
        let range = db
            .all_docs(AllDocsOptions {
                start_key: Some("b".into()),
                end_key: Some("c".into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert_eq!(ids(&range), ["b", "c"]);
        let key = db
            .all_docs(AllDocsOptions {
                key: Some("a".into()),
                ..AllDocsOptions::new()
            })
            .await
            .unwrap();
        assert_eq!(ids(&key), ["a"]);
        let ch = db
            .changes(ChangesOptions {
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        let seqs: Vec<(u64, &str)> = ch
            .results
            .iter()
            .map(|c| (c.seq.as_num(), c.id.as_str()))
            .collect();
        assert_eq!(seqs, [(1, "a"), (2, "b")]);
        // Scanning into the corrupt record still reports the error.
        assert!(matches!(
            db.all_docs(AllDocsOptions::new()).await,
            Err(RouchError::DatabaseError(_))
        ));
    }

    /// F87: storage work (fsync'd commits, scans) must not run on the async
    /// runtime's thread. On a current-thread runtime another task has to keep
    /// making progress while a large write is in flight.
    #[test]
    fn storage_work_runs_off_the_runtime_thread() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_dir, db) = temp_db();
            let db = Arc::new(db);
            let docs: Vec<Document> = (0..2000)
                .map(|i| put_doc(&format!("d{}", i), None, serde_json::json!({"i": i})))
                .collect();
            let writer = tokio::spawn({
                let db = db.clone();
                async move { db.bulk_docs(docs, BulkDocsOptions::new()).await }
            });
            let mut ticks = 0u64;
            while !writer.is_finished() {
                tokio::task::yield_now().await;
                ticks += 1;
            }
            let results = writer.await.unwrap().unwrap();
            assert_eq!(results.len(), 2000);
            assert!(
                ticks > 10,
                "the runtime thread was blocked (ticks = {})",
                ticks
            );
        });
    }

    /// F05: files written by rouchdb <= 0.4 stored attachment bytes under
    /// `doc_id\0name`; opening them re-keys the bytes by digest.
    #[tokio::test]
    async fn legacy_attachment_keys_are_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.redb");
        let digest = attachment_digest(b"legacy bytes");
        {
            let db = RedbAdapter::open(&path, "legacy").unwrap();
            let tree = linear_tree(2);
            let rev = format!("2-{:032x}", 2);
            write_legacy_record(&db, "d", &tree, 1);
            let txn = db.inner.db.begin_write().unwrap();
            {
                let mut revs = txn.open_table(REV_DATA_TABLE).unwrap();
                let rd = serde_json::json!({
                    "data": {"v": 1}, "deleted": false,
                    "attachments": {"a.txt": {"content_type": "text/plain", "digest": digest, "length": 12}}
                });
                revs.insert(
                    rev_data_key("d", &rev).as_str(),
                    serde_json::to_vec(&rd).unwrap().as_slice(),
                )
                .unwrap();
                let mut atts = txn.open_table(ATTACHMENT_TABLE).unwrap();
                atts.insert("d\0a.txt", &b"legacy bytes"[..]).unwrap();
                // A pre-0.5 metadata record (no schema field).
                let mut meta = txn.open_table(META_TABLE).unwrap();
                meta.insert(META_KEY, &br#"{"update_seq":1,"db_uuid":"x"}"#[..])
                    .unwrap();
            }
            txn.commit().unwrap();
        }

        let db = RedbAdapter::open(&path, "legacy").unwrap();
        let bytes = db
            .get_attachment("d", "a.txt", GetAttachmentOptions::default())
            .await
            .unwrap();
        assert_eq!(bytes, b"legacy bytes");
        let txn = db.inner.db.begin_read().unwrap();
        let atts = txn.open_table(ATTACHMENT_TABLE).unwrap();
        assert!(atts.get("d\0a.txt").unwrap().is_none());
        assert!(atts.get(digest.as_str()).unwrap().is_some());
        let meta = read_meta(&txn.open_table(META_TABLE).unwrap()).unwrap();
        assert_eq!(meta.schema, SCHEMA_VERSION);
        assert_eq!(meta.update_seq, 1);
        // Document counts are computed once for files that predate them.
        let info = db.info().await.unwrap();
        assert_eq!((info.doc_count, info.doc_del_count), (1, 0));
    }

    #[tokio::test]
    async fn get_with_conflicts() {
        let (_dir, db) = temp_db();

        // Create two conflicting revisions via replication mode
        let doc1 = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "aaa".into())),
            deleted: false,
            data: serde_json::json!({"branch": "a"}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc1], BulkDocsOptions::replication())
            .await
            .unwrap();

        let doc2 = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "bbb".into())),
            deleted: false,
            data: serde_json::json!({"branch": "b"}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc2], BulkDocsOptions::replication())
            .await
            .unwrap();

        let fetched = db
            .get(
                "doc1",
                GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // Same generation: the higher hash wins, the other is the conflict.
        assert_eq!(fetched.rev.unwrap().to_string(), "1-bbb");
        assert_eq!(fetched.data["branch"], "b");
        assert_eq!(fetched.data["_conflicts"], serde_json::json!(["1-aaa"]));
    }

    #[tokio::test]
    async fn remove_local_nonexistent() {
        let (_dir, db) = temp_db();
        let result = db.remove_local("nope").await;
        assert!(
            matches!(result, Err(RouchError::NotFound(_))),
            "{:?}",
            result
        );
    }

    fn put_raw(db: &RedbAdapter, table: TableDefinition<&str, &[u8]>, key: &str, value: &[u8]) {
        let txn = db.inner.db.begin_write().unwrap();
        txn.open_table(table).unwrap().insert(key, value).unwrap();
        txn.commit().unwrap();
    }

    fn body_keys(db: &RedbAdapter, id: &str) -> Vec<String> {
        let txn = db.inner.db.begin_read().unwrap();
        rev_data_keys(&txn.open_table(REV_DATA_TABLE).unwrap(), id).unwrap()
    }

    async fn write_doc(
        db: &RedbAdapter,
        id: &str,
        rev: Option<&str>,
        data: serde_json::Value,
    ) -> String {
        let doc = Document {
            id: id.into(),
            rev: rev.map(|r| r.parse().unwrap()),
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let res = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(res[0].ok, "{:?}", res[0]);
        res[0].rev.clone().unwrap()
    }

    /// A file written by a newer rouchdb (unknown schema) is refused with a
    /// clear error instead of being misread, and is left untouched.
    #[tokio::test]
    async fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.redb");
        let future =
            serde_json::json!({"update_seq": 7, "db_uuid": "u", "schema": SCHEMA_VERSION + 1});
        {
            let db = RedbAdapter::open(&path, "future").unwrap();
            put_raw(
                &db,
                META_TABLE,
                META_KEY,
                &serde_json::to_vec(&future).unwrap(),
            );
        }
        let err = RedbAdapter::open(&path, "future").err().expect("must fail");
        let msg = err.to_string();
        assert!(matches!(err, RouchError::DatabaseError(_)), "{msg}");
        assert!(
            msg.contains(&format!("schema version {}", SCHEMA_VERSION + 1)),
            "{msg}"
        );
        assert!(msg.contains("newer"), "{msg}");
        // Still refused (nothing was rewritten); the current schema opens.
        assert!(RedbAdapter::open(&path, "future").is_err());
        let current =
            serde_json::json!({"update_seq": 7, "db_uuid": "u", "schema": SCHEMA_VERSION});
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(META_TABLE)
                .unwrap()
                .insert(META_KEY, serde_json::to_vec(&current).unwrap().as_slice())
                .unwrap();
            txn.commit().unwrap();
        }
        let db = RedbAdapter::open(&path, "future").unwrap();
        assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(7));
    }

    /// Q-CORE-3: the body keys of `a` and of ids that extend it with a NUL
    /// (`a\0b`, `a\0`) share a key range but never mix. The key encoding
    /// itself is unchanged, so existing files keep working.
    #[tokio::test]
    async fn nul_ids_have_disjoint_body_keys() {
        assert_eq!(rev_data_key("a\0b", "1-x"), "a\u{0}b\u{0}1-x");
        let (_dir, db) = temp_db();
        let mut revs: HashMap<&str, String> = HashMap::new();
        for id in ["a", "a\0b", "a\0", "a\0\0"] {
            revs.insert(id, write_doc(&db, id, None, serde_json::json!({})).await);
        }
        for (id, rev) in &revs {
            assert_eq!(body_keys(&db, id), [rev_data_key(id, rev)], "{id:?}");
        }
        let a = revs["a"].clone();
        let res = db
            .purge(HashMap::from([("a".to_string(), vec![a])]))
            .await
            .unwrap();
        assert_eq!(res.purged["a"].len(), 1);
        db.compact().await.unwrap();
        for id in ["a\0b", "a\0", "a\0\0"] {
            assert!(db.get(id, GetOptions::default()).await.is_ok(), "{id:?}");
        }
    }

    /// Q-CORE-1: revisions stemmed by the revision limit lose their bodies.
    #[tokio::test]
    async fn stemming_drops_stored_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let db = RedbAdapter::open(dir.path().join("s.redb"), "s")
            .unwrap()
            .with_rev_limit(3);
        let mut rev = write_doc(&db, "d", None, serde_json::json!({"v": 0})).await;
        let first = rev.clone();
        for v in 1..6 {
            rev = write_doc(&db, "d", Some(&rev), serde_json::json!({"v": v})).await;
        }
        assert_eq!(body_keys(&db, "d").len(), 3);
        let old = GetOptions {
            rev: Some(first),
            ..Default::default()
        };
        assert!(matches!(
            db.get("d", old).await,
            Err(RouchError::NotFound(_))
        ));
    }

    /// Bodies left behind by versions that did not drop stemmed revisions
    /// are unreadable, and compaction removes them.
    #[tokio::test]
    async fn stale_bodies_of_stemmed_revisions() {
        let (_dir, db) = temp_db();
        let r1 = write_doc(&db, "d", None, serde_json::json!({"v": 1})).await;
        let r2 = write_doc(&db, "d", Some(&r1), serde_json::json!({"v": 2})).await;
        // What an older version left: the tree stemmed to 2-x, 1-x's body kept.
        let (mut tree, seq) = decode_doc_record(&raw_record(&db, "d")).unwrap();
        rouchdb_core::merge::stem(&mut tree, 1);
        put_raw(&db, DOC_TABLE, "d", &encode_doc_record(&tree, seq).unwrap());
        assert_eq!(body_keys(&db, "d").len(), 2);
        for rev in [&r1, &r2] {
            let opts = GetOptions {
                rev: Some(rev.clone()),
                ..Default::default()
            };
            let got = db.get("d", opts).await;
            assert_eq!(got.is_ok(), rev == &r2, "{rev}: {got:?}");
        }
        let bulk = db
            .bulk_get(vec![BulkGetItem {
                id: "d".into(),
                rev: Some(r1.clone()),
            }])
            .await
            .unwrap();
        assert!(bulk.results[0].docs[0].ok.is_none());
        db.compact().await.unwrap();
        assert_eq!(body_keys(&db, "d"), [rev_data_key("d", &r2)]);
    }

    /// Q-API-3: stored bodies nested beyond serde_json's 128 levels decode;
    /// one beyond rouchdb's limit (only an older version could have stored
    /// it) is a clear error, and does not break compaction.
    #[tokio::test]
    async fn deep_bodies_decode() {
        let (_dir, db) = temp_db();
        let mut deep = serde_json::json!(1);
        for _ in 1..MAX_NESTING_DEPTH {
            deep = serde_json::json!([deep]);
        }
        write_doc(&db, "deep", None, serde_json::json!({ "v": deep.clone() })).await;
        let got = db.get("deep", GetOptions::default()).await.unwrap();
        assert_eq!(got.data["v"], deep);
        db.put_local("l", serde_json::json!({ "v": deep.clone() }))
            .await
            .unwrap();
        assert_eq!(db.get_local("l").await.unwrap()["v"], deep);
        assert!(
            db.put_local("l", serde_json::json!({ "v": [deep.clone()] }))
                .await
                .is_err()
        );

        let rev = write_doc(&db, "legacy", None, serde_json::json!({})).await;
        let body = format!(
            r#"{{"data":{{"v":{}1{}}},"deleted":false}}"#,
            "[".repeat(MAX_NESTING_DEPTH + 1),
            "]".repeat(MAX_NESTING_DEPTH + 1)
        );
        put_raw(
            &db,
            REV_DATA_TABLE,
            &rev_data_key("legacy", &rev),
            body.as_bytes(),
        );
        let err = db.get("legacy", GetOptions::default()).await.unwrap_err();
        assert!(err.to_string().contains("exceeds the maximum"), "{err}");
        db.compact().await.unwrap();
    }

    #[tokio::test]
    async fn attachment_metadata_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("att.redb");
        let doc = Document::from_json(serde_json::json!({
            "_id": "d",
            "_attachments": {"a.txt": {"content_type": "text/plain", "data": "aGk="}}
        }))
        .unwrap();
        let expected = serde_json::json!({"a.txt": {"content_type": "text/plain", "revpos": 1,
            "digest": attachment_digest(b"hi"), "length": 2, "stub": true}});
        {
            let db = RedbAdapter::open(&path, "t").unwrap();
            db.bulk_docs(vec![doc], BulkDocsOptions::new())
                .await
                .unwrap();
        }
        let db = RedbAdapter::open(&path, "t").unwrap();
        let got = db.get("d", GetOptions::default()).await.unwrap();
        assert_eq!(got.to_json()["_attachments"], expected);
        let bulk = db
            .bulk_get(vec![BulkGetItem {
                id: "d".into(),
                rev: None,
            }])
            .await
            .unwrap();
        let doc = bulk.results[0].docs[0].ok.as_ref().unwrap();
        assert_eq!(doc["_attachments"]["a.txt"]["revpos"], 1);
        assert_eq!(doc["_attachments"]["a.txt"]["data"], "aGk=");
    }
}
