use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::*;
use rouchdb_core::error::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::checkpoint::Checkpointer;

/// Filter for selective replication.
pub enum ReplicationFilter {
    /// Replicate only these document IDs.
    DocIds(Vec<String>),

    /// Replicate documents matching a Mango selector.
    Selector(serde_json::Value),

    /// Replicate documents passing a custom predicate.
    /// Receives the ChangeEvent (id, deleted, seq).
    Custom(Arc<dyn Fn(&ChangeEvent) -> bool + Send + Sync>),
}

impl Clone for ReplicationFilter {
    fn clone(&self) -> Self {
        match self {
            Self::DocIds(ids) => Self::DocIds(ids.clone()),
            Self::Selector(sel) => Self::Selector(sel.clone()),
            Self::Custom(f) => Self::Custom(Arc::clone(f)),
        }
    }
}

/// Replication configuration.
pub struct ReplicationOptions {
    /// Number of documents to process per batch.
    pub batch_size: u64,
    /// Maximum number of batches to buffer.
    pub batches_limit: u64,
    /// Optional filter for selective replication.
    pub filter: Option<ReplicationFilter>,
    /// Enable continuous/live replication.
    pub live: bool,
    /// Automatically retry on transient errors.
    pub retry: bool,
    /// Polling interval for live replication (default: 500ms).
    pub poll_interval: Duration,
    /// Backoff function for retry: takes attempt number, returns delay.
    pub back_off_function: Option<Box<dyn Fn(u32) -> Duration + Send + Sync>>,
    /// Override the starting sequence (skip checkpoint lookup).
    pub since: Option<Seq>,
    /// Whether to save/read checkpoints (default: true).
    /// Set to false to always replicate from scratch.
    pub checkpoint: bool,
}

impl Default for ReplicationOptions {
    fn default() -> Self {
        Self {
            batch_size: 100,
            batches_limit: 10,
            filter: None,
            live: false,
            retry: false,
            poll_interval: Duration::from_millis(500),
            back_off_function: None,
            since: None,
            checkpoint: true,
        }
    }
}

/// Result of a completed replication.
#[derive(Debug, Clone)]
pub struct ReplicationResult {
    pub ok: bool,
    pub docs_read: u64,
    pub docs_written: u64,
    pub errors: Vec<String>,
    pub last_seq: Seq,
}

/// Events emitted during replication for progress tracking.
#[derive(Debug, Clone)]
pub enum ReplicationEvent {
    Change { docs_read: u64 },
    Paused,
    Active,
    Complete(ReplicationResult),
    Error(String),
}

/// Run a one-shot replication from source to target.
///
/// Implements the CouchDB replication protocol:
/// 1. Read checkpoint
/// 2. Fetch changes from source
/// 3. Compute revs_diff against target
/// 4. Fetch missing docs from source
/// 5. Write to target
/// 6. Save checkpoint
pub async fn replicate(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: ReplicationOptions,
) -> Result<ReplicationResult> {
    let source_info = source.info().await?;
    let target_info = target.info().await?;

    let checkpointer = Checkpointer::new(&source_info.db_name, &target_info.db_name);

    // Step 1: Read checkpoint (or use override)
    let since = if let Some(ref override_since) = opts.since {
        override_since.clone()
    } else if opts.checkpoint {
        checkpointer.read_checkpoint(source, target).await?
    } else {
        Seq::default()
    };

    // Extract doc_ids from filter (for ChangesOptions)
    let filter_doc_ids = match &opts.filter {
        Some(ReplicationFilter::DocIds(ids)) => Some(ids.clone()),
        _ => None,
    };

    let mut total_docs_read = 0u64;
    let mut total_docs_written = 0u64;
    let mut errors = Vec::new();
    let mut current_seq = since;

    loop {
        // Step 2: Fetch changes from source
        let changes = source
            .changes(ChangesOptions {
                since: current_seq.clone(),
                limit: Some(opts.batch_size),
                include_docs: false,
                doc_ids: filter_doc_ids.clone(),
                ..Default::default()
            })
            .await?;

        if changes.results.is_empty() {
            break; // No more changes
        }

        let batch_last_seq = changes.last_seq;

        // Step 2.5: Apply Custom filter to changes
        let filtered_changes: Vec<&ChangeEvent> = match &opts.filter {
            Some(ReplicationFilter::Custom(predicate)) => {
                changes.results.iter().filter(|c| predicate(c)).collect()
            }
            _ => changes.results.iter().collect(),
        };

        total_docs_read += filtered_changes.len() as u64;

        if filtered_changes.is_empty() {
            current_seq = batch_last_seq;
            if (changes.results.len() as u64) < opts.batch_size {
                break;
            }
            continue;
        }

        // Step 3: Compute revision diff
        let mut rev_map: HashMap<String, Vec<String>> = HashMap::new();
        for change in &filtered_changes {
            let revs: Vec<String> = change.changes.iter().map(|c| c.rev.clone()).collect();
            rev_map.insert(change.id.clone(), revs);
        }

        let diff = target.revs_diff(rev_map).await?;

        if diff.results.is_empty() {
            // Target already has everything in this batch
            current_seq = batch_last_seq;
            if (changes.results.len() as u64) < opts.batch_size {
                break;
            }
            continue;
        }

        // Step 4: Fetch missing documents from source
        let mut bulk_get_items: Vec<BulkGetItem> = Vec::new();
        for (doc_id, diff_result) in &diff.results {
            for missing_rev in &diff_result.missing {
                bulk_get_items.push(BulkGetItem {
                    id: doc_id.clone(),
                    rev: Some(missing_rev.clone()),
                });
            }
        }

        let bulk_get_response = source.bulk_get(bulk_get_items).await?;

        // Step 5: Write to target with new_edits=false
        let mut docs_to_write: Vec<Document> = Vec::new();
        for result in &bulk_get_response.results {
            for doc in &result.docs {
                if let Some(ref json) = doc.ok {
                    match Document::from_json(json.clone()) {
                        Ok(document) => docs_to_write.push(document),
                        Err(e) => errors.push(format!("parse error for {}: {}", result.id, e)),
                    }
                }
            }
        }

        // Step 4.5: Apply Selector filter to fetched documents
        if let Some(ReplicationFilter::Selector(ref selector)) = opts.filter {
            docs_to_write.retain(|doc| rouchdb_query::matches_selector(&doc.data, selector));
        }

        if !docs_to_write.is_empty() {
            let write_count = docs_to_write.len() as u64;
            let write_results = target
                .bulk_docs(docs_to_write, BulkDocsOptions::replication())
                .await?;

            for wr in &write_results {
                if !wr.ok {
                    errors.push(format!(
                        "write error for {}: {}",
                        wr.id,
                        wr.reason.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            total_docs_written += write_count;
        }

        // Step 6: Save checkpoint (if enabled)
        current_seq = batch_last_seq;
        if opts.checkpoint {
            let _ = checkpointer
                .write_checkpoint(source, target, current_seq.clone())
                .await;
        }

        // Check if we got fewer results than batch_size (last batch)
        if (changes.results.len() as u64) < opts.batch_size {
            break;
        }
    }

    Ok(ReplicationResult {
        ok: errors.is_empty(),
        docs_read: total_docs_read,
        docs_written: total_docs_written,
        errors,
        last_seq: current_seq,
    })
}

/// Run a one-shot replication with event streaming.
///
/// Same as `replicate()` but emits `ReplicationEvent` through the provided
/// channel as replication progresses.
pub async fn replicate_with_events(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: ReplicationOptions,
    events_tx: mpsc::Sender<ReplicationEvent>,
) -> Result<ReplicationResult> {
    let source_info = source.info().await?;
    let target_info = target.info().await?;

    let checkpointer = Checkpointer::new(&source_info.db_name, &target_info.db_name);

    let since = if let Some(ref override_since) = opts.since {
        override_since.clone()
    } else if opts.checkpoint {
        checkpointer.read_checkpoint(source, target).await?
    } else {
        Seq::default()
    };

    let filter_doc_ids = match &opts.filter {
        Some(ReplicationFilter::DocIds(ids)) => Some(ids.clone()),
        _ => None,
    };

    let mut total_docs_read = 0u64;
    let mut total_docs_written = 0u64;
    let mut errors = Vec::new();
    let mut current_seq = since;

    let _ = events_tx.send(ReplicationEvent::Active).await;

    loop {
        let changes = source
            .changes(ChangesOptions {
                since: current_seq.clone(),
                limit: Some(opts.batch_size),
                include_docs: false,
                doc_ids: filter_doc_ids.clone(),
                ..Default::default()
            })
            .await?;

        if changes.results.is_empty() {
            break;
        }

        let batch_last_seq = changes.last_seq;

        let filtered_changes: Vec<&ChangeEvent> = match &opts.filter {
            Some(ReplicationFilter::Custom(predicate)) => {
                changes.results.iter().filter(|c| predicate(c)).collect()
            }
            _ => changes.results.iter().collect(),
        };

        total_docs_read += filtered_changes.len() as u64;

        if filtered_changes.is_empty() {
            current_seq = batch_last_seq;
            if (changes.results.len() as u64) < opts.batch_size {
                break;
            }
            continue;
        }

        let mut rev_map: HashMap<String, Vec<String>> = HashMap::new();
        for change in &filtered_changes {
            let revs: Vec<String> = change.changes.iter().map(|c| c.rev.clone()).collect();
            rev_map.insert(change.id.clone(), revs);
        }

        let diff = target.revs_diff(rev_map).await?;

        if diff.results.is_empty() {
            current_seq = batch_last_seq;
            if (changes.results.len() as u64) < opts.batch_size {
                break;
            }
            continue;
        }

        let mut bulk_get_items: Vec<BulkGetItem> = Vec::new();
        for (doc_id, diff_result) in &diff.results {
            for missing_rev in &diff_result.missing {
                bulk_get_items.push(BulkGetItem {
                    id: doc_id.clone(),
                    rev: Some(missing_rev.clone()),
                });
            }
        }

        let bulk_get_response = source.bulk_get(bulk_get_items).await?;

        let mut docs_to_write: Vec<Document> = Vec::new();
        for result in &bulk_get_response.results {
            for doc in &result.docs {
                if let Some(ref json) = doc.ok {
                    match Document::from_json(json.clone()) {
                        Ok(document) => docs_to_write.push(document),
                        Err(e) => errors.push(format!("parse error for {}: {}", result.id, e)),
                    }
                }
            }
        }

        if let Some(ReplicationFilter::Selector(ref selector)) = opts.filter {
            docs_to_write.retain(|doc| rouchdb_query::matches_selector(&doc.data, selector));
        }

        if !docs_to_write.is_empty() {
            let write_count = docs_to_write.len() as u64;
            let write_results = target
                .bulk_docs(docs_to_write, BulkDocsOptions::replication())
                .await?;

            for wr in &write_results {
                if !wr.ok {
                    errors.push(format!(
                        "write error for {}: {}",
                        wr.id,
                        wr.reason.as_deref().unwrap_or("unknown")
                    ));
                }
            }

            total_docs_written += write_count;
        }

        // Emit change event
        let _ = events_tx
            .send(ReplicationEvent::Change {
                docs_read: total_docs_read,
            })
            .await;

        current_seq = batch_last_seq;
        if opts.checkpoint {
            let _ = checkpointer
                .write_checkpoint(source, target, current_seq.clone())
                .await;
        }

        if (changes.results.len() as u64) < opts.batch_size {
            break;
        }
    }

    let result = ReplicationResult {
        ok: errors.is_empty(),
        docs_read: total_docs_read,
        docs_written: total_docs_written,
        errors,
        last_seq: current_seq,
    };

    let _ = events_tx
        .send(ReplicationEvent::Complete(result.clone()))
        .await;

    Ok(result)
}

/// Run continuous (live) replication from source to target.
///
/// Performs an initial one-shot replication, then polls for new changes
/// at the configured `poll_interval`. Runs until the returned
/// `ReplicationHandle` is cancelled/dropped.
///
/// Events are emitted through the returned channel receiver.
pub fn replicate_live(
    source: Arc<dyn Adapter>,
    target: Arc<dyn Adapter>,
    opts: ReplicationOptions,
) -> (mpsc::Receiver<ReplicationEvent>, ReplicationHandle) {
    let (tx, rx) = mpsc::channel(64);
    let poll_interval = opts.poll_interval;
    let retry = opts.retry;
    let back_off = opts.back_off_function;

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        let mut attempt: u32 = 0;

        loop {
            // Clone the filter for each iteration so Selector and Custom
            // filters remain active across the entire live replication.
            let one_shot_opts = ReplicationOptions {
                batch_size: opts.batch_size,
                batches_limit: opts.batches_limit,
                filter: opts.filter.clone(),
                live: false,
                retry: false,
                poll_interval,
                back_off_function: None,
                since: None,
                checkpoint: opts.checkpoint,
            };

            let result =
                replicate_with_events(source.as_ref(), target.as_ref(), one_shot_opts, tx.clone())
                    .await;

            match result {
                Ok(r) => {
                    attempt = 0; // Reset retry counter on success
                    if r.docs_read == 0 {
                        // No changes — emit Paused and wait
                        let _ = tx.send(ReplicationEvent::Paused).await;
                    }
                }
                Err(e) => {
                    let _ = tx.send(ReplicationEvent::Error(e.to_string())).await;
                    if retry {
                        attempt += 1;
                        let delay = if let Some(ref f) = back_off {
                            f(attempt)
                        } else {
                            // Default exponential backoff: min(1s * 2^attempt, 60s)
                            let secs = (1u64 << attempt.min(6)).min(60);
                            Duration::from_secs(secs)
                        };
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => continue,
                            _ = cancel_clone.cancelled() => break,
                        }
                    } else {
                        break;
                    }
                }
            }

            // Wait for poll_interval or cancellation
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {},
                _ = cancel_clone.cancelled() => break,
            }
        }
    });

    (rx, ReplicationHandle { cancel })
}

/// Handle for a live replication task. Dropping this cancels the replication.
pub struct ReplicationHandle {
    cancel: CancellationToken,
}

impl ReplicationHandle {
    /// Cancel the live replication.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for ReplicationHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_adapter_memory::MemoryAdapter;

    async fn put_doc(adapter: &dyn Adapter, id: &str, data: serde_json::Value) {
        let doc = Document {
            id: id.into(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        adapter
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn replicate_empty_databases() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_read, 0);
        assert_eq!(result.docs_written, 0);
    }

    #[tokio::test]
    async fn replicate_source_to_target() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"name": "Alice"})).await;
        put_doc(&source, "doc2", serde_json::json!({"name": "Bob"})).await;
        put_doc(&source, "doc3", serde_json::json!({"name": "Charlie"})).await;

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_read, 3);
        assert_eq!(result.docs_written, 3);

        // Verify target has the documents
        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 3);

        let doc = target.get("doc1", GetOptions::default()).await.unwrap();
        assert_eq!(doc.data["name"], "Alice");
    }

    #[tokio::test]
    async fn replicate_incremental() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // First replication
        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(r1.docs_written, 1);

        // Add more docs
        put_doc(&source, "doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "doc3", serde_json::json!({"v": 3})).await;

        // Second replication should only sync new docs
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(r2.docs_read, 2);
        assert_eq!(r2.docs_written, 2);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 3);
    }

    #[tokio::test]
    async fn replicate_already_synced() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;

        // First replication
        replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        // Second replication with no new changes
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 0);
    }

    #[tokio::test]
    async fn replicate_batched() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Create more docs than batch size
        for i in 0..15 {
            put_doc(
                &source,
                &format!("doc{:03}", i),
                serde_json::json!({"i": i}),
            )
            .await;
        }

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                batch_size: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 15);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 15);
    }

    #[tokio::test]
    async fn replicate_with_deletes() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Create and sync
        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        // Delete on source
        let doc = source.get("doc1", GetOptions::default()).await.unwrap();
        let del = Document {
            id: "doc1".into(),
            rev: doc.rev,
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        source
            .bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();

        // Replicate delete
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(result.ok);

        // Target should see deletion
        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 0);
    }

    // -----------------------------------------------------------------------
    // Filtered replication tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn replicate_filtered_by_doc_ids() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        put_doc(&source, "doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "doc3", serde_json::json!({"v": 3})).await;
        put_doc(&source, "doc4", serde_json::json!({"v": 4})).await;
        put_doc(&source, "doc5", serde_json::json!({"v": 5})).await;

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec![
                    "doc2".into(),
                    "doc4".into(),
                ])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 2);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 2);

        target.get("doc2", GetOptions::default()).await.unwrap();
        target.get("doc4", GetOptions::default()).await.unwrap();

        // doc1, doc3, doc5 should not exist
        assert!(target.get("doc1", GetOptions::default()).await.is_err());
        assert!(target.get("doc3", GetOptions::default()).await.is_err());
        assert!(target.get("doc5", GetOptions::default()).await.is_err());
    }

    #[tokio::test]
    async fn replicate_filtered_by_selector() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(
            &source,
            "inv1",
            serde_json::json!({"type": "invoice", "amount": 100}),
        )
        .await;
        put_doc(
            &source,
            "inv2",
            serde_json::json!({"type": "invoice", "amount": 200}),
        )
        .await;
        put_doc(
            &source,
            "user1",
            serde_json::json!({"type": "user", "name": "Alice"}),
        )
        .await;
        put_doc(
            &source,
            "user2",
            serde_json::json!({"type": "user", "name": "Bob"}),
        )
        .await;

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::Selector(
                    serde_json::json!({"type": "invoice"}),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 2);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 2);

        let doc = target.get("inv1", GetOptions::default()).await.unwrap();
        assert_eq!(doc.data["amount"], 100);

        assert!(target.get("user1", GetOptions::default()).await.is_err());
    }

    #[tokio::test]
    async fn replicate_filtered_by_custom_closure() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "public:doc1", serde_json::json!({"v": 1})).await;
        put_doc(&source, "public:doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "private:doc3", serde_json::json!({"v": 3})).await;
        put_doc(&source, "private:doc4", serde_json::json!({"v": 4})).await;

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::Custom(Arc::new(|change| {
                    change.id.starts_with("public:")
                }))),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 2);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 2);

        target
            .get("public:doc1", GetOptions::default())
            .await
            .unwrap();
        assert!(
            target
                .get("private:doc3", GetOptions::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replicate_filtered_incremental() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // First batch
        put_doc(&source, "doc1", serde_json::json!({"type": "a"})).await;
        put_doc(&source, "doc2", serde_json::json!({"type": "b"})).await;

        let r1 = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec!["doc1".into()])),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(r1.docs_written, 1);

        // Add more docs
        put_doc(&source, "doc3", serde_json::json!({"type": "a"})).await;
        put_doc(&source, "doc4", serde_json::json!({"type": "b"})).await;

        // Second replication — checkpoint should have advanced past doc1/doc2
        let r2 = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec![
                    "doc1".into(),
                    "doc3".into(),
                ])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Only doc3 is new (doc1 was already replicated)
        assert_eq!(r2.docs_written, 1);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 2); // doc1 + doc3
    }

    #[tokio::test]
    async fn replicate_filtered_with_deletes() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"type": "keep"})).await;
        put_doc(&source, "doc2", serde_json::json!({"type": "skip"})).await;

        // Replicate only doc1
        replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec!["doc1".into()])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Delete doc1 on source
        let doc = source.get("doc1", GetOptions::default()).await.unwrap();
        let del = Document {
            id: "doc1".into(),
            rev: doc.rev,
            deleted: true,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        };
        source
            .bulk_docs(vec![del], BulkDocsOptions::new())
            .await
            .unwrap();

        // Replicate again with same filter — deletion should propagate
        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec!["doc1".into()])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 0);
    }

    #[tokio::test]
    async fn replicate_no_filter_unchanged() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        put_doc(&source, "doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "doc3", serde_json::json!({"v": 3})).await;

        // No filter — should replicate everything (same as before)
        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_read, 3);
        assert_eq!(result.docs_written, 3);

        let target_info = target.info().await.unwrap();
        assert_eq!(target_info.doc_count, 3);
    }
}
