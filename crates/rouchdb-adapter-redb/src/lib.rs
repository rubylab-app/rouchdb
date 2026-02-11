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

#[derive(Debug, Serialize, Deserialize)]
struct DocRecord {
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

fn rev_tree_to_serialized(tree: &RevTree) -> Vec<SerializedRevPath> {
    tree.iter()
        .map(|path| SerializedRevPath {
            pos: path.pos,
            tree: rev_node_to_serialized(&path.tree),
        })
        .collect()
}

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

    fn read_meta(&self) -> Result<MetaRecord> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let table = read_txn
            .open_table(META_TABLE)
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?;
        let guard = table
            .get("meta")
            .map_err(|e| RouchError::DatabaseError(e.to_string()))?
            .ok_or_else(|| RouchError::DatabaseError("missing metadata".into()))?;
        let meta: MetaRecord = serde_json::from_slice(guard.value())?;
        Ok(meta)
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

fn parse_rev(rev_str: &str) -> Result<(u64, String)> {
    let (pos_str, hash) = rev_str
        .split_once('-')
        .ok_or_else(|| RouchError::InvalidRev(rev_str.to_string()))?;
    let pos: u64 = pos_str
        .parse()
        .map_err(|_| RouchError::InvalidRev(rev_str.to_string()))?;
    Ok((pos, hash.to_string()))
}

macro_rules! db_err {
    ($e:expr) => {
        $e.map_err(|e| RouchError::DatabaseError(e.to_string()))
    };
}

#[async_trait]
impl Adapter for RedbAdapter {
    async fn info(&self) -> Result<DbInfo> {
        let meta = self.read_meta()?;
        let read_txn = db_err!(self.db.begin_read())?;
        let table = db_err!(read_txn.open_table(DOC_TABLE))?;

        let mut doc_count = 0u64;
        let iter = db_err!(table.iter())?;
        for entry in iter {
            let entry = db_err!(entry)?;
            let record: DocRecord = serde_json::from_slice(entry.1.value())?;
            let tree = serialized_to_rev_tree(&record.rev_tree);
            if !is_deleted(&tree) {
                doc_count += 1;
            }
        }

        Ok(DbInfo {
            db_name: self.name.clone(),
            doc_count,
            update_seq: Seq::Num(meta.update_seq),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        let read_txn = db_err!(self.db.begin_read())?;
        let doc_table = db_err!(read_txn.open_table(DOC_TABLE))?;
        let rev_table = db_err!(read_txn.open_table(REV_DATA_TABLE))?;

        let guard =
            db_err!(doc_table.get(id))?.ok_or_else(|| RouchError::NotFound(id.to_string()))?;
        let record: DocRecord = serde_json::from_slice(guard.value())?;
        let tree = serialized_to_rev_tree(&record.rev_tree);

        let target_rev = if let Some(ref rev_str) = opts.rev {
            rev_str.clone()
        } else {
            winning_rev(&tree)
                .ok_or_else(|| RouchError::NotFound(id.to_string()))?
                .to_string()
        };

        let key = rev_data_key(id, &target_rev);
        let rev_guard = db_err!(rev_table.get(key.as_str()))?;

        let (data, deleted) = if let Some(guard) = rev_guard {
            let rd: RevDataRecord = serde_json::from_slice(guard.value())?;
            (rd.data, rd.deleted)
        } else {
            (serde_json::Value::Object(serde_json::Map::new()), false)
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

        let iter = db_err!(doc_table.iter())?;
        for entry in iter {
            let entry = db_err!(entry)?;
            let doc_id = entry.0.value().to_string();
            let record: DocRecord = serde_json::from_slice(entry.1.value())?;
            let tree = serialized_to_rev_tree(&record.rev_tree);

            let winner = match winning_rev(&tree) {
                Some(w) => w,
                None => continue,
            };
            let deleted = is_deleted(&tree);

            if deleted && opts.keys.is_none() {
                continue;
            }

            // Apply key range filters
            if opts.keys.is_none() && opts.key.is_none() {
                if let Some(ref start) = opts.start_key
                    && doc_id.as_str() < start.as_str()
                {
                    continue;
                }
                if let Some(ref end) = opts.end_key {
                    if opts.inclusive_end {
                        if doc_id.as_str() > end.as_str() {
                            continue;
                        }
                    } else if doc_id.as_str() >= end.as_str() {
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
                db_err!(rev_table.get(key.as_str()))?.map(|guard| {
                    let rd: RevDataRecord = serde_json::from_slice(guard.value()).unwrap();
                    let mut obj = match rd.data {
                        serde_json::Value::Object(m) => m,
                        _ => serde_json::Map::new(),
                    };
                    obj.insert("_id".into(), serde_json::Value::String(doc_id.clone()));
                    obj.insert("_rev".into(), serde_json::Value::String(rev_str));
                    serde_json::Value::Object(obj)
                })
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

        let total_rows = rows.len() as u64;
        let skip = opts.skip as usize;
        if skip > 0 {
            rows = rows.into_iter().skip(skip).collect();
        }
        if let Some(limit) = opts.limit {
            rows.truncate(limit as usize);
        }

        let update_seq = if opts.update_seq {
            let meta = self.read_meta()?;
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

        let start = opts.since.as_num() + 1;
        let iter = db_err!(changes_table.range(start..))?;

        let entries: Vec<_> = iter
            .filter_map(|e| e.ok())
            .map(|e| {
                (
                    e.0.value(),
                    serde_json::from_slice::<ChangeRecord>(e.1.value()).unwrap(),
                )
            })
            .collect();

        let iter: Box<dyn Iterator<Item = &(u64, ChangeRecord)>> = if opts.descending {
            Box::new(entries.iter().rev())
        } else {
            Box::new(entries.iter())
        };

        for (seq, change) in iter {
            if let Some(ref doc_ids) = opts.doc_ids
                && !doc_ids.contains(&change.doc_id)
            {
                continue;
            }

            let rev_str = db_err!(doc_table.get(change.doc_id.as_str()))?
                .and_then(|guard| {
                    let record: DocRecord = serde_json::from_slice(guard.value()).ok()?;
                    let tree = serialized_to_rev_tree(&record.rev_tree);
                    winning_rev(&tree).map(|r| r.to_string())
                })
                .unwrap_or_default();

            let doc = if opts.include_docs && !rev_str.is_empty() {
                let key = rev_data_key(&change.doc_id, &rev_str);
                db_err!(rev_table.get(key.as_str()))?.map(|guard| {
                    let rd: RevDataRecord = serde_json::from_slice(guard.value()).unwrap();
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
                    serde_json::Value::Object(obj)
                })
            } else {
                None
            };

            // Build changes list based on style
            let changes_list = if opts.style == ChangesStyle::AllDocs {
                // Fetch all leaf revisions for AllDocs style
                if let Some(guard) = db_err!(doc_table.get(change.doc_id.as_str()))? {
                    let record: DocRecord = serde_json::from_slice(guard.value())?;
                    let tree = serialized_to_rev_tree(&record.rev_tree);
                    collect_leaves(&tree)
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
                if let Some(guard) = db_err!(doc_table.get(change.doc_id.as_str()))? {
                    let record: DocRecord = serde_json::from_slice(guard.value())?;
                    let tree = serialized_to_rev_tree(&record.rev_tree);
                    let c = collect_conflicts(&tree);
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

            let stored = db_err!(doc_table.get(doc_id.as_str()))?;
            let tree = stored.as_ref().and_then(|guard| {
                let record: DocRecord = serde_json::from_slice(guard.value()).ok()?;
                Some(serialized_to_rev_tree(&record.rev_tree))
            });

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

            match db_err!(doc_table.get(item.id.as_str()))? {
                Some(guard) => {
                    let record: DocRecord = serde_json::from_slice(guard.value())?;
                    let tree = serialized_to_rev_tree(&record.rev_tree);

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
        _doc_id: &str,
        _att_id: &str,
        _rev: &str,
        _data: Vec<u8>,
        _content_type: &str,
    ) -> Result<DocResult> {
        // TODO: implement attachment support
        Err(RouchError::BadRequest(
            "attachments not yet implemented for redb".into(),
        ))
    }

    async fn get_attachment(
        &self,
        _doc_id: &str,
        _att_id: &str,
        _opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        Err(RouchError::BadRequest(
            "attachments not yet implemented for redb".into(),
        ))
    }

    async fn remove_attachment(
        &self,
        _doc_id: &str,
        _att_id: &str,
        _rev: &str,
    ) -> Result<DocResult> {
        Err(RouchError::BadRequest(
            "attachments not yet implemented for redb".into(),
        ))
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

    // Load existing record (clone data out of access guard immediately)
    let existing_record: Option<DocRecord> = {
        let existing = db_err!(doc_table.get(doc_id.as_str()))?;
        existing
            .as_ref()
            .and_then(|g| serde_json::from_slice(g.value()).ok())
    };

    let existing_tree = existing_record
        .as_ref()
        .map(|r| serialized_to_rev_tree(&r.rev_tree))
        .unwrap_or_default();

    // Conflict check
    if let Some(ref record) = existing_record {
        let tree = serialized_to_rev_tree(&record.rev_tree);
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
            (None, Some(_)) => {
                if !is_deleted(&tree) {
                    return Ok(DocResult {
                        ok: false,
                        id: doc_id,
                        rev: None,
                        error: Some("conflict".into()),
                        reason: Some("Document update conflict".into()),
                    });
                }
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
    if let Some(ref record) = existing_record {
        let _ = db_err!(changes_table.remove(record.seq));
    }

    // Save doc record
    let new_record = DocRecord {
        rev_tree: rev_tree_to_serialized(&merged_tree),
        seq,
    };
    let doc_bytes = serde_json::to_vec(&new_record)?;
    db_err!(doc_table.insert(doc_id.as_str(), doc_bytes.as_slice()))?;

    // Save rev data
    let rd = RevDataRecord {
        data: doc.data,
        deleted: doc.deleted,
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

    let existing_record: Option<DocRecord> = {
        let existing = db_err!(doc_table.get(doc_id.as_str()))?;
        existing
            .as_ref()
            .and_then(|g| serde_json::from_slice(g.value()).ok())
    };

    let existing_tree = existing_record
        .as_ref()
        .map(|r| serialized_to_rev_tree(&r.rev_tree))
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

    if let Some(ref record) = existing_record {
        let _ = db_err!(changes_table.remove(record.seq));
    }

    let doc_deleted = is_deleted(&merged_tree);

    let new_record = DocRecord {
        rev_tree: rev_tree_to_serialized(&merged_tree),
        seq,
    };
    let doc_bytes = serde_json::to_vec(&new_record)?;
    db_err!(doc_table.insert(doc_id.as_str(), doc_bytes.as_slice()))?;

    let rd = RevDataRecord {
        data: doc.data,
        deleted: doc.deleted,
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
