use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use md5::{Digest, Md5};
use tokio::sync::RwLock;
use uuid::Uuid;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::{Result, RouchError};
use rouchdb_core::merge::{collect_conflicts, is_deleted, merge_tree, winning_rev};
use rouchdb_core::rev_tree::{
    NodeOpts, RevPath, RevStatus, RevTree, build_path_from_revs, collect_leaves, find_rev_ancestry,
    rev_exists,
};

const DEFAULT_REV_LIMIT: u64 = 1000;

// ---------------------------------------------------------------------------
// Internal storage types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredDoc {
    rev_tree: RevTree,
    /// Map from "pos-hash" to the document data at that revision.
    rev_data: HashMap<String, serde_json::Value>,
    /// Map from "pos-hash" to the deleted flag at that revision.
    rev_deleted: HashMap<String, bool>,
    /// Current sequence number for this document.
    seq: u64,
}

#[derive(Debug)]
struct Inner {
    name: String,
    /// Documents keyed by ID.
    docs: HashMap<String, StoredDoc>,
    /// Sequence counter (monotonically increasing).
    update_seq: u64,
    /// Changes log: seq -> (doc_id, was_deleted).
    changes: BTreeMap<u64, (String, bool)>,
    /// Local (non-replicated) documents.
    local_docs: HashMap<String, serde_json::Value>,
    /// Attachment data keyed by digest.
    attachments: HashMap<String, Vec<u8>>,
}

/// In-memory adapter for RouchDB. All data is held in RAM.
#[derive(Debug, Clone)]
pub struct MemoryAdapter {
    inner: Arc<RwLock<Inner>>,
}

impl MemoryAdapter {
    pub fn new(name: &str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                name: name.to_string(),
                docs: HashMap::new(),
                update_seq: 0,
                changes: BTreeMap::new(),
                local_docs: HashMap::new(),
                attachments: HashMap::new(),
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Generate a revision hash from the document content.
fn generate_rev_hash(
    doc_data: &serde_json::Value,
    deleted: bool,
    prev_rev: Option<&str>,
) -> String {
    let mut hasher = Md5::new();
    // Include the previous revision in the hash for determinism
    if let Some(prev) = prev_rev {
        hasher.update(prev.as_bytes());
    }
    hasher.update(if deleted { b"1" } else { b"0" });
    let serialized = serde_json::to_string(doc_data).unwrap_or_default();
    hasher.update(serialized.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn rev_string(pos: u64, hash: &str) -> String {
    format!("{}-{}", pos, hash)
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

fn compute_attachment_digest(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    let hash = hasher.finalize();
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
    format!("md5-{}", b64)
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Adapter for MemoryAdapter {
    async fn info(&self) -> Result<DbInfo> {
        let inner = self.inner.read().await;
        let doc_count = inner
            .docs
            .values()
            .filter(|d| {
                // Count only non-deleted documents
                !is_deleted(&d.rev_tree)
            })
            .count() as u64;

        Ok(DbInfo {
            db_name: inner.name.clone(),
            doc_count,
            update_seq: Seq::Num(inner.update_seq),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        let inner = self.inner.read().await;
        let stored = inner
            .docs
            .get(id)
            .ok_or_else(|| RouchError::NotFound(id.to_string()))?;

        // Determine which revision to return
        let mut target_rev = if let Some(ref rev_str) = opts.rev {
            rev_str.clone()
        } else {
            // Use the winning revision
            let winner = winning_rev(&stored.rev_tree)
                .ok_or_else(|| RouchError::NotFound(id.to_string()))?;
            winner.to_string()
        };

        // latest: if requested rev isn't a leaf, return the latest leaf instead
        if opts.latest && opts.rev.is_some() {
            let leaves = collect_leaves(&stored.rev_tree);
            let is_leaf = leaves.iter().any(|l| l.rev_string() == target_rev);
            if !is_leaf && let Some(leaf) = leaves.first() {
                target_rev = leaf.rev_string();
            }
        }

        // Get the data for this revision
        let data = stored
            .rev_data
            .get(&target_rev)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        let deleted = stored
            .rev_deleted
            .get(&target_rev)
            .copied()
            .unwrap_or(false);

        // If the winning rev is deleted and no specific rev was requested, it's "not found"
        if deleted && opts.rev.is_none() {
            return Err(RouchError::NotFound(id.to_string()));
        }

        let (pos, hash) = parse_rev(&target_rev)?;
        let rev = Revision::new(pos, hash);

        let mut doc = Document {
            id: id.to_string(),
            rev: Some(rev),
            deleted,
            data,
            attachments: HashMap::new(),
        };

        // Add conflicts if requested
        if opts.conflicts {
            let conflicts = collect_conflicts(&stored.rev_tree);
            if !conflicts.is_empty() {
                let conflict_list: Vec<serde_json::Value> = conflicts
                    .iter()
                    .map(|c| serde_json::Value::String(c.to_string()))
                    .collect();
                if let serde_json::Value::Object(ref mut map) = doc.data {
                    map.insert(
                        "_conflicts".to_string(),
                        serde_json::Value::Array(conflict_list),
                    );
                }
            }
        }

        // Add revs_info if requested
        if opts.revs_info {
            use rouchdb_core::rev_tree::traverse_rev_tree;
            let mut revs_info = Vec::new();
            traverse_rev_tree(&stored.rev_tree, |node_pos, node, _root_pos| {
                let rev_str = format!("{}-{}", node_pos, node.hash);
                let status = if node.opts.deleted {
                    "deleted"
                } else {
                    match node.status {
                        rouchdb_core::rev_tree::RevStatus::Available => "available",
                        rouchdb_core::rev_tree::RevStatus::Missing => "missing",
                    }
                };
                revs_info.push(RevInfo {
                    rev: rev_str,
                    status: status.to_string(),
                });
            });
            // Sort by pos descending (newest first)
            revs_info.sort_by(|a, b| {
                let a_pos: u64 = a.rev.split('-').next().unwrap_or("0").parse().unwrap_or(0);
                let b_pos: u64 = b.rev.split('-').next().unwrap_or("0").parse().unwrap_or(0);
                b_pos.cmp(&a_pos)
            });
            if let serde_json::Value::Object(ref mut map) = doc.data {
                map.insert(
                    "_revs_info".to_string(),
                    serde_json::to_value(&revs_info).unwrap(),
                );
            }
        }

        Ok(doc)
    }

    async fn bulk_docs(
        &self,
        docs: Vec<Document>,
        opts: BulkDocsOptions,
    ) -> Result<Vec<DocResult>> {
        let mut inner = self.inner.write().await;
        let mut results = Vec::with_capacity(docs.len());

        for doc in docs {
            let result = if opts.new_edits {
                process_doc_new_edits(&mut inner, doc)
            } else {
                process_doc_replication(&mut inner, doc)
            };
            results.push(result);
        }

        Ok(results)
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let inner = self.inner.read().await;

        // Collect all doc IDs sorted
        let mut doc_ids: Vec<&String> = inner.docs.keys().collect();
        doc_ids.sort();

        if opts.descending {
            doc_ids.reverse();
        }

        // If specific keys are requested, use those instead
        let target_keys: Vec<String> = if let Some(ref keys) = opts.keys {
            keys.clone()
        } else if let Some(ref key) = opts.key {
            vec![key.clone()]
        } else {
            doc_ids.iter().map(|k| (*k).clone()).collect()
        };

        let mut rows = Vec::new();

        for key in &target_keys {
            // Apply key range filters if no specific keys were given
            if opts.keys.is_none() && opts.key.is_none() {
                if let Some(ref start) = opts.start_key
                    && ((!opts.descending && key.as_str() < start.as_str())
                        || (opts.descending && key.as_str() > start.as_str()))
                {
                    continue;
                }
                if let Some(ref end) = opts.end_key {
                    if opts.inclusive_end {
                        if (!opts.descending && key.as_str() > end.as_str())
                            || (opts.descending && key.as_str() < end.as_str())
                        {
                            continue;
                        }
                    } else if (!opts.descending && key.as_str() >= end.as_str())
                        || (opts.descending && key.as_str() <= end.as_str())
                    {
                        continue;
                    }
                }
            }

            if let Some(stored) = inner.docs.get(key.as_str()) {
                let winner = match winning_rev(&stored.rev_tree) {
                    Some(w) => w,
                    None => continue,
                };
                let deleted = is_deleted(&stored.rev_tree);

                // Skip deleted docs unless specific keys were requested
                if deleted && opts.keys.is_none() {
                    continue;
                }

                let doc_json = if opts.include_docs && !deleted {
                    let rev_str = winner.to_string();
                    stored.rev_data.get(&rev_str).map(|data| {
                        let mut obj = match data {
                            serde_json::Value::Object(m) => m.clone(),
                            _ => serde_json::Map::new(),
                        };
                        obj.insert("_id".into(), serde_json::Value::String(key.clone()));
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str));
                        // Include conflicts if requested
                        if opts.conflicts {
                            let conflicts = collect_conflicts(&stored.rev_tree);
                            if !conflicts.is_empty() {
                                let conflict_list: Vec<serde_json::Value> = conflicts
                                    .iter()
                                    .map(|c| serde_json::Value::String(c.to_string()))
                                    .collect();
                                obj.insert(
                                    "_conflicts".to_string(),
                                    serde_json::Value::Array(conflict_list),
                                );
                            }
                        }
                        serde_json::Value::Object(obj)
                    })
                } else {
                    None
                };

                rows.push(AllDocsRow {
                    id: key.clone(),
                    key: key.clone(),
                    value: AllDocsRowValue {
                        rev: winner.to_string(),
                        deleted: if deleted { Some(true) } else { None },
                    },
                    doc: doc_json,
                });
            } else if opts.keys.is_some() {
                // For specific key lookups, include missing keys as errors
                // (CouchDB returns {"key":"x","error":"not_found"})
                // We skip these for now — they don't fit our row struct cleanly
            }
        }

        // Apply skip and limit
        let total_rows = rows.len() as u64;
        let skip = opts.skip as usize;
        if skip > 0 {
            rows = rows.into_iter().skip(skip).collect();
        }
        if let Some(limit) = opts.limit {
            rows.truncate(limit as usize);
        }

        let update_seq = if opts.update_seq {
            Some(Seq::Num(inner.update_seq))
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
        let inner = self.inner.read().await;

        let mut results = Vec::new();

        // Iterate changes after `since`
        let range = (opts.since.as_num() + 1)..;
        let iter: Box<dyn Iterator<Item = (&u64, &(String, bool))>> = if opts.descending {
            Box::new(
                inner
                    .changes
                    .range(range)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            )
        } else {
            Box::new(inner.changes.range(range))
        };

        for (seq, (doc_id, deleted)) in iter {
            // Filter by doc_ids if specified
            if let Some(ref doc_ids) = opts.doc_ids
                && !doc_ids.contains(doc_id)
            {
                continue;
            }

            let stored = inner.docs.get(doc_id);
            let rev_str = stored
                .and_then(|s| winning_rev(&s.rev_tree))
                .map(|r| r.to_string())
                .unwrap_or_default();

            let doc = if opts.include_docs {
                stored.and_then(|s| {
                    s.rev_data.get(&rev_str).map(|data| {
                        let mut obj = match data {
                            serde_json::Value::Object(m) => m.clone(),
                            _ => serde_json::Map::new(),
                        };
                        obj.insert("_id".into(), serde_json::Value::String(doc_id.clone()));
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str.clone()));
                        if *deleted {
                            obj.insert("_deleted".into(), serde_json::Value::Bool(true));
                        }
                        serde_json::Value::Object(obj)
                    })
                })
            } else {
                None
            };

            // Build changes list based on style
            let changes_list = if opts.style == ChangesStyle::AllDocs {
                if let Some(s) = stored {
                    collect_leaves(&s.rev_tree)
                        .iter()
                        .filter(|l| !s.rev_deleted.get(&l.rev_string()).copied().unwrap_or(false))
                        .map(|l| ChangeRev {
                            rev: l.rev_string(),
                        })
                        .collect()
                } else {
                    vec![ChangeRev { rev: rev_str }]
                }
            } else {
                vec![ChangeRev { rev: rev_str }]
            };

            // Collect conflicts if requested
            let conflicts = if opts.conflicts {
                stored
                    .map(|s| {
                        let c = collect_conflicts(&s.rev_tree);
                        if c.is_empty() {
                            None
                        } else {
                            Some(c.iter().map(|r| r.to_string()).collect())
                        }
                    })
                    .unwrap_or(None)
            } else {
                None
            };

            results.push(ChangeEvent {
                seq: Seq::Num(*seq),
                id: doc_id.clone(),
                changes: changes_list,
                deleted: *deleted,
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
        let inner = self.inner.read().await;
        let mut results = HashMap::new();

        for (doc_id, rev_list) in revs {
            let mut missing = Vec::new();
            let mut possible_ancestors = Vec::new();

            let stored = inner.docs.get(&doc_id);

            for rev_str in &rev_list {
                let (pos, hash) = parse_rev(rev_str)?;

                let exists = stored
                    .map(|s| rev_exists(&s.rev_tree, pos, &hash))
                    .unwrap_or(false);

                if !exists {
                    missing.push(rev_str.clone());

                    // Find possible ancestors (existing revisions with lower pos)
                    if let Some(stored) = stored {
                        let leaves = collect_leaves(&stored.rev_tree);
                        for leaf in &leaves {
                            if leaf.pos < pos {
                                possible_ancestors.push(leaf.rev_string());
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
        let inner = self.inner.read().await;
        let mut results = Vec::new();

        for item in docs {
            let mut bulk_docs = Vec::new();

            match inner.docs.get(&item.id) {
                Some(stored) => {
                    let rev_str = if let Some(ref rev) = item.rev {
                        rev.clone()
                    } else {
                        match winning_rev(&stored.rev_tree) {
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

                    if let Some(data) = stored.rev_data.get(&rev_str) {
                        let deleted = stored.rev_deleted.get(&rev_str).copied().unwrap_or(false);
                        let mut obj = match data {
                            serde_json::Value::Object(m) => m.clone(),
                            _ => serde_json::Map::new(),
                        };
                        obj.insert("_id".into(), serde_json::Value::String(item.id.clone()));
                        obj.insert("_rev".into(), serde_json::Value::String(rev_str.clone()));
                        if deleted {
                            obj.insert("_deleted".into(), serde_json::Value::Bool(true));
                        }

                        // Include _revisions for replication
                        if let Ok((pos, ref hash)) = parse_rev(&rev_str)
                            && let Some(ancestry) = find_rev_ancestry(&stored.rev_tree, pos, hash)
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

        let mut inner = self.inner.write().await;

        // Store the attachment data
        inner.attachments.insert(digest.clone(), data);

        // Get or create the document
        let stored = inner
            .docs
            .get(doc_id)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;

        // Verify the rev matches
        let winner = winning_rev(&stored.rev_tree)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        if winner.to_string() != rev {
            return Err(RouchError::Conflict);
        }

        // Get current doc data and add attachment
        let doc_data = stored
            .rev_data
            .get(rev)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        // Build updated document with attachment metadata
        let att_meta = AttachmentMeta {
            content_type: content_type.to_string(),
            digest: digest.clone(),
            length,
            stub: true,
            data: None,
        };

        let doc = Document {
            id: doc_id.to_string(),
            rev: Some(winner.clone()),
            deleted: false,
            data: doc_data.clone(),
            attachments: {
                let mut atts = HashMap::new();
                atts.insert(att_id.to_string(), att_meta);
                atts
            },
        };

        // Process as a normal edit
        let result = process_doc_new_edits(&mut inner, doc);
        Ok(result)
    }

    async fn get_attachment(
        &self,
        doc_id: &str,
        att_id: &str,
        opts: GetAttachmentOptions,
    ) -> Result<Vec<u8>> {
        let inner = self.inner.read().await;

        let stored = inner
            .docs
            .get(doc_id)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;

        let rev_str = if let Some(ref rev) = opts.rev {
            rev.clone()
        } else {
            winning_rev(&stored.rev_tree)
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?
                .to_string()
        };

        // Look for attachment metadata in the doc data
        // For now, look up by digest in our attachment store
        // We'd need to track which attachments belong to which doc/rev
        // For simplicity, search through our attachment map
        let _data = stored.rev_data.get(&rev_str);

        // TODO: proper attachment tracking per revision
        Err(RouchError::NotFound(format!(
            "attachment {}/{}",
            doc_id, att_id
        )))
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        let _ = att_id; // attachment tracking is simplified in memory adapter
        let mut inner = self.inner.write().await;

        let stored = inner
            .docs
            .get(doc_id)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;

        let winner = winning_rev(&stored.rev_tree)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        if winner.to_string() != rev {
            return Err(RouchError::Conflict);
        }

        let doc_data = stored
            .rev_data
            .get(rev)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

        // Create a new revision (attachment removal is a document update)
        let doc = Document {
            id: doc_id.to_string(),
            rev: Some(winner.clone()),
            deleted: false,
            data: doc_data,
            attachments: HashMap::new(),
        };

        let result = process_doc_new_edits(&mut inner, doc);
        Ok(result)
    }

    async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
        let inner = self.inner.read().await;
        inner
            .local_docs
            .get(id)
            .cloned()
            .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))
    }

    async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
        let mut inner = self.inner.write().await;
        inner.local_docs.insert(id.to_string(), doc);
        Ok(())
    }

    async fn remove_local(&self, id: &str) -> Result<()> {
        let mut inner = self.inner.write().await;
        inner
            .local_docs
            .remove(id)
            .ok_or_else(|| RouchError::NotFound(format!("_local/{}", id)))?;
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        let mut inner = self.inner.write().await;

        for stored in inner.docs.values_mut() {
            let leaves = collect_leaves(&stored.rev_tree);
            let leaf_revs: std::collections::HashSet<String> =
                leaves.iter().map(|l| l.rev_string()).collect();

            // Remove data for non-leaf revisions
            stored.rev_data.retain(|k, _| leaf_revs.contains(k));
            stored.rev_deleted.retain(|k, _| leaf_revs.contains(k));
        }

        Ok(())
    }

    async fn destroy(&self) -> Result<()> {
        let mut inner = self.inner.write().await;
        inner.docs.clear();
        inner.changes.clear();
        inner.local_docs.clear();
        inner.attachments.clear();
        inner.update_seq = 0;
        Ok(())
    }

    async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        let mut inner = self.inner.write().await;
        let mut purged = HashMap::new();
        let mut docs_to_remove = Vec::new();

        for (doc_id, revs) in req {
            let mut purged_revs = Vec::new();
            if let Some(stored) = inner.docs.get_mut(&doc_id) {
                for rev_str in &revs {
                    if stored.rev_data.remove(rev_str).is_some() {
                        stored.rev_deleted.remove(rev_str);
                        purged_revs.push(rev_str.clone());

                        // Also prune the revision from the rev_tree so that
                        // winning_rev(), collect_conflicts(), and replication
                        // don't reference purged revisions.
                        if let Some((pos, hash)) = rev_str.split_once('-')
                            && let Ok(pos) = pos.parse::<u64>()
                        {
                            prune_leaf_from_tree(&mut stored.rev_tree, pos, hash);
                        }
                    }
                }
                // Remove empty rev_tree paths after pruning
                stored.rev_tree.retain(|p| !is_tree_empty(&p.tree));

                if stored.rev_data.is_empty() {
                    docs_to_remove.push((doc_id.clone(), stored.seq));
                }
            }
            if !purged_revs.is_empty() {
                purged.insert(doc_id, purged_revs);
            }
        }

        for (doc_id, seq) in docs_to_remove {
            inner.changes.remove(&seq);
            inner.docs.remove(&doc_id);
        }

        Ok(PurgeResponse {
            purge_seq: Some(inner.update_seq),
            purged,
        })
    }

    async fn get_security(&self) -> Result<SecurityDocument> {
        let inner = self.inner.read().await;
        match inner.local_docs.get("_security") {
            Some(val) => serde_json::from_value(val.clone())
                .map_err(|e| RouchError::DatabaseError(e.to_string())),
            None => Ok(SecurityDocument::default()),
        }
    }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        let mut inner = self.inner.write().await;
        let val = serde_json::to_value(&doc)?;
        inner.local_docs.insert("_security".to_string(), val);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Document processing (new_edits = true)
// ---------------------------------------------------------------------------

fn process_doc_new_edits(inner: &mut Inner, doc: Document) -> DocResult {
    let doc_id = if doc.id.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        doc.id.clone()
    };

    let existing = inner.docs.get(&doc_id);

    // Check for conflicts: if the doc has a _rev, it must match the winning rev
    if let Some(stored) = existing {
        let winner = winning_rev(&stored.rev_tree);

        match (&doc.rev, &winner) {
            (Some(provided_rev), Some(current_winner)) => {
                if provided_rev.to_string() != current_winner.to_string() {
                    return DocResult {
                        ok: false,
                        id: doc_id,
                        rev: None,
                        error: Some("conflict".into()),
                        reason: Some("Document update conflict".into()),
                    };
                }
            }
            (None, Some(_)) => {
                // Trying to create a doc that already exists (and isn't deleted)
                if !is_deleted(&stored.rev_tree) {
                    return DocResult {
                        ok: false,
                        id: doc_id,
                        rev: None,
                        error: Some("conflict".into()),
                        reason: Some("Document update conflict".into()),
                    };
                }
                // If winner is deleted, allow creating a new doc at the same ID
            }
            _ => {}
        }
    } else if doc.rev.is_some() {
        // Updating a doc that doesn't exist
        return DocResult {
            ok: false,
            id: doc_id,
            rev: None,
            error: Some("not_found".into()),
            reason: Some("missing".into()),
        };
    }

    // Generate new revision
    let new_pos = doc.rev.as_ref().map(|r| r.pos + 1).unwrap_or(1);
    let prev_rev_str = doc.rev.as_ref().map(|r| r.to_string());
    let new_hash = generate_rev_hash(&doc.data, doc.deleted, prev_rev_str.as_deref());
    let new_rev_str = rev_string(new_pos, &new_hash);

    // Build the revision path for merging
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

    // Merge into existing tree or create new one
    let existing_tree = existing.map(|s| s.rev_tree.clone()).unwrap_or_default();

    let (merged_tree, _merge_result) = merge_tree(&existing_tree, &new_path, DEFAULT_REV_LIMIT);

    // Update sequence
    inner.update_seq += 1;
    let seq = inner.update_seq;

    // Remove old change entry for this doc (each doc has only one entry in changes)
    if let Some(existing) = inner.docs.get(&doc_id) {
        inner.changes.remove(&existing.seq);
    }

    // Store or update the document
    let stored = inner
        .docs
        .entry(doc_id.clone())
        .or_insert_with(|| StoredDoc {
            rev_tree: Vec::new(),
            rev_data: HashMap::new(),
            rev_deleted: HashMap::new(),
            seq: 0,
        });

    stored.rev_tree = merged_tree;
    stored.rev_data.insert(new_rev_str.clone(), doc.data);
    stored.rev_deleted.insert(new_rev_str.clone(), doc.deleted);
    stored.seq = seq;

    // Record in changes
    inner.changes.insert(seq, (doc_id.clone(), doc.deleted));

    DocResult {
        ok: true,
        id: doc_id,
        rev: Some(new_rev_str),
        error: None,
        reason: None,
    }
}

// ---------------------------------------------------------------------------
// Document processing (new_edits = false, replication mode)
// ---------------------------------------------------------------------------

fn process_doc_replication(inner: &mut Inner, mut doc: Document) -> DocResult {
    let doc_id = doc.id.clone();
    let rev = match &doc.rev {
        Some(r) => r.clone(),
        None => {
            return DocResult {
                ok: false,
                id: doc_id,
                rev: None,
                error: Some("bad_request".into()),
                reason: Some("missing _rev".into()),
            };
        }
    };

    let rev_str = rev.to_string();

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
            tree: rouchdb_core::rev_tree::RevNode {
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

    // Merge into existing tree
    let existing_tree = inner
        .docs
        .get(&doc_id)
        .map(|s| s.rev_tree.clone())
        .unwrap_or_default();

    let (merged_tree, _merge_result) = merge_tree(&existing_tree, &new_path, DEFAULT_REV_LIMIT);

    // Update sequence
    inner.update_seq += 1;
    let seq = inner.update_seq;

    // Remove old change entry
    if let Some(existing) = inner.docs.get(&doc_id) {
        inner.changes.remove(&existing.seq);
    }

    let is_doc_deleted = is_deleted(&merged_tree);

    let stored = inner
        .docs
        .entry(doc_id.clone())
        .or_insert_with(|| StoredDoc {
            rev_tree: Vec::new(),
            rev_data: HashMap::new(),
            rev_deleted: HashMap::new(),
            seq: 0,
        });

    stored.rev_tree = merged_tree;
    stored.rev_data.insert(rev_str.clone(), doc.data);
    stored.rev_deleted.insert(rev_str.clone(), doc.deleted);
    stored.seq = seq;

    inner.changes.insert(seq, (doc_id.clone(), is_doc_deleted));

    DocResult {
        ok: true,
        id: doc_id,
        rev: Some(rev_str),
        error: None,
        reason: None,
    }
}

// ---------------------------------------------------------------------------
// Rev-tree pruning helpers for purge
// ---------------------------------------------------------------------------

use rouchdb_core::rev_tree::RevNode;

/// Remove a specific leaf node from the rev tree. If the node at (pos, hash)
/// is a leaf (no children), it's removed from its parent's children list.
fn prune_leaf_from_tree(tree: &mut RevTree, target_pos: u64, target_hash: &str) {
    for path in tree.iter_mut() {
        prune_leaf_from_node(&mut path.tree, path.pos, target_pos, target_hash);
    }
}

/// Recursively remove a matching leaf node from the subtree.
/// Returns true if the node at this level should be removed (it matched and was a leaf).
fn prune_leaf_from_node(node: &mut RevNode, current_pos: u64, target_pos: u64, target_hash: &str) {
    // Remove matching children that are leaves
    node.children.retain(|child| {
        let child_pos = current_pos + 1;
        !(child_pos == target_pos && child.hash == target_hash && child.children.is_empty())
    });

    // Recurse into remaining children
    for child in node.children.iter_mut() {
        prune_leaf_from_node(child, current_pos + 1, target_pos, target_hash);
    }
}

/// Check if a rev tree node is effectively empty (no children and no useful data).
/// Used to clean up orphaned root paths after leaf pruning.
fn is_tree_empty(node: &RevNode) -> bool {
    node.children.is_empty() && node.hash.is_empty()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_core::document::{AllDocsOptions, BulkDocsOptions, ChangesOptions, GetOptions};

    async fn new_db() -> MemoryAdapter {
        MemoryAdapter::new("test")
    }

    #[tokio::test]
    async fn info_empty_db() {
        let db = new_db().await;
        let info = db.info().await.unwrap();
        assert_eq!(info.db_name, "test");
        assert_eq!(info.doc_count, 0);
        assert_eq!(info.update_seq, Seq::Num(0));
    }

    #[tokio::test]
    async fn put_and_get_document() {
        let db = new_db().await;

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
        assert_eq!(results[0].id, "doc1");
        assert!(results[0].rev.is_some());

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.id, "doc1");
        assert_eq!(fetched.data["name"], "Alice");
        assert!(fetched.rev.is_some());
    }

    #[tokio::test]
    async fn update_document() {
        let db = new_db().await;

        // Create
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
        let rev1 = results[0].rev.clone().unwrap();

        // Update
        let rev_parsed: Revision = rev1.parse().unwrap();
        let doc2 = Document {
            id: "doc1".into(),
            rev: Some(rev_parsed),
            deleted: false,
            data: serde_json::json!({"name": "Bob"}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc2], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["name"], "Bob");
    }

    #[tokio::test]
    async fn conflict_on_wrong_rev() {
        let db = new_db().await;

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

        // Try updating with wrong rev
        let doc2 = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "wronghash".into())),
            deleted: false,
            data: serde_json::json!({"v": 2}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc2], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(!results[0].ok);
        assert_eq!(results[0].error.as_deref(), Some("conflict"));
    }

    #[tokio::test]
    async fn delete_document() {
        let db = new_db().await;

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
        let rev1: Revision = results[0].rev.clone().unwrap().parse().unwrap();

        // Delete
        let del = Document {
            id: "doc1".into(),
            rev: Some(rev1),
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);

        // Get should fail
        let err = db.get("doc1", GetOptions::default()).await;
        assert!(err.is_err());

        // Info should show 0 docs
        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 0);
    }

    #[tokio::test]
    async fn all_docs() {
        let db = new_db().await;

        for name in ["charlie", "alice", "bob"] {
            let doc = Document {
                id: name.into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"name": name}),
                attachments: HashMap::new(),
            };
            db.bulk_docs(vec![doc], BulkDocsOptions::new())
                .await
                .unwrap();
        }

        let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(result.total_rows, 3);
        // Should be sorted alphabetically
        assert_eq!(result.rows[0].id, "alice");
        assert_eq!(result.rows[1].id, "bob");
        assert_eq!(result.rows[2].id, "charlie");
    }

    #[tokio::test]
    async fn all_docs_with_include_docs() {
        let db = new_db().await;

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

        let mut opts = AllDocsOptions::new();
        opts.include_docs = true;
        let result = db.all_docs(opts).await.unwrap();
        assert!(result.rows[0].doc.is_some());
        let doc = result.rows[0].doc.as_ref().unwrap();
        assert_eq!(doc["name"], "Alice");
        assert_eq!(doc["_id"], "doc1");
    }

    #[tokio::test]
    async fn changes_feed() {
        let db = new_db().await;

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
        assert_eq!(changes.last_seq, Seq::Num(3));

        // Changes since seq 2
        let changes = db
            .changes(ChangesOptions {
                since: Seq::Num(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(changes.results.len(), 1);
        assert_eq!(changes.results[0].id, "doc2");
    }

    #[tokio::test]
    async fn revs_diff() {
        let db = new_db().await;

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        let existing_rev = results[0].rev.clone().unwrap();

        let mut revs = HashMap::new();
        revs.insert(
            "doc1".into(),
            vec![existing_rev.clone(), "2-doesnotexist".into()],
        );
        revs.insert("doc2".into(), vec!["1-abc".into()]);

        let diff = db.revs_diff(revs).await.unwrap();

        // doc1: existing_rev should not be missing, 2-doesnotexist should be
        let doc1_diff = diff.results.get("doc1").unwrap();
        assert!(!doc1_diff.missing.contains(&existing_rev));
        assert!(doc1_diff.missing.contains(&"2-doesnotexist".to_string()));

        // doc2: completely missing
        let doc2_diff = diff.results.get("doc2").unwrap();
        assert!(doc2_diff.missing.contains(&"1-abc".to_string()));
    }

    #[tokio::test]
    async fn local_docs() {
        let db = new_db().await;

        let doc = serde_json::json!({"checkpoint": 42});
        db.put_local("repl-123", doc.clone()).await.unwrap();

        let fetched = db.get_local("repl-123").await.unwrap();
        assert_eq!(fetched["checkpoint"], 42);

        db.remove_local("repl-123").await.unwrap();
        assert!(db.get_local("repl-123").await.is_err());
    }

    #[tokio::test]
    async fn replication_mode_bulk_docs() {
        let db = new_db().await;

        // Insert with explicit revision (replication mode)
        let doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "abc123".into())),
            deleted: false,
            data: serde_json::json!({"name": "replicated"}),
            attachments: HashMap::new(),
        };

        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results[0].ok);

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["name"], "replicated");
        assert_eq!(fetched.rev.unwrap().to_string(), "1-abc123");
    }

    #[tokio::test]
    async fn auto_generate_id() {
        let db = new_db().await;

        let doc = Document {
            id: String::new(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "no-id"}),
            attachments: HashMap::new(),
        };

        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);
        assert!(!results[0].id.is_empty());
    }

    #[tokio::test]
    async fn destroy_clears_everything() {
        let db = new_db().await;

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
        db.put_local("x", serde_json::json!({})).await.unwrap();

        db.destroy().await.unwrap();

        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 0);
        assert_eq!(info.update_seq, Seq::Num(0));
    }

    #[tokio::test]
    async fn bulk_get_documents() {
        let db = new_db().await;

        let doc = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "test"}),
            attachments: HashMap::new(),
        };
        db.bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();

        let result = db
            .bulk_get(vec![
                BulkGetItem {
                    id: "doc1".into(),
                    rev: None,
                },
                BulkGetItem {
                    id: "missing".into(),
                    rev: None,
                },
            ])
            .await
            .unwrap();

        assert_eq!(result.results.len(), 2);
        assert!(result.results[0].docs[0].ok.is_some());
        assert!(result.results[1].docs[0].error.is_some());
    }
}
