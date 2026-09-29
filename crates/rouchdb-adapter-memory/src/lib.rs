use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use uuid::Uuid;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::{Result, RouchError};
use rouchdb_core::merge::{
    collect_conflicts, is_deleted, latest_leaf, remove_leaves, revs_diff_one, winning_rev,
};
use rouchdb_core::rev_tree::{
    RevStatus, RevTree, collect_leaves, find_rev_ancestry, revs_info, traverse_rev_tree,
};
use rouchdb_core::write::{
    LocalWrite, PlannedWrite, ReplicatedWrite, edit_parent, error_result, local_doc_id,
    local_document, ok_result, plan_local_write, plan_new_edit, plan_replicated_edit,
};

/// Revisions kept per branch unless configured otherwise (CouchDB's
/// default `_revs_limit`).
pub const DEFAULT_REV_LIMIT: u64 = 1000;

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
    /// Map from "pos-hash" to that revision's attachments (att_id -> meta).
    rev_attachments: HashMap<String, HashMap<String, AttachmentMeta>>,
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
    /// The security document (kept apart from local documents, so that
    /// `_local/_security` is an ordinary local document as in CouchDB).
    security: SecurityDocument,
    /// Attachment data keyed by digest.
    attachments: HashMap<String, Vec<u8>>,
    /// Number of purge requests applied.
    purge_seq: u64,
}

/// In-memory adapter for RouchDB. All data is held in RAM.
#[derive(Debug, Clone)]
pub struct MemoryAdapter {
    inner: Arc<RwLock<Inner>>,
    rev_limit: u64,
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
                security: SecurityDocument::default(),
                attachments: HashMap::new(),
                purge_seq: 0,
            })),
            rev_limit: DEFAULT_REV_LIMIT,
        }
    }

    /// Keep at most `limit` revisions per branch of a document's history
    /// (PouchDB's `revs_limit`, CouchDB's `_revs_limit`; 0 means no limit).
    /// Older revisions are stemmed away and are no longer readable.
    pub fn with_rev_limit(mut self, limit: u64) -> Self {
        self.rev_limit = limit;
        self
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Parse (and normalize) a revision string.
fn parse_rev(rev_str: &str) -> Result<(u64, String)> {
    let rev: Revision = rev_str.parse()?;
    Ok((rev.pos, rev.hash))
}

/// The canonical form of a revision string (`InvalidRev` if malformed).
fn canonical_rev(rev_str: &str) -> Result<String> {
    Ok(rev_str.parse::<Revision>()?.to_string())
}

/// The answer to a `changes` request with `limit: 0`: no change, and the
/// position the feed stands at (like CouchDB: `since`, or the current
/// sequence when descending).
fn empty_changes(opts: &ChangesOptions, update_seq: u64) -> ChangesResponse {
    ChangesResponse {
        results: Vec::new(),
        last_seq: if opts.descending {
            Seq::Num(update_seq)
        } else {
            opts.since.clone()
        },
    }
}

/// Map a failed attachment edit to the error the attachment APIs return.
fn attachment_edit_error(result: DocResult) -> RouchError {
    match result.error.as_deref() {
        Some("conflict") => RouchError::Conflict,
        Some("not_found") => RouchError::NotFound(result.reason.unwrap_or_default()),
        _ => RouchError::BadRequest(result.reason.unwrap_or_default()),
    }
}

// ---------------------------------------------------------------------------
// Adapter implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Adapter for MemoryAdapter {
    async fn info(&self) -> Result<DbInfo> {
        let inner = self.inner.read().await;
        let mut doc_count = 0u64;
        let mut doc_del_count = 0u64;
        for d in inner.docs.values() {
            if is_deleted(&d.rev_tree) {
                doc_del_count += 1;
            } else {
                doc_count += 1;
            }
        }

        Ok(DbInfo {
            db_name: inner.name.clone(),
            doc_count,
            doc_del_count,
            update_seq: Seq::Num(inner.update_seq),
        })
    }

    async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
        if opts.open_revs.is_some() {
            return Err(RouchError::BadRequest(
                "open_revs is not supported by get(); use bulk_get".into(),
            ));
        }

        let requested = opts.rev.as_deref().map(canonical_rev).transpose()?;
        let inner = self.inner.read().await;
        if let Some(local) = local_doc_id(id) {
            return match inner.local_docs.get(local) {
                Some(stored) => Ok(local_document(local, stored.clone())),
                None => Err(RouchError::NotFound("missing".into())),
            };
        }
        let stored = inner
            .docs
            .get(id)
            .ok_or_else(|| RouchError::NotFound(id.to_string()))?;

        // Determine which revision to return
        let mut target_rev = if let Some(rev_str) = requested {
            rev_str
        } else {
            // Use the winning revision
            let winner = winning_rev(&stored.rev_tree)
                .ok_or_else(|| RouchError::NotFound(id.to_string()))?;
            winner.to_string()
        };

        // latest: walk the requested rev's own branch down to its winning leaf
        // (returns the rev unchanged when it is already a leaf).
        if opts.latest
            && opts.rev.is_some()
            && let Ok((pos, hash)) = parse_rev(&target_rev)
            && let Some(rev) = latest_leaf(&stored.rev_tree, pos, &hash)
        {
            target_rev = rev.to_string();
        }

        // An unknown, stemmed, compacted or otherwise body-less revision is
        // missing, never an empty document.
        let data = stored
            .rev_data
            .get(&target_rev)
            .cloned()
            .ok_or_else(|| RouchError::NotFound("missing".into()))?;

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

        let mut doc = Document {
            id: id.to_string(),
            rev: Some(Revision::new(pos, hash.clone())),
            deleted,
            data,
            attachments: HashMap::new(),
        };

        // Populate attachment metadata for this revision (inlining bytes only
        // when explicitly requested via opts.attachments).
        if let Some(atts) = stored.rev_attachments.get(&target_rev) {
            for (name, meta) in atts {
                let mut meta = meta.clone();
                if opts.attachments {
                    meta.data = inner.attachments.get(&meta.digest).cloned();
                    meta.stub = meta.data.is_none();
                } else {
                    meta.data = None;
                    meta.stub = true;
                }
                doc.attachments.insert(name.clone(), meta);
            }
        }

        if let serde_json::Value::Object(ref mut map) = doc.data {
            // Add conflicts if requested
            if opts.conflicts {
                let conflicts = collect_conflicts(&stored.rev_tree);
                if !conflicts.is_empty() {
                    let conflict_list: Vec<serde_json::Value> = conflicts
                        .iter()
                        .map(|c| serde_json::Value::String(c.to_string()))
                        .collect();
                    map.insert(
                        "_conflicts".to_string(),
                        serde_json::Value::Array(conflict_list),
                    );
                }
            }

            // The revision's ancestry, newest first.
            if opts.revs
                && let Some(ids) = find_rev_ancestry(&stored.rev_tree, pos, &hash)
            {
                map.insert(
                    "_revisions".to_string(),
                    serde_json::json!({"start": pos, "ids": ids}),
                );
            }

            // Add revs_info if requested: this revision's branch only.
            if opts.revs_info
                && let Some(info) = revs_info(&stored.rev_tree, pos, &hash)
            {
                map.insert("_revs_info".to_string(), serde_json::to_value(&info)?);
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
                process_doc_new_edits(&mut inner, doc, true, self.rev_limit)
            } else {
                process_doc_replication(&mut inner, doc, self.rev_limit)
            };
            results.push(result);
        }

        Ok(results)
    }

    async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
        let inner = self.inner.read().await;

        // If specific keys are requested, use those (in request order,
        // reversed for descending, like CouchDB); otherwise every id, sorted.
        let target_keys: Vec<String> = if let Some(ref keys) = opts.keys {
            let mut keys = keys.clone();
            if opts.descending {
                keys.reverse();
            }
            keys
        } else if let Some(ref key) = opts.key {
            vec![key.clone()]
        } else {
            let mut doc_ids: Vec<String> = inner.docs.keys().cloned().collect();
            doc_ids.sort();
            if opts.descending {
                doc_ids.reverse();
            }
            doc_ids
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

            let found = inner
                .docs
                .get(key.as_str())
                .and_then(|stored| winning_rev(&stored.rev_tree).map(|w| (stored, w)));
            if let Some((stored, winner)) = found {
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
                    doc: doc_json,
                    ..AllDocsRow::document(
                        key.clone(),
                        AllDocsRowValue {
                            rev: winner.to_string(),
                            deleted: deleted.then_some(true),
                        },
                    )
                });
            } else if opts.keys.is_some() {
                // Every requested key gets a row (CouchDB).
                rows.push(AllDocsRow::not_found(key.clone()));
            }
        }

        // total_rows is the total number of non-deleted documents in the
        // database, independent of key range / key / keys / skip / limit
        // (CouchDB semantics).
        let total_rows = inner
            .docs
            .values()
            .filter(|stored| {
                winning_rev(&stored.rev_tree).is_some() && !is_deleted(&stored.rev_tree)
            })
            .count() as u64;

        // Apply skip and limit
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
        if opts.limit == Some(0) {
            return Ok(empty_changes(&opts, inner.update_seq));
        }

        let mut results = Vec::new();
        // Highest sequence actually inspected (even if filtered out), so the
        // feed's last_seq advances past a fully-filtered range instead of
        // sticking at `since` and forcing endless re-scans.
        let mut max_scanned: Option<u64> = None;

        // Iterate changes after `since` (saturating so a huge `since` such as
        // an unresolved "now" can never overflow).
        let range = (opts.since.as_num().saturating_add(1))..;
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
            max_scanned = Some(max_scanned.map_or(*seq, |m| m.max(*seq)));

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
                    // All leaf revisions, including deleted ones, so a fully
                    // deleted document still reports a non-empty `changes`
                    // array (deletion is signaled by the `deleted` field).
                    collect_leaves(&s.rev_tree)
                        .iter()
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
            .or_else(|| max_scanned.map(Seq::Num))
            .unwrap_or(opts.since.clone());

        Ok(ChangesResponse { results, last_seq })
    }

    async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
        let inner = self.inner.read().await;
        let mut results = HashMap::new();

        for (doc_id, rev_list) in revs {
            let tree = inner.docs.get(&doc_id).map(|s| &s.rev_tree);
            if let Some(diff) = revs_diff_one(tree, &rev_list)? {
                results.insert(doc_id, diff);
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
                        canonical_rev(rev).unwrap_or_else(|_| rev.clone())
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

                        // Include inline attachments so replication carries
                        // their bytes end-to-end.
                        if let Some(atts) = stored.rev_attachments.get(&rev_str)
                            && !atts.is_empty()
                        {
                            use base64::Engine;
                            let mut att_map = serde_json::Map::new();
                            for (name, meta) in atts {
                                let mut m = serde_json::Map::new();
                                m.insert(
                                    "content_type".into(),
                                    serde_json::Value::String(meta.content_type.clone()),
                                );
                                m.insert(
                                    "digest".into(),
                                    serde_json::Value::String(meta.digest.clone()),
                                );
                                m.insert("length".into(), serde_json::json!(meta.length));
                                if let Some(bytes) = inner.attachments.get(&meta.digest) {
                                    m.insert(
                                        "data".into(),
                                        serde_json::Value::String(
                                            base64::engine::general_purpose::STANDARD.encode(bytes),
                                        ),
                                    );
                                    m.insert("stub".into(), serde_json::Value::Bool(false));
                                } else {
                                    m.insert("stub".into(), serde_json::Value::Bool(true));
                                }
                                att_map.insert(name.clone(), serde_json::Value::Object(m));
                            }
                            obj.insert("_attachments".into(), serde_json::Value::Object(att_map));
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
        let mut inner = self.inner.write().await;

        let stored = inner
            .docs
            .get(doc_id)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        let parent: Revision = rev.parse()?;
        let rev = parent.to_string();

        // The new revision builds on `rev` (any leaf, not only the winner):
        // its body plus the parent's attachments with this one added.
        let doc_data = stored
            .rev_data
            .get(&rev)
            .cloned()
            .ok_or(RouchError::Conflict)?;
        let parent_atts = stored
            .rev_attachments
            .get(&rev)
            .cloned()
            .unwrap_or_default();
        let mut attachments = parent_atts.clone();
        attachments.insert(
            att_id.to_string(),
            AttachmentMeta {
                content_type: content_type.to_string(),
                digest: String::new(),
                length: data.len() as u64,
                stub: false,
                data: Some(data),
            },
        );

        let doc = Document {
            id: doc_id.to_string(),
            rev: Some(parent),
            deleted: false,
            data: doc_data,
            attachments,
        };
        let tree = stored.rev_tree.clone();
        let plan = plan_new_edit(Some(&tree), doc, Some(&parent_atts), false, self.rev_limit)
            .map_err(attachment_edit_error)?;
        Ok(apply_write(&mut inner, plan))
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
            canonical_rev(rev)?
        } else {
            winning_rev(&stored.rev_tree)
                .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?
                .to_string()
        };

        // Resolve the attachment metadata stored for this revision, then
        // return the raw bytes held by digest.
        let meta = stored
            .rev_attachments
            .get(&rev_str)
            .and_then(|m| m.get(att_id))
            .ok_or_else(|| RouchError::NotFound(format!("attachment {}/{}", doc_id, att_id)))?;

        inner
            .attachments
            .get(&meta.digest)
            .cloned()
            .ok_or_else(|| RouchError::NotFound(format!("attachment {}/{}", doc_id, att_id)))
    }

    async fn remove_attachment(&self, doc_id: &str, att_id: &str, rev: &str) -> Result<DocResult> {
        let mut inner = self.inner.write().await;

        let stored = inner
            .docs
            .get(doc_id)
            .ok_or_else(|| RouchError::NotFound(doc_id.to_string()))?;
        let parent: Revision = rev.parse()?;
        let rev = parent.to_string();

        let doc_data = stored
            .rev_data
            .get(&rev)
            .cloned()
            .ok_or(RouchError::Conflict)?;
        let parent_atts = stored
            .rev_attachments
            .get(&rev)
            .cloned()
            .unwrap_or_default();
        if !parent_atts.contains_key(att_id) {
            return Err(RouchError::NotFound(format!(
                "attachment {}/{}",
                doc_id, att_id
            )));
        }
        let mut attachments = parent_atts.clone();
        attachments.remove(att_id);

        // Create a new revision (attachment removal is a document update)
        // whose attachment set is exactly the remaining ones.
        let doc = Document {
            id: doc_id.to_string(),
            rev: Some(parent),
            deleted: false,
            data: doc_data,
            attachments,
        };
        let tree = stored.rev_tree.clone();
        let plan = plan_new_edit(Some(&tree), doc, Some(&parent_atts), false, self.rev_limit)
            .map_err(attachment_edit_error)?;
        Ok(apply_write(&mut inner, plan))
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
        rouchdb_core::json::check_document_depth(&doc)?;
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
            stored.rev_attachments.retain(|k, _| leaf_revs.contains(k));

            // ...and record in the tree that their bodies are gone.
            mark_non_leaves_missing(&mut stored.rev_tree);
        }

        // Drop attachment bytes no remaining revision references.
        collect_unreferenced_attachments(&mut inner);

        Ok(())
    }

    async fn destroy(&self) -> Result<()> {
        let mut inner = self.inner.write().await;
        inner.docs.clear();
        inner.changes.clear();
        inner.local_docs.clear();
        inner.security = SecurityDocument::default();
        inner.attachments.clear();
        inner.update_seq = 0;
        inner.purge_seq = 0;
        Ok(())
    }

    async fn purge(&self, req: HashMap<String, Vec<String>>) -> Result<PurgeResponse> {
        let mut inner = self.inner.write().await;
        let mut purged = HashMap::new();
        let mut bumped = false;

        for (doc_id, revs) in req {
            let revs = revs
                .iter()
                .map(|r| canonical_rev(r))
                .collect::<Result<Vec<_>>>()?;
            let Some(stored) = inner.docs.get(&doc_id) else {
                continue;
            };
            // Only leaves can be purged; their ancestors go too unless another
            // leaf still needs them. Nothing older is ever resurrected.
            let (new_tree, removed) = remove_leaves(&stored.rev_tree, &revs);
            if removed.is_empty() {
                purged.insert(doc_id, removed);
                continue;
            }
            let old_seq = stored.seq;
            inner.changes.remove(&old_seq);

            if new_tree.is_empty() {
                inner.docs.remove(&doc_id);
            } else {
                // The winner may have changed: record the document again.
                inner.update_seq += 1;
                bumped = true;
                let seq = inner.update_seq;
                let deleted = is_deleted(&new_tree);
                let stored = inner.docs.get_mut(&doc_id).expect("doc exists");
                let mut kept = std::collections::HashSet::new();
                traverse_rev_tree(&new_tree, |pos, node, _| {
                    kept.insert(format!("{}-{}", pos, node.hash));
                });
                stored.rev_data.retain(|k, _| kept.contains(k));
                stored.rev_deleted.retain(|k, _| kept.contains(k));
                stored.rev_attachments.retain(|k, _| kept.contains(k));
                stored.rev_tree = new_tree;
                stored.seq = seq;
                inner.changes.insert(seq, (doc_id.clone(), deleted));
            }
            purged.insert(doc_id, removed);
        }

        // A purge is a database update even when no document keeps a change
        // entry (CouchDB bumps update_seq per purge request).
        if !bumped {
            inner.update_seq += 1;
        }
        inner.purge_seq += 1;
        collect_unreferenced_attachments(&mut inner);

        Ok(PurgeResponse {
            purge_seq: Some(inner.purge_seq),
            purged,
        })
    }

    async fn get_security(&self) -> Result<SecurityDocument> {
        Ok(self.inner.read().await.security.clone())
    }

    async fn put_security(&self, doc: SecurityDocument) -> Result<()> {
        self.inner.write().await.security = doc;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Document processing (new_edits = true)
// ---------------------------------------------------------------------------

/// Apply one `new_edits=true` write. The edit rules (conflicts, attachment
/// inheritance, revision hashing) live in `rouchdb_core::write` so every
/// adapter behaves the same; this only loads the inputs and stores the plan.
fn process_doc_new_edits(
    inner: &mut Inner,
    mut doc: Document,
    inherit: bool,
    rev_limit: u64,
) -> DocResult {
    if let Err(e) = doc.prepare_for_write() {
        return error_result(&doc.id, "bad_request", &e.to_string());
    }
    if doc.id.is_empty() {
        doc.id = Uuid::new_v4().to_string();
    }
    if local_doc_id(&doc.id).is_some() {
        return match plan_local_write(doc) {
            Ok(LocalWrite::Put { id, body, result }) => {
                inner.local_docs.insert(id, body);
                result
            }
            Ok(LocalWrite::Delete { id, result }) => {
                inner.local_docs.remove(&id);
                result
            }
            Err(result) => result,
        };
    }

    let existing = inner.docs.get(&doc.id);
    let tree = existing.map(|s| &s.rev_tree);
    let parent_atts = edit_parent(tree, &doc)
        .and_then(|p| existing.and_then(|s| s.rev_attachments.get(&p.to_string()).cloned()));

    match plan_new_edit(tree, doc, parent_atts.as_ref(), inherit, rev_limit) {
        Ok(plan) => apply_write(inner, plan),
        Err(result) => result,
    }
}

// ---------------------------------------------------------------------------
// Document processing (new_edits = false, replication mode)
// ---------------------------------------------------------------------------

fn process_doc_replication(inner: &mut Inner, doc: Document, rev_limit: u64) -> DocResult {
    // CouchDB ignores `_local/` documents in replicated writes (they are
    // never replicated): nothing is stored.
    if local_doc_id(&doc.id).is_some() {
        return DocResult {
            ok: true,
            id: doc.id,
            rev: doc.rev.map(|r| r.to_string()),
            error: None,
            reason: None,
        };
    }
    let existing = inner.docs.get(&doc.id);
    let has_body = match (existing, &doc.rev) {
        (Some(s), Some(r)) => s.rev_data.contains_key(&r.clone().normalized().to_string()),
        _ => false,
    };

    let plan = match plan_replicated_edit(existing.map(|s| &s.rev_tree), doc, has_body, rev_limit) {
        Ok(ReplicatedWrite::Write(plan)) => *plan,
        Ok(ReplicatedWrite::AlreadyStored(result)) => return result,
        Err(result) => return result,
    };

    // Stubs must refer to bytes we already hold.
    if let Some(digest) = plan
        .required_blobs
        .iter()
        .find(|d| !inner.attachments.contains_key(*d))
    {
        return error_result(
            &plan.id,
            "missing_stub",
            &format!("Invalid attachment stub in {} for {}", plan.id, digest),
        );
    }

    apply_write(inner, plan)
}

/// Persist a planned write: attachment bytes, revision tree, the revision's
/// body/attachments, and a new sequence entry.
fn apply_write(inner: &mut Inner, plan: PlannedWrite) -> DocResult {
    for (digest, bytes) in plan.new_blobs {
        inner.attachments.insert(digest, bytes);
    }

    // Update sequence
    inner.update_seq += 1;
    let seq = inner.update_seq;

    // Remove old change entry for this doc (each doc has only one entry in changes)
    if let Some(existing) = inner.docs.get(&plan.id) {
        inner.changes.remove(&existing.seq);
    }

    let rev_str = plan.rev.to_string();
    let stored = inner
        .docs
        .entry(plan.id.clone())
        .or_insert_with(|| StoredDoc {
            rev_tree: Vec::new(),
            rev_data: HashMap::new(),
            rev_deleted: HashMap::new(),
            rev_attachments: HashMap::new(),
            seq: 0,
        });

    // Stemmed revisions no longer exist: drop their bodies.
    for rev in &plan.stemmed {
        let rev = rev.to_string();
        stored.rev_data.remove(&rev);
        stored.rev_deleted.remove(&rev);
        stored.rev_attachments.remove(&rev);
    }
    stored.rev_tree = plan.tree;
    stored.rev_data.insert(rev_str.clone(), plan.data);
    stored.rev_deleted.insert(rev_str.clone(), plan.deleted);
    stored.rev_attachments.insert(rev_str, plan.attachments);
    stored.seq = seq;

    // The feed reports whether the document (its winner) is deleted, not
    // whether this particular edit was a deletion.
    inner
        .changes
        .insert(seq, (plan.id.clone(), plan.doc_deleted));

    ok_result(&plan.id, &plan.rev)
}

// ---------------------------------------------------------------------------
// Compaction helpers
// ---------------------------------------------------------------------------

/// Mark every non-leaf node as `Missing` (its body has been discarded).
fn mark_non_leaves_missing(tree: &mut RevTree) {
    fn walk(node: &mut rouchdb_core::rev_tree::RevNode) {
        if !node.children.is_empty() {
            node.status = RevStatus::Missing;
            for child in node.children.iter_mut() {
                walk(child);
            }
        }
    }
    for path in tree.iter_mut() {
        walk(&mut path.tree);
    }
}

/// Drop attachment bytes that no stored revision references any more.
fn collect_unreferenced_attachments(inner: &mut Inner) {
    let referenced: std::collections::HashSet<String> = inner
        .docs
        .values()
        .flat_map(|d| d.rev_attachments.values())
        .flat_map(|atts| atts.values().map(|m| m.digest.clone()))
        .collect();
    inner
        .attachments
        .retain(|digest, _| referenced.contains(digest));
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
    async fn recreate_deleted_doc_with_same_content() {
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

        // Re-create with the identical content and no rev. The deterministic
        // rev hash would reproduce rev 1 exactly, so the new edit must extend
        // the tombstone instead of starting a fresh pos-1 branch.
        let recreated = Document {
            id: "doc1".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"name": "Alice"}),
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![recreated], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(results[0].ok);
        let rev3 = results[0].rev.clone().unwrap();
        assert!(rev3.starts_with("3-"), "expected pos 3, got {rev3}");

        let fetched = db.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(fetched.data["name"], "Alice");

        let info = db.info().await.unwrap();
        assert_eq!(info.doc_count, 1);
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
        let rev = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();

        let mut opts = AllDocsOptions::new();
        opts.include_docs = true;
        let result = db.all_docs(opts).await.unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].rev().unwrap(), rev);
        assert_eq!(
            result.rows[0].doc,
            Some(serde_json::json!({"_id": "doc1", "_rev": rev, "name": "Alice"}))
        );
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
        assert_eq!(diff.results.len(), 2);

        // doc1: only the unknown rev is missing; the stored leaf (a lower
        // generation) may be its ancestor.
        let doc1_diff = diff.results.get("doc1").unwrap();
        assert_eq!(doc1_diff.missing, ["2-doesnotexist"]);
        assert_eq!(doc1_diff.possible_ancestors, [existing_rev]);

        // doc2: completely missing, nothing to descend from.
        let doc2_diff = diff.results.get("doc2").unwrap();
        assert_eq!(doc2_diff.missing, ["1-abc"]);
        assert!(doc2_diff.possible_ancestors.is_empty());
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
        assert_eq!(changes.last_seq, Seq::Num(0));
        assert!(db.get_security().await.unwrap().admins.names.is_empty());
        let inner = db.inner.read().await;
        assert!(inner.attachments.is_empty());
        assert_eq!(inner.purge_seq, 0);
    }

    #[tokio::test]
    async fn attachment_roundtrip() {
        let db = new_db().await;

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
        let rev1 = r[0].rev.clone().unwrap();

        // Store an attachment; the bytes must be retrievable afterwards.
        let r2 = db
            .put_attachment("doc1", "hi.txt", &rev1, b"hi!".to_vec(), "text/plain")
            .await
            .unwrap();
        let rev2 = r2.rev.clone().unwrap();
        assert!(rev2.starts_with("2-"), "{}", rev2);

        let bytes = db
            .get_attachment("doc1", "hi.txt", GetAttachmentOptions::default())
            .await
            .unwrap();
        assert_eq!(bytes, b"hi!");

        // get with attachments=true exposes the metadata + inline data.
        let fetched = db
            .get(
                "doc1",
                GetOptions {
                    attachments: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.attachments["hi.txt"].content_type, "text/plain");
        assert_eq!(
            fetched.attachments["hi.txt"].data.as_deref(),
            Some(&b"hi!"[..])
        );

        // Removing the attachment makes it unretrievable on the new rev.
        let r3 = db.remove_attachment("doc1", "hi.txt", &rev2).await.unwrap();
        assert!(r3.rev.as_deref().unwrap().starts_with("3-"), "{:?}", r3);
        let err = db
            .get_attachment("doc1", "hi.txt", GetAttachmentOptions::default())
            .await;
        assert!(matches!(err, Err(RouchError::NotFound(_))), "{:?}", err);
        // The previous revision still serves its bytes.
        let old = db
            .get_attachment(
                "doc1",
                "hi.txt",
                GetAttachmentOptions {
                    rev: Some(rev2.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(old, b"hi!");
    }

    #[tokio::test]
    async fn all_docs_total_rows_is_full_count() {
        let db = new_db().await;
        for name in ["a", "b", "c", "d"] {
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

        // Narrow the range to a single row; total_rows must still be 4.
        let opts = AllDocsOptions {
            start_key: Some("b".into()),
            end_key: Some("b".into()),
            ..AllDocsOptions::new()
        };
        let result = db.all_docs(opts).await.unwrap();
        let ids: Vec<&str> = result.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(ids, ["b"]);
        assert_eq!(result.total_rows, 4);
    }

    #[tokio::test]
    async fn changes_last_seq_advances_when_all_filtered() {
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

        // doc_ids filter matches nothing, but last_seq must still reach the
        // database's update_seq so polling doesn't re-scan forever.
        let changes = db
            .changes(ChangesOptions {
                doc_ids: Some(vec!["nonexistent".into()]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(changes.results.is_empty());
        assert_eq!(changes.last_seq, Seq::Num(3));
    }

    #[tokio::test]
    async fn changes_all_docs_style_nonempty_for_deleted() {
        let db = new_db().await;
        let r = db
            .bulk_docs(
                vec![Document {
                    id: "doc1".into(),
                    rev: None,
                    deleted: false,
                    data: serde_json::json!({"v": 1}),
                    attachments: HashMap::new(),
                }],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        let rev1: Revision = r[0].rev.clone().unwrap().parse().unwrap();
        let tomb = db
            .bulk_docs(
                vec![Document {
                    id: "doc1".into(),
                    rev: Some(rev1),
                    deleted: true,
                    data: serde_json::json!({}),
                    attachments: HashMap::new(),
                }],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();

        let changes = db
            .changes(ChangesOptions {
                style: ChangesStyle::AllDocs,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(changes.results.len(), 1);
        let ev = &changes.results[0];
        assert_eq!(ev.id, "doc1");
        assert!(ev.deleted);
        // A deleted doc still lists its (tombstone) leaf rev.
        let revs: Vec<&str> = ev.changes.iter().map(|c| c.rev.as_str()).collect();
        assert_eq!(revs, [tomb.as_str()]);
    }

    #[tokio::test]
    async fn get_latest_stays_on_requested_branch() {
        let db = new_db().await;
        // Two branches sharing ancestor 1-aaa, built via replication so the
        // intermediate nodes are missing:
        //   losing branch:  1-aaa -> 2-bbb -> 3-ccc  (leaf)
        //   winning branch: 1-aaa -> 2-zzz -> 3-ddd -> 4-eee (leaf, the winner)
        let branches = [
            (3u64, "ccc", vec!["ccc", "bbb", "aaa"]),
            (4u64, "eee", vec!["eee", "ddd", "zzz", "aaa"]),
        ];
        for (pos, hash, ids) in branches {
            let mut data = serde_json::json!({ "v": hash });
            data.as_object_mut().unwrap().insert(
                "_revisions".into(),
                serde_json::json!({"start": pos, "ids": ids}),
            );
            let d = Document {
                id: "doc1".into(),
                rev: Some(Revision::new(pos, hash.into())),
                deleted: false,
                data,
                attachments: HashMap::new(),
            };
            db.bulk_docs(vec![d], BulkDocsOptions::replication())
                .await
                .unwrap();
        }

        // Requesting an internal node on the losing branch with latest=true
        // must follow THAT branch to 3-ccc, not jump to the global winner
        // 4-eee.
        let doc = db
            .get(
                "doc1",
                GetOptions {
                    rev: Some("2-bbb".into()),
                    latest: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), "3-ccc");
        assert_eq!(doc.data["v"], "ccc");
    }

    #[tokio::test]
    async fn attachment_store_is_garbage_collected() {
        let db = new_db().await;
        let doc = Document {
            id: "d".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        let r1 = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap()[0]
            .rev
            .clone()
            .unwrap();
        let r2 = db
            .put_attachment("d", "a", &r1, vec![1; 64], "application/octet-stream")
            .await
            .unwrap()
            .rev
            .unwrap();
        // A rejected write must not leave its bytes behind.
        assert!(matches!(
            db.put_attachment("d", "a", &r1, vec![9; 64], "application/octet-stream")
                .await,
            Err(RouchError::Conflict)
        ));
        assert_eq!(db.inner.read().await.attachments.len(), 1);
        // Replacing the attachment and compacting frees the old bytes.
        db.put_attachment("d", "a", &r2, vec![2; 64], "application/octet-stream")
            .await
            .unwrap();
        db.compact().await.unwrap();
        let inner = db.inner.read().await;
        assert_eq!(inner.attachments.len(), 1);
        assert!(inner.attachments.values().all(|b| b == &vec![2; 64]));
    }

    async fn edit(db: &MemoryAdapter, rev: Option<&str>, v: u64) -> String {
        let doc = Document {
            id: "d".into(),
            rev: rev.map(|r| r.parse().unwrap()),
            deleted: false,
            data: serde_json::json!({ "v": v }),
            attachments: HashMap::new(),
        };
        let res = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        assert!(res[0].ok, "{:?}", res[0]);
        res[0].rev.clone().unwrap()
    }

    /// Q-CORE-1: revisions stemmed by the revision limit lose their bodies.
    #[tokio::test]
    async fn stemming_drops_stored_bodies() {
        let db = MemoryAdapter::new("s").with_rev_limit(3);
        let mut rev = edit(&db, None, 0).await;
        for v in 1..6 {
            rev = edit(&db, Some(&rev), v).await;
        }
        let inner = db.inner.read().await;
        let stored = &inner.docs["d"];
        assert_eq!(stored.rev_data.len(), 3);
        assert_eq!(stored.rev_deleted.len(), 3);
        assert_eq!(stored.rev_attachments.len(), 3);
    }

    /// CouchDB's default `_revs_limit` (1000) applies without configuration.
    #[tokio::test]
    async fn default_rev_limit_is_1000() {
        let db = new_db().await;
        let first = edit(&db, None, 0).await;
        let mut rev = first.clone();
        for v in 1..=1000 {
            rev = edit(&db, Some(&rev), v).await;
        }
        assert!(rev.starts_with("1001-"));
        let old = GetOptions {
            rev: Some(first),
            ..Default::default()
        };
        assert!(matches!(
            db.get("d", old).await,
            Err(RouchError::NotFound(_))
        ));
        let got = db
            .get(
                "d",
                GetOptions {
                    revs: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            got.data["_revisions"]["ids"].as_array().unwrap().len(),
            1000
        );
        assert_eq!(db.inner.read().await.docs["d"].rev_data.len(), 1000);
        // 0 means no limit.
        let unlimited = MemoryAdapter::new("u").with_rev_limit(0);
        let mut rev = edit(&unlimited, None, 0).await;
        for v in 1..=1000 {
            rev = edit(&unlimited, Some(&rev), v).await;
        }
        assert_eq!(unlimited.inner.read().await.docs["d"].rev_data.len(), 1001);
    }
}
