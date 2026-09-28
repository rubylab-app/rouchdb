use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use md5::{Digest, Md5};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::{Result, RouchError};
use rouchdb_core::merge::{collect_conflicts, is_deleted, merge_tree, winning_rev};
use rouchdb_core::rev_tree::{
    NodeOpts, RevNode, RevPath, RevStatus, RevTree, build_path_from_revs, collect_leaves,
    find_rev_ancestry, rev_exists,
};

const DEFAULT_REV_LIMIT: u64 = 1000;

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

/// Attachments: digest -> raw bytes
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

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AttachmentRecord {
    content_type: String,
    digest: String,
    length: u64,
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

fn rev_data_key(doc_id: &str, rev_str: &str) -> String {
    format!("{}\0{}", doc_id, rev_str)
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Persistent adapter backed by `redb`.
pub struct RedbAdapter {
    db: Arc<Database>,
    name: String,
    /// Lock for write serialization (redb handles transactions, but we need
    /// to serialize our read-modify-write sequences).
    write_lock: Arc<RwLock<()>>,
}

impl RedbAdapter {
    /// Open or create a database at the given path.
    pub fn open(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let db = Database::create(path.as_ref())
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;

        // Initialize tables
        {
            let write_txn = db
                .begin_write()
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
            // Opening tables in a write transaction creates them if they don't exist
            {
                write_txn
                    .open_table(DOC_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                write_txn
                    .open_table(REV_DATA_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                write_txn
                    .open_table(CHANGES_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                write_txn
                    .open_table(LOCAL_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                write_txn
                    .open_table(ATTACHMENT_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
            }
            {
                let mut meta = write_txn
                    .open_table(META_TABLE)
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                if meta
                    .get("meta")
                    .map_err(|e| RouchError::DatabaseError(e.to_string()))?
                    .is_none()
                {
                    let record = MetaRecord {
                        update_seq: 0,
                        db_uuid: Uuid::new_v4().to_string(),
                    };
                    let bytes = serde_json::to_vec(&record)?;
                    meta.insert("meta", bytes.as_slice())
                        .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
                }
            }
            write_txn
                .commit()
                .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        }

        Ok(Self {
            db: Arc::new(db),
            name: name.to_string(),
            write_lock: Arc::new(RwLock::new(())),
        })
    }
}

fn generate_rev_hash(
    doc_data: &serde_json::Value,
    deleted: bool,
    prev_rev: Option<&str>,
) -> String {
    let mut hasher = Md5::new();
    if let Some(prev) = prev_rev {
        hasher.update(prev.as_bytes());
    }
    hasher.update(if deleted { b"1" } else { b"0" });
    let serialized = serde_json::to_string(doc_data).unwrap_or_default();
    hasher.update(serialized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn attachment_key(doc_id: &str, att_id: &str) -> String {
    format!("{}\0{}", doc_id, att_id)
}

fn compute_attachment_digest(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    let hash = hasher.finalize();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
    format!("md5-{}", b64)
}

fn parse_rev(rev_str: &str) -> Result<(u64, String)> {
    let (pos_str, hash) = rev_str
        .split_once('-')
        .ok_or_else(|| RouchError::InvalidRev(rev_str.to_string()))?;
    let pos: u64 = pos_str
        .parse()
        .map_err(|_| RouchError::InvalidRev(rev_str.to_string()))?;
    Ok((pos, hash.to_string()))
}

#[async_trait]
impl Adapter for RedbAdapter {
    async fn info(&self) -> Result<DbInfo> {
        // Read metadata and document data from a SINGLE read transaction so
        // update_seq and the doc snapshot reflect the same committed state.
        let read_txn = db_err!(self.db.begin_read())?;
        let meta_table = db_err!(read_txn.open_table(META_TABLE))?;
        let meta: MetaRecord = serde_json::from_slice(
            db_err!(meta_table.get("meta"))?
                .ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?
                .value(),
        )?;
        let table = db_err!(read_txn.open_table(DOC_TABLE))?;

        let mut doc_count = 0u64;
        let mut doc_del_count = 0u64;
        let iter = db_err!(table.iter())?;
        for entry in iter {
            let entry = db_err!(entry)?;
            let (tree, _) = decode_doc_record(entry.1.value())?;
            if is_deleted(&tree) {
                doc_del_count += 1;
            } else {
                doc_count += 1;
            }
        }

        Ok(DbInfo {
            db_name: self.name.clone(),
            doc_count,
            doc_del_count,
            update_seq: Seq::Num(meta.update_seq),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let (tree, _) =
            load_doc_record(&doc_table, id)?.ok_or_else(|| RouchError::NotFound(id.to_string()))?;

        let target_rev = if let Some(ref rev_str) = opts.rev {
            rev_str.clone()
        } else {
            winning_rev(&tree)
                .ok_or_else(|| RouchError::NotFound(id.to_string()))?
                .to_string()
        };

        let key = rev_data_key(id, &target_rev);
        let rev_guard = db_err!(rev_table.get(key.as_str()))?;

        let (data, deleted, att_records) = if let Some(guard) = rev_guard {
            let rd: RevDataRecord = serde_json::from_slice(guard.value())?;
            (rd.data, rd.deleted, rd.attachments)
        } else {
            (
                serde_json::Value::Object(serde_json::Map::new()),
                false,
                HashMap::new(),
            )
        };

        if deleted && opts.rev.is_none() {
            return Err(RouchError::NotFound(id.to_string()));
        }

        let (pos, hash) = parse_rev(&target_rev)?;

        let mut doc = Document {
            id: id.to_string(),
            rev: Some(Revision::new(pos, hash)),
            deleted,
            data,
            attachments: HashMap::new(),
        };

        // Surface stored attachment metadata (content type, digest, length) so
        // callers can read it without a separate attachment fetch.
        for (name, rec) in att_records {
            doc.attachments.insert(
                name,
                AttachmentMeta {
                    content_type: rec.content_type,
                    digest: rec.digest,
                    length: rec.length,
                    stub: true,
                    data: None,
                },
            );
        }

        if opts.conflicts {
            let conflicts = collect_conflicts(&tree);
            if !conflicts.is_empty() {
                let conflict_list: Vec<serde_json::Value> = conflicts
                    .iter()
                    .map(|c| serde_json::Value::String(c.to_string()))
                    .collect();
                if let serde_json::Value::Object(ref mut map) = doc.data {
                    map.insert("_conflicts".into(), serde_json::Value::Array(conflict_list));
                }
            }
        }

        Ok(doc)
    }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;

        let mut results = Vec::with_capacity(docs.len());

        // Read current metadata
        let mut meta = {
            let meta_table = db_err!(write_txn.open_table(META_TABLE))?;
            let guard = db_err!(meta_table.get("meta"))?.unwrap();
            serde_json::from_slice::<MetaRecord>(guard.value())?
        };

        {
            let mut doc_table = db_err!(write_txn.open_table(DOC_TABLE))?;
            let mut rev_table = db_err!(write_txn.open_table(REV_DATA_TABLE))?;
            let mut changes_table = db_err!(write_txn.open_table(CHANGES_TABLE))?;

            for doc in docs {
                let result = process_doc(
                    &mut doc_table,
                    &mut rev_table,
                    &mut changes_table,
                    &mut meta,
                    doc,
                    opts.new_edits,
                )?;
                results.push(result);
            }
        }

        // Write updated metadata
        {
            let mut meta_table = db_err!(write_txn.open_table(META_TABLE))?;
            let meta_bytes = serde_json::to_vec(&meta)?;
            db_err!(meta_table.insert("meta", meta_bytes.as_slice()))?;
        }

        db_err!(write_txn.commit())?;

        Ok(results)
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let mut rows = Vec::new();
        let mut total_count = 0u64;

        let iter = db_err!(doc_table.iter())?;
        for entry in iter {
            let entry = db_err!(entry)?;
            let doc_id = entry.0.value().to_string();
            let (tree, _) = decode_doc_record(entry.1.value())?;

            let winner = match winning_rev(&tree) {
                Some(w) => w,
                None => continue,
            };
            let deleted = is_deleted(&tree);

            // total_rows is the count of non-deleted documents in the whole
            // database, independent of any range / key / skip / limit filters.
            if !deleted {
                total_count += 1;
            }

            if deleted && opts.keys.is_none() {
                continue;
            }

            // Apply key range filters (descending flips startkey/endkey meaning)
            if opts.keys.is_none() && opts.key.is_none() {
                if let Some(ref start) = opts.start_key
                    && ((!opts.descending && doc_id.as_str() < start.as_str())
                        || (opts.descending && doc_id.as_str() > start.as_str()))
                {
                    continue;
                }
                if let Some(ref end) = opts.end_key {
                    if opts.inclusive_end {
                        if (!opts.descending && doc_id.as_str() > end.as_str())
                            || (opts.descending && doc_id.as_str() < end.as_str())
                        {
                            continue;
                        }
                    } else if (!opts.descending && doc_id.as_str() >= end.as_str())
                        || (opts.descending && doc_id.as_str() <= end.as_str())
                    {
                        continue;
                    }
                }
            }

            if let Some(ref key) = opts.key
                && &doc_id != key
            {
                continue;
            }

            if let Some(ref keys) = opts.keys
                && !keys.contains(&doc_id)
            {
                continue;
            }

            let doc_json = if opts.include_docs && !deleted {
                let rev_str = winner.to_string();
                let key = rev_data_key(&doc_id, &rev_str);
                match db_err!(rev_table.get(key.as_str()))? {
                    Some(guard) => {
                        let rd: RevDataRecord = serde_json::from_slice(guard.value())?;
                        let mut obj = match rd.data {
                            serde_json::Value::Object(m) => m,
                            _ => serde_json::Map::new(),
                        };
                        obj.insert("_id".into(), serde_json::Value::String(doc_id.clone()));
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str));
                        // Embed _conflicts when requested, matching the memory
                        // adapter and CouchDB.
                        if opts.conflicts {
                            let conflicts = collect_conflicts(&tree);
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

            rows.push(AllDocsRow {
                id: doc_id.clone(),
                key: doc_id,
                value: AllDocsRowValue {
                    rev: winner.to_string(),
                    deleted: if deleted { Some(true) } else { None },
                },
                doc: doc_json,
            });
        }

        if opts.descending {
            rows.reverse();
        }

        let total_rows = total_count;
        let skip = opts.skip as usize;
        if skip > 0 {
            rows = rows.into_iter().skip(skip).collect();
        }
        if let Some(limit) = opts.limit {
            rows.truncate(limit as usize);
        }

        // Read update_seq from the SAME read transaction as the doc snapshot
        // to avoid a TOCTOU inconsistency with a concurrent committed write.
        let update_seq = if opts.update_seq {
            let meta_table = db_err!(read_txn.open_table(META_TABLE))?;
            let meta: MetaRecord = serde_json::from_slice(
                db_err!(meta_table.get("meta"))?
                    .ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?
                    .value(),
            )?;
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

    async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let changes_table = db_err!(read_txn.open_table(CHANGES_TABLE))?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let mut results = Vec::new();

        let start = opts.since.as_num().saturating_add(1);
        let iter = db_err!(changes_table.range(start..))?;

        // Propagate deserialization errors instead of panicking on a corrupt
        // or truncated change record.
        let entries: Vec<(u64, ChangeRecord)> = iter
            .filter_map(|e| e.ok())
            .map(|e| {
                Ok((
                    e.0.value(),
                    serde_json::from_slice::<ChangeRecord>(e.1.value())?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let iter: Box<dyn Iterator<Item = &(u64, ChangeRecord)>> = if opts.descending {
            Box::new(entries.iter().rev())
        } else {
            Box::new(entries.iter())
        };

        // Highest sequence inspected, so last_seq advances past a fully
        // filtered range instead of sticking at `since`.
        let mut max_scanned: Option<u64> = None;

        for (seq, change) in iter {
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
                let key = rev_data_key(&change.doc_id, &rev_str);
                match db_err!(rev_table.get(key.as_str()))? {
                    Some(guard) => {
                        let rd: RevDataRecord = serde_json::from_slice(guard.value())?;
                        let mut obj = match rd.data {
                            serde_json::Value::Object(m) => m,
                            _ => serde_json::Map::new(),
                        };
                        obj.insert(
                            "_id".into(),
                            serde_json::Value::String(change.doc_id.clone()),
                        );
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str.clone()));
                        if change.deleted {
                            obj.insert("_deleted".into(), serde_json::Value::Bool(true));
                        }
                        Some(serde_json::Value::Object(obj))
                    }
                    None => None,
                }
            } else {
                None
            };

            // Build changes list based on style
            let changes_list = if opts.style == ChangesStyle::AllDocs {
                // Fetch all leaf revisions for AllDocs style
                if let Some(ref tree) = tree {
                    collect_leaves(tree)
                        .iter()
                        .map(|l| ChangeRev {
                            rev: l.rev_string(),
                        })
                        .collect()
                } else {
                    vec![ChangeRev {
                        rev: rev_str.clone(),
                    }]
                }
            } else {
                vec![ChangeRev { rev: rev_str }]
            };

            // Collect conflicts if requested
            let conflicts = if opts.conflicts {
                if let Some(ref tree) = tree {
                    let c = collect_conflicts(tree);
                    if c.is_empty() {
                        None
                    } else {
                        Some(c.iter().map(|r| r.to_string()).collect())
                    }
                } else {
                    None
                }
            } else {
                None
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

    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;

        let mut results = HashMap::new();

        for (doc_id, rev_list) in revs {
            let mut missing = Vec::new();
            let mut possible_ancestors = Vec::new();

            let tree = load_doc_record(&doc_table, doc_id.as_str())?.map(|(t, _)| t);

            for rev_str in &rev_list {
                let (pos, hash) = parse_rev(rev_str)?;
                let exists = tree
                    .as_ref()
                    .map(|t| rev_exists(t, pos, &hash))
                    .unwrap_or(false);

                if !exists {
                    missing.push(rev_str.clone());
                    if let Some(ref tree) = tree {
                        let leaves = collect_leaves(tree);
                        for leaf in &leaves {
                            if leaf.pos < pos {
                                let anc = leaf.rev_string();
                                if !possible_ancestors.contains(&anc) {
                                    possible_ancestors.push(anc);
                                }
                            }
                        }
                    }
                }
            }

            if !missing.is_empty() {
                results.insert(
                    doc_id,
                    RevsDiffResult {
                        missing,
                        possible_ancestors,
                    },
                );
            }
        }

        Ok(RevsDiffResponse { results })
    }

    async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let mut results = Vec::new();

        for item in docs {
            let mut bulk_docs = Vec::new();

            match load_doc_record(&doc_table, item.id.as_str())? {
                Some((tree, _)) => {
                    let rev_str = if let Some(ref rev) = item.rev {
                        rev.clone()
                    } else {
                        match winning_rev(&tree) {
                            Some(w) => w.to_string(),
                            None => {
                                bulk_docs.push(BulkGetDoc {
                                    ok: None,
                                    error: Some(BulkGetError {
                                        id: item.id.clone(),
                                        rev: item.rev.unwrap_or_default(),
                                        error: "not_found".into(),
                                        reason: "missing".into(),
                                    }),
                                });
                                results.push(BulkGetResult {
                                    id: item.id,
                                    docs: bulk_docs,
                                });
                                continue;
                            }
                        }
                    };

                    let key = rev_data_key(&item.id, &rev_str);
                    if let Some(rev_guard) = db_err!(rev_table.get(key.as_str()))? {
                        let rd: RevDataRecord = serde_json::from_slice(rev_guard.value())?;
                        let mut obj = match rd.data {
                            serde_json::Value::Object(m) => m,
                            _ => serde_json::Map::new(),
                        };
                        obj.insert("_id".into(), serde_json::Value::String(item.id.clone()));
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str.clone()));
                        if rd.deleted {
                            obj.insert("_deleted".into(), serde_json::Value::Bool(true));
                        }

                        // Include _revisions for replication
                        if let Ok((pos, ref hash)) = parse_rev(&rev_str)
                            && let Some(ancestry) = find_rev_ancestry(&tree, pos, hash)
                        {
                            obj.insert(
                                "_revisions".into(),
                                serde_json::json!({
                                    "start": pos,
                                    "ids": ancestry
                                }),
                            );
                        }

                        bulk_docs.push(BulkGetDoc {
                            ok: Some(serde_json::Value::Object(obj)),
                            error: None,
                        });
                    } else {
                        bulk_docs.push(BulkGetDoc {
                            ok: None,
                            error: Some(BulkGetError {
                                id: item.id.clone(),
                                rev: rev_str,
                                error: "not_found".into(),
                                reason: "missing".into(),
                            }),
                        });
                    }
                }
                None => {
                    bulk_docs.push(BulkGetDoc {
                        ok: None,
                        error: Some(BulkGetError {
                            id: item.id.clone(),
                            rev: item.rev.unwrap_or_default(),
                            error: "not_found".into(),
                            reason: "missing".into(),
                        }),
                    });
                }
            }

            results.push(BulkGetResult {
                id: item.id,
                docs: bulk_docs,
            });
        }

        Ok(BulkGetResponse { results })
    }

    async fn put_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        rev: &str,
        data: Vec<u8>,
        content_type: &str,
    ) -> Result<DocResult> {
        let digest = compute_attachment_digest(&data);
        let length = data.len() as u64;
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;

        let result = {
            // Store the raw attachment data
            let mut att_table = db_err!(write_txn.open_table(ATTACHMENT_TABLE))?;
            let att_key = attachment_key(doc_id, att_id);
            db_err!(att_table.insert(att_key.as_str(), data.as_slice()))?;

            // Load existing doc and verify rev
            let mut doc_table = db_err!(write_txn.open_table(DOC_TABLE))?;
            let mut rev_table = db_err!(write_txn.open_table(REV_DATA_TABLE))?;
            let mut changes_table = db_err!(write_txn.open_table(CHANGES_TABLE))?;

            let (tree, _) = load_doc_record(&doc_table, doc_id)?
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            let winner =
                winning_rev(&tree).ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            if winner.to_string() != rev {
                return Err(RouchError::Conflict);
            }

            // Load current rev data to preserve existing attachments
            let rev_key = rev_data_key(doc_id, rev);
            let rd: RevDataRecord = db_err!(rev_table.get(rev_key.as_str()))?
                .map(|g| serde_json::from_slice(g.value()).unwrap())
                .unwrap_or(RevDataRecord {
                    data: serde_json::Value::Object(serde_json::Map::new()),
                    deleted: false,
                    attachments: HashMap::new(),
                });

            // Build updated attachment map
            let mut attachments = rd.attachments;
            attachments.insert(
                att_id.to_string(),
                AttachmentRecord {
                    content_type: content_type.to_string(),
                    digest,
                    length,
                },
            );

            // Build a Document and process as normal edit
            let doc = Document {
                id: doc_id.to_string(),
                rev: Some(winner),
                deleted: false,
                data: rd.data,
                attachments: attachments
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            AttachmentMeta {
                                content_type: v.content_type.clone(),
                                digest: v.digest.clone(),
                                length: v.length,
                                stub: true,
                                data: None,
                            },
                        )
                    })
                    .collect(),
            };

            let mut meta = {
                let meta_table = db_err!(write_txn.open_table(META_TABLE))?;
                let guard = db_err!(meta_table.get("meta"))?.unwrap();
                serde_json::from_slice::<MetaRecord>(guard.value())?
            };

            let result = process_doc_new_edits_with_attachments(
                &mut doc_table,
                &mut rev_table,
                &mut changes_table,
                &mut meta,
                doc,
                attachments,
            )?;

            // Save updated metadata
            {
                let mut meta_table = db_err!(write_txn.open_table(META_TABLE))?;
                let meta_bytes = serde_json::to_vec(&meta)?;
                db_err!(meta_table.insert("meta", meta_bytes.as_slice()))?;
            }

            result
        };

        db_err!(write_txn.commit())?;
        Ok(result)
    }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        let read_txn = db_err!(self.db.begin_read())?;

        // Verify the document and revision exist, and the attachment is tracked
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let (tree, _) = load_doc_record(&doc_table, doc_id)?
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        let rev_str = if let Some(ref rev) = opts.rev {
            rev.clone()
        } else {
            winning_rev(&tree)
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?
                .to_string()
        };

        // Check that the attachment exists in this revision's metadata
        let rev_key = rev_data_key(doc_id, &rev_str);
        let rd: RevDataRecord = db_err!(rev_table.get(rev_key.as_str()))?
            .map(|g| serde_json::from_slice(g.value()).unwrap())
            .ok_or_else(|| RouchError::NotFound(format!("attachment {}/{}", doc_id, att_id)))?;

        if !rd.attachments.contains_key(att_id) {
            return Err(RouchError::NotFound(format!(
                "attachment {}/{}",
                doc_id, att_id
            )));
        }

        // Fetch raw bytes
        let att_table = db_err!(read_txn.open_table(ATTACHMENT_TABLE))?;
        let att_key = attachment_key(doc_id, att_id);
        let guard = db_err!(att_table.get(att_key.as_str()))?
            .ok_or_else(|| RouchError::NotFound(format!("attachment {}/{}", doc_id, att_id)))?;

        Ok(guard.value().to_vec())
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;

        let result = {
            let mut doc_table = db_err!(write_txn.open_table(DOC_TABLE))?;
            let mut rev_table = db_err!(write_txn.open_table(REV_DATA_TABLE))?;
            let mut changes_table = db_err!(write_txn.open_table(CHANGES_TABLE))?;
            let mut att_table = db_err!(write_txn.open_table(ATTACHMENT_TABLE))?;

            // Load existing doc and verify rev
            let (tree, _) = load_doc_record(&doc_table, doc_id)?
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            let winner =
                winning_rev(&tree).ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
            if winner.to_string() != rev {
                return Err(RouchError::Conflict);
            }

            // Load current rev data
            let rev_key = rev_data_key(doc_id, rev);
            let rd: RevDataRecord = db_err!(rev_table.get(rev_key.as_str()))?
                .map(|g| serde_json::from_slice(g.value()).unwrap())
                .unwrap_or(RevDataRecord {
                    data: serde_json::Value::Object(serde_json::Map::new()),
                    deleted: false,
                    attachments: HashMap::new(),
                });

            // Remove attachment from metadata and storage
            let mut attachments = rd.attachments;
            attachments.remove(att_id);

            let att_key = attachment_key(doc_id, att_id);
            let _ = db_err!(att_table.remove(att_key.as_str()));

            // Create a new revision without the attachment
            let doc = Document {
                id: doc_id.to_string(),
                rev: Some(winner),
                deleted: false,
                data: rd.data,
                attachments: attachments
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            AttachmentMeta {
                                content_type: v.content_type.clone(),
                                digest: v.digest.clone(),
                                length: v.length,
                                stub: true,
                                data: None,
                            },
                        )
                    })
                    .collect(),
            };

            let mut meta = {
                let meta_table = db_err!(write_txn.open_table(META_TABLE))?;
                let guard = db_err!(meta_table.get("meta"))?.unwrap();
                serde_json::from_slice::<MetaRecord>(guard.value())?
            };

            let result = process_doc_new_edits_with_attachments(
                &mut doc_table,
                &mut rev_table,
                &mut changes_table,
                &mut meta,
                doc,
                attachments,
            )?;

            {
                let mut meta_table = db_err!(write_txn.open_table(META_TABLE))?;
                let meta_bytes = serde_json::to_vec(&meta)?;
                db_err!(meta_table.insert("meta", meta_bytes.as_slice()))?;
            }

            result
        };

        db_err!(write_txn.commit())?;
        Ok(result)
    }

    async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
        let read_txn = db_err!(self.db.begin_read())?;
        let table = db_err!(read_txn.open_table(LOCAL_TABLE))?;
        let guard = db_err!(table.get(id))?
            .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))?;
        let value: serde_json::Value = serde_json::from_slice(guard.value())?;
        Ok(value)
    }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut table = db_err!(write_txn.open_table(LOCAL_TABLE))?;
            let bytes = serde_json::to_vec(&doc)?;
            db_err!(table.insert(id, bytes.as_slice()))?;
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }

    async fn remove_local(&self, id: &str) -> Result<()> {
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;
        {
            let mut table = db_err!(write_txn.open_table(LOCAL_TABLE))?;
            db_err!(table.remove(id))?
                .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))?;
        }
        db_err!(write_txn.commit())?;
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        // TODO: remove non-leaf revision data
        Ok(())
    }

    async fn destroy(&self) -> Result<()> {
        let _lock = self.write_lock.write().await;
        let write_txn = db_err!(self.db.begin_write())?;

        // Delete all tables in O(1) instead of draining entries one by one.
        let _ = db_err!(write_txn.delete_table(DOC_TABLE))?;
        let _ = db_err!(write_txn.delete_table(REV_DATA_TABLE))?;
        let _ = db_err!(write_txn.delete_table(CHANGES_TABLE))?;
        let _ = db_err!(write_txn.delete_table(LOCAL_TABLE))?;
        let _ = db_err!(write_txn.delete_table(ATTACHMENT_TABLE))?;

        // Recreate empty tables so subsequent operations don't fail.
        db_err!(write_txn.open_table(DOC_TABLE))?;
        db_err!(write_txn.open_table(REV_DATA_TABLE))?;
        db_err!(write_txn.open_table(CHANGES_TABLE))?;
        db_err!(write_txn.open_table(LOCAL_TABLE))?;
        db_err!(write_txn.open_table(ATTACHMENT_TABLE))?;

        // Reset metadata
        {
            let mut meta_table = db_err!(write_txn.open_table(META_TABLE))?;
            let record = MetaRecord {
                update_seq: 0,
                db_uuid: Uuid::new_v4().to_string(),
            };
            let bytes = serde_json::to_vec(&record)?;
            db_err!(meta_table.insert("meta", bytes.as_slice()))?;
        }

        db_err!(write_txn.commit())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Document processing (shared by bulk_docs)
// ---------------------------------------------------------------------------

fn process_doc(
    doc_table: &mut redb::Table<&str, &[u8]>,
    rev_table: &mut redb::Table<&str, &[u8]>,
    changes_table: &mut redb::Table<u64, &[u8]>,
    meta: &mut MetaRecord,
    doc: Document,
    new_edits: bool,
) -> Result<DocResult> {
    if new_edits {
        process_doc_new_edits(doc_table, rev_table, changes_table, meta, doc)
    } else {
        process_doc_replication(doc_table, rev_table, changes_table, meta, doc)
    }
}

fn process_doc_new_edits(
    doc_table: &mut redb::Table<&str, &[u8]>,
    rev_table: &mut redb::Table<&str, &[u8]>,
    changes_table: &mut redb::Table<u64, &[u8]>,
    meta: &mut MetaRecord,
    doc: Document,
) -> Result<DocResult> {
    let doc_id = if doc.id.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        doc.id.clone()
    };

    // Load existing record; a decoding error aborts the batch instead of
    // being treated as a missing document.
    let existing_record = load_doc_record(doc_table, doc_id.as_str())?;

    let existing_tree = existing_record
        .as_ref()
        .map(|(t, _)| t.clone())
        .unwrap_or_default();

    // Conflict check
    if let Some((ref tree, _)) = existing_record {
        let tree = tree.clone();
        let winner = winning_rev(&tree);
        match (&doc.rev, &winner) {
            (Some(provided_rev), Some(current_winner)) => {
                if provided_rev.to_string() != current_winner.to_string() {
                    return Ok(DocResult {
                        ok: false,
                        id: doc_id,
                        rev: None,
                        error: Some("conflict".into()),
                        reason: Some("Document update conflict".into()),
                    });
                }
            }
            // Creating a doc that already exists and is not deleted is a
            // conflict; a deleted winner falls through and may be re-created.
            (None, Some(_)) if !is_deleted(&tree) => {
                return Ok(DocResult {
                    ok: false,
                    id: doc_id,
                    rev: None,
                    error: Some("conflict".into()),
                    reason: Some("Document update conflict".into()),
                });
            }
            _ => {}
        }
    } else if doc.rev.is_some() {
        return Ok(DocResult {
            ok: false,
            id: doc_id,
            rev: None,
            error: Some("not_found".into()),
            reason: Some("missing".into()),
        });
    }

    // Generate new revision
    let new_pos = doc.rev.as_ref().map(|r| r.pos + 1).unwrap_or(1);
    let prev_rev_str = doc.rev.as_ref().map(|r| r.to_string());
    let new_hash = generate_rev_hash(&doc.data, doc.deleted, prev_rev_str.as_deref());
    let new_rev_str = format!("{}-{}", new_pos, new_hash);

    let mut rev_hashes = vec![new_hash.clone()];
    if let Some(ref prev) = doc.rev {
        rev_hashes.push(prev.hash.clone());
    }
    let new_path = build_path_from_revs(
        new_pos,
        &rev_hashes,
        NodeOpts {
            deleted: doc.deleted,
        },
        RevStatus::Available,
    );

    let (merged_tree, _) = merge_tree(&existing_tree, &new_path, DEFAULT_REV_LIMIT);

    // Update sequence
    meta.update_seq += 1;
    let seq = meta.update_seq;

    // Remove old change entry
    if let Some((_, old_seq)) = existing_record {
        db_err!(changes_table.remove(old_seq))?;
    }

    // Save doc record
    let doc_bytes = encode_doc_record(&merged_tree, seq)?;
    db_err!(doc_table.insert(doc_id.as_str(), doc_bytes.as_slice()))?;

    // Save rev data
    let rd = RevDataRecord {
        data: doc.data,
        deleted: doc.deleted,
        attachments: HashMap::new(),
    };
    let rev_bytes = serde_json::to_vec(&rd)?;
    let key = rev_data_key(&doc_id, &new_rev_str);
    db_err!(rev_table.insert(key.as_str(), rev_bytes.as_slice()))?;

    // Save change
    let change = ChangeRecord {
        doc_id: doc_id.clone(),
        deleted: doc.deleted,
    };
    let change_bytes = serde_json::to_vec(&change)?;
    db_err!(changes_table.insert(seq, change_bytes.as_slice()))?;

    Ok(DocResult {
        ok: true,
        id: doc_id,
        rev: Some(new_rev_str),
        error: None,
        reason: None,
    })
}

/// Like `process_doc_new_edits` but also stores attachment metadata in the rev data.
fn process_doc_new_edits_with_attachments(
    doc_table: &mut redb::Table<&str, &[u8]>,
    rev_table: &mut redb::Table<&str, &[u8]>,
    changes_table: &mut redb::Table<u64, &[u8]>,
    meta: &mut MetaRecord,
    doc: Document,
    attachments: HashMap<String, AttachmentRecord>,
) -> Result<DocResult> {
    let doc_id = doc.id.clone();

    let existing_record = load_doc_record(doc_table, doc_id.as_str())?;

    let existing_tree = existing_record
        .as_ref()
        .map(|(t, _)| t.clone())
        .unwrap_or_default();

    // Generate new revision
    let new_pos = doc.rev.as_ref().map(|r| r.pos + 1).unwrap_or(1);
    let prev_rev_str = doc.rev.as_ref().map(|r| r.to_string());
    let new_hash = generate_rev_hash(&doc.data, doc.deleted, prev_rev_str.as_deref());
    let new_rev_str = format!("{}-{}", new_pos, new_hash);

    let mut rev_hashes = vec![new_hash.clone()];
    if let Some(ref prev) = doc.rev {
        rev_hashes.push(prev.hash.clone());
    }
    let new_path = build_path_from_revs(
        new_pos,
        &rev_hashes,
        NodeOpts {
            deleted: doc.deleted,
        },
        RevStatus::Available,
    );

    let (merged_tree, _) = merge_tree(&existing_tree, &new_path, DEFAULT_REV_LIMIT);

    meta.update_seq += 1;
    let seq = meta.update_seq;

    if let Some((_, old_seq)) = existing_record {
        db_err!(changes_table.remove(old_seq))?;
    }

    let doc_bytes = encode_doc_record(&merged_tree, seq)?;
    db_err!(doc_table.insert(doc_id.as_str(), doc_bytes.as_slice()))?;

    // Save rev data with attachment metadata
    let rd = RevDataRecord {
        data: doc.data,
        deleted: doc.deleted,
        attachments,
    };
    let rev_bytes = serde_json::to_vec(&rd)?;
    let key = rev_data_key(&doc_id, &new_rev_str);
    db_err!(rev_table.insert(key.as_str(), rev_bytes.as_slice()))?;

    let change = ChangeRecord {
        doc_id: doc_id.clone(),
        deleted: doc.deleted,
    };
    let change_bytes = serde_json::to_vec(&change)?;
    db_err!(changes_table.insert(seq, change_bytes.as_slice()))?;

    Ok(DocResult {
        ok: true,
        id: doc_id,
        rev: Some(new_rev_str),
        error: None,
        reason: None,
    })
}

fn process_doc_replication(
    doc_table: &mut redb::Table<&str, &[u8]>,
    rev_table: &mut redb::Table<&str, &[u8]>,
    changes_table: &mut redb::Table<u64, &[u8]>,
    meta: &mut MetaRecord,
    mut doc: Document,
) -> Result<DocResult> {
    let doc_id = doc.id.clone();
    let rev = match &doc.rev {
        Some(r) => r.clone(),
        None => {
            return Ok(DocResult {
                ok: false,
                id: doc_id,
                rev: None,
                error: Some("bad_request".into()),
                reason: Some("missing _rev".into()),
            });
        }
    };

    let rev_str = rev.to_string();

    let existing_record = load_doc_record(doc_table, doc_id.as_str())?;

    let existing_tree = existing_record
        .as_ref()
        .map(|(t, _)| t.clone())
        .unwrap_or_default();

    // Build the revision path — use _revisions ancestry if available
    let new_path = if let Some(revisions) = doc.data.get("_revisions") {
        let start = revisions["start"].as_u64().unwrap_or(rev.pos);
        let ids: Vec<String> = revisions["ids"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec![rev.hash.clone()]);

        build_path_from_revs(
            start,
            &ids,
            NodeOpts {
                deleted: doc.deleted,
            },
            RevStatus::Available,
        )
    } else {
        // Fallback: single-node path (no ancestry available)
        RevPath {
            pos: rev.pos,
            tree: RevNode {
                hash: rev.hash.clone(),
                status: RevStatus::Available,
                opts: NodeOpts {
                    deleted: doc.deleted,
                },
                children: vec![],
            },
        }
    };

    // Strip _revisions from data before storing
    if let serde_json::Value::Object(ref mut map) = doc.data {
        map.remove("_revisions");
    }

    let (merged_tree, _) = merge_tree(&existing_tree, &new_path, DEFAULT_REV_LIMIT);

    meta.update_seq += 1;
    let seq = meta.update_seq;

    if let Some((_, old_seq)) = existing_record {
        db_err!(changes_table.remove(old_seq))?;
    }

    let doc_deleted = is_deleted(&merged_tree);

    let doc_bytes = encode_doc_record(&merged_tree, seq)?;
    db_err!(doc_table.insert(doc_id.as_str(), doc_bytes.as_slice()))?;

    let rd = RevDataRecord {
        data: doc.data,
        deleted: doc.deleted,
        attachments: HashMap::new(),
    };
    let rev_bytes = serde_json::to_vec(&rd)?;
    let key = rev_data_key(&doc_id, &rev_str);
    db_err!(rev_table.insert(key.as_str(), rev_bytes.as_slice()))?;

    let change = ChangeRecord {
        doc_id: doc_id.clone(),
        deleted: doc_deleted,
    };
    let change_bytes = serde_json::to_vec(&change)?;
    db_err!(changes_table.insert(seq, change_bytes.as_slice()))?;

    Ok(DocResult {
        ok: true,
        id: doc_id,
        rev: Some(rev_str),
        error: None,
        reason: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_core::document::{AllDocsOptions, BulkDocsOptions, ChangesOptions, GetOptions};

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
    async fn put_and_get() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "Alice"}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["name"], "Alice");
    }

    #[tokio::test]
    async fn update_and_conflict() {
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

        // Successful update
        let doc2 = Document {
            id: "doc1".into(),
            rev: Some(rev1),
            deleted: false,
            data: serde_json::json!({"v": 2}),
            attachments: HashMap::new(),
        };
        let r2 = db
            .bulk_docs(vec![doc2], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(r2[0].ok);

        // Conflict
        let bad = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "wrong".into())),
            deleted: false,
            data: serde_json::json!({"v": 3}),
            attachments: HashMap::new(),
        };
        let r3 = db
            .bulk_docs(vec![bad], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(!r3[0].ok);
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
        let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
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
    async fn changes_feed() {
        let (_dir, db) = temp_db();

        for i in 0..3 {
            let doc = Document {
                id: format!("doc{}", i),
                rev: None,
                deleted: false,
                data: serde_json::json!({"i": i}),
                attachments: HashMap::new(),
            };
            db.bulk_docs(vec![doc], BulkDocsOptions::new())
                .await
                .unwrap();
        }

        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert_eq!(changes.results.len(), 3);
    }

    #[tokio::test]
    async fn all_docs_sorted() {
        let (_dir, db) = temp_db();

        for name in ["charlie", "alice", "bob"] {
            let doc = Document {
                id: name.into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({}),
                attachments: HashMap::new(),
            };
            db.bulk_docs(vec![doc], BulkDocsOptions::new())
                .await
                .unwrap();
        }

        let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(result.rows[0].id, "alice");
        assert_eq!(result.rows[1].id, "bob");
        assert_eq!(result.rows[2].id, "charlie");
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
        assert!(db.get_local("ck1").await.is_err());
    }

    #[tokio::test]
    async fn replication_mode() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "abc".into())),
            deleted: false,
            data: serde_json::json!({"from": "remote"}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.rev.unwrap().to_string(), "1-abc");
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
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        db.destroy().await.unwrap();
        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 0);
        assert_eq!(info.update_seq, Seq::Num(0));
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
        // doc1: 2-def missing, 1-abc exists
        let d1 = &diff.results["doc1"];
        assert!(d1.missing.contains(&"2-def".to_string()));
        assert!(!d1.missing.contains(&"1-abc".to_string()));
        // doc2: entirely missing
        let d2 = &diff.results["doc2"];
        assert!(d2.missing.contains(&"1-xyz".to_string()));
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
        // doc1 should be found
        assert!(response.results[0].docs[0].ok.is_some());
        let ok_doc = response.results[0].docs[0].ok.as_ref().unwrap();
        assert_eq!(ok_doc["name"], "Alice");
        assert!(ok_doc["_revisions"].is_object());
        // nonexistent should error
        assert!(response.results[1].docs[0].error.is_some());
    }

    #[tokio::test]
    async fn auto_generate_id() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: String::new(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"auto": true}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);
        assert!(!results[0].id.is_empty());

        let fetched = db.get(&results[0].id, GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["auto"], true);
    }

    #[tokio::test]
    async fn conflict_put_without_rev_on_existing() {
        let (_dir, db) = temp_db();

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        // Try to create again without rev => conflict
        let doc2 = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 2}),
            attachments: HashMap::new(),
        };
        let r = db
            .bulk_docs(vec![doc2], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(!r[0].ok);
        assert_eq!(r[0].error.as_deref(), Some("conflict"));
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
        assert!(!r[0].ok);
        assert_eq!(r[0].error.as_deref(), Some("not_found"));
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
        let txn = db.db.begin_write().unwrap();
        {
            let mut t = txn.open_table(DOC_TABLE).unwrap();
            t.insert(id, bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    fn raw_record(db: &RedbAdapter, id: &str) -> Vec<u8> {
        let txn = db.db.begin_read().unwrap();
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
    async fn long_history_is_readable_and_writable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.redb");
        let mut rev = {
            let db = RedbAdapter::open(&path, "long").unwrap();
            let mut rev = db
                .bulk_docs(
                    vec![put_doc("d", None, serde_json::json!({"v": 0}))],
                    BulkDocsOptions::new(),
                )
                .await
                .unwrap()[0]
                .rev
                .clone()
                .unwrap();
            for i in 1..200 {
                let r = db
                    .bulk_docs(
                        vec![put_doc("d", Some(&rev), serde_json::json!({"v": i}))],
                        BulkDocsOptions::new(),
                    )
                    .await
                    .unwrap();
                assert!(r[0].ok, "update {} failed: {:?}", i, r[0]);
                rev = r[0].rev.clone().unwrap();
            }
            rev
        };
        let db = RedbAdapter::open(&path, "long").unwrap();
        assert_eq!(
            db.get("d", GetOptions::default()).await.unwrap().data["v"],
            199
        );
        assert_eq!(db.info().await.unwrap().doc_count, 1);
        assert_eq!(
            db.all_docs(AllDocsOptions::new()).await.unwrap().rows.len(),
            1
        );
        let r = db
            .bulk_docs(
                vec![put_doc("d", Some(&rev), serde_json::json!({"v": 200}))],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        assert!(r[0].ok);
        rev = r[0].rev.clone().unwrap();
        assert!(rev.starts_with("201-"));
    }

    #[tokio::test]
    async fn legacy_nested_records_still_load() {
        let (_dir, db) = temp_db();
        // A shallow legacy record and one far deeper than serde_json's
        // default recursion limit (which is what bricked F01 databases).
        for (id, len) in [("shallow", 3u64), ("deep", 400u64)] {
            let tree = linear_tree(len);
            let leaf = format!("{}-{:032x}", len, len);
            write_legacy_record(&db, id, &tree, len);
            let txn = db.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(REV_DATA_TABLE).unwrap();
                let rd =
                    serde_json::to_vec(&serde_json::json!({"data": {"id": id}, "deleted": false}))
                        .unwrap();
                t.insert(rev_data_key(id, &leaf).as_str(), rd.as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();

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
            let txn = db.db.begin_write().unwrap();
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
        assert!(res.is_err(), "{:?}", res);
        assert!(db.get("d", GetOptions::default()).await.is_err());
        let diff = db
            .revs_diff(HashMap::from([(
                "d".to_string(),
                vec!["1-abc".to_string()],
            )]))
            .await;
        assert!(diff.is_err());
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

    #[tokio::test]
    async fn compact_is_noop() {
        let (_dir, db) = temp_db();
        db.compact().await.unwrap();
    }

    #[tokio::test]
    async fn get_nonexistent_returns_not_found() {
        let (_dir, db) = temp_db();
        let result = db.get("nope", GetOptions::default()).await;
        assert!(result.is_err());
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
        assert!(fetched.data["_conflicts"].is_array());
        assert_eq!(fetched.data["_conflicts"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn remove_local_nonexistent() {
        let (_dir, db) = temp_db();
        let result = db.remove_local("nope").await;
        assert!(result.is_err());
    }
}
