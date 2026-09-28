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
    /// Maximum number of batches to buffer (PouchDB option). Currently has
    /// no effect: batches are fetched and written one at a time.
    pub batches_limit: u64,
    /// Optional filter for selective replication.
    pub filter: Option<ReplicationFilter>,
    /// Enable continuous/live replication. Only honored by
    /// [`replicate_live`]; [`replicate`] always runs a single pass.
    pub live: bool,
    /// Automatically retry on transient errors (live replication only).
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

/// Build a stable fingerprint of the active filter for the replication ID,
/// so filtered and unfiltered replications use distinct checkpoints.
fn filter_fingerprint(filter: &Option<ReplicationFilter>) -> String {
    match filter {
        None => "nofilter".to_string(),
        Some(ReplicationFilter::DocIds(ids)) => {
            let mut sorted = ids.clone();
            sorted.sort();
            format!("docids:{}", sorted.join("\u{0}"))
        }
        Some(ReplicationFilter::Selector(sel)) => format!("selector:{}", sel),
        // A custom closure cannot be fingerprinted deterministically; distinct
        // custom filters between the same pair therefore share a checkpoint.
        Some(ReplicationFilter::Custom(_)) => "custom".to_string(),
    }
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
///
/// This is a single pass: `live`, `retry`, `back_off_function` and
/// `poll_interval` only apply to [`replicate_live`].
pub async fn replicate(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: ReplicationOptions,
) -> Result<ReplicationResult> {
    let checkpointer = new_checkpointer(source, target, &opts.filter).await?;
    let since = opts.since.clone();
    let outcome = run_replication(source, target, &opts, &checkpointer, since, None).await?;
    Ok(outcome.result)
}

/// Run a one-shot replication with event streaming.
///
/// Same as `replicate()` but emits `ReplicationEvent` through the provided
/// channel as replication progresses. The replication waits for room in the
/// channel, so the receiver must be drained concurrently (or the channel must
/// be large enough to hold every event).
pub async fn replicate_with_events(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: ReplicationOptions,
    events_tx: mpsc::Sender<ReplicationEvent>,
) -> Result<ReplicationResult> {
    let checkpointer = new_checkpointer(source, target, &opts.filter).await?;
    let since = opts.since.clone();
    let outcome = run_replication(
        source,
        target,
        &opts,
        &checkpointer,
        since,
        Some(&events_tx),
    )
    .await?;
    let _ = events_tx
        .send(ReplicationEvent::Complete(outcome.result.clone()))
        .await;
    Ok(outcome.result)
}

/// Result of one pass of the replication loop.
struct RunOutcome {
    result: ReplicationResult,
}

/// Send an event if there is a listener.
async fn emit(events: Option<&mpsc::Sender<ReplicationEvent>>, event: ReplicationEvent) {
    if let Some(tx) = events {
        let _ = tx.send(event).await;
    }
}

/// Build the checkpointer (and so the replication id) for a source/target
/// pair and filter.
async fn new_checkpointer(
    source: &dyn Adapter,
    target: &dyn Adapter,
    filter: &Option<ReplicationFilter>,
) -> Result<Checkpointer> {
    let source_info = source.info().await?;
    let target_info = target.info().await?;
    Ok(Checkpointer::new(
        &source_info.db_name,
        &target_info.db_name,
        &filter_fingerprint(filter),
    ))
}

/// One replication pass shared by the one-shot and live paths. `since`
/// overrides the checkpoint as the starting sequence. Does not emit
/// `Complete`; callers decide when the replication as a whole is done.
async fn run_replication(
    source: &dyn Adapter,
    target: &dyn Adapter,
    opts: &ReplicationOptions,
    checkpointer: &Checkpointer,
    since: Option<Seq>,
    events: Option<&mpsc::Sender<ReplicationEvent>>,
) -> Result<RunOutcome> {
    let since = if let Some(override_since) = since {
        override_since
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

    emit(events, ReplicationEvent::Active).await;

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
        let mut batch_failed = false;
        for result in &bulk_get_response.results {
            for doc in &result.docs {
                if let Some(ref json) = doc.ok {
                    match Document::from_json(json.clone()) {
                        Ok(document) => docs_to_write.push(document),
                        Err(e) => {
                            errors.push(format!("parse error for {}: {}", result.id, e));
                            batch_failed = true;
                        }
                    }
                }
            }
        }

        // Step 4.5: Apply Selector filter to fetched documents
        if let Some(ReplicationFilter::Selector(ref selector)) = opts.filter {
            docs_to_write.retain(|doc| rouchdb_query::matches_selector(&doc.data, selector));
        }

        if !docs_to_write.is_empty() {
            let attempted = docs_to_write.len() as u64;
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
                    batch_failed = true;
                }
            }

            // With new_edits=false CouchDB replies only with the docs that
            // failed (an empty array means every doc was stored), so count
            // what was sent minus the reported failures.
            let failed = write_results.iter().filter(|wr| !wr.ok).count() as u64;
            total_docs_written += attempted - failed;
        }

        emit(
            events,
            ReplicationEvent::Change {
                docs_read: total_docs_read,
            },
        )
        .await;

        // Do not advance the checkpoint past a batch that had any parse or
        // write failure; stop so the next run retries from the un-advanced
        // sequence rather than silently losing those docs.
        if batch_failed {
            break;
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

    Ok(RunOutcome {
        result: ReplicationResult {
            ok: errors.is_empty(),
            docs_read: total_docs_read,
            docs_written: total_docs_written,
            errors,
            last_seq: current_seq,
        },
    })
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
    let mut opts = opts;
    let poll_interval = opts.poll_interval;
    let retry = opts.retry;
    let back_off = opts.back_off_function.take();

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        let mut attempt: u32 = 0;
        // Track the last successful result so a single terminal Complete can
        // be emitted when the live loop finally exits.
        let mut last_result: Option<ReplicationResult> = None;

        'live: loop {
            let result = async {
                let checkpointer =
                    new_checkpointer(source.as_ref(), target.as_ref(), &opts.filter).await?;
                run_replication(
                    source.as_ref(),
                    target.as_ref(),
                    &opts,
                    &checkpointer,
                    None,
                    Some(&tx),
                )
                .await
            }
            .await
            .map(|outcome| outcome.result);

            match result {
                Ok(r) => {
                    attempt = 0; // Reset retry counter on success
                    if r.docs_read == 0 {
                        // No changes — emit Paused and wait
                        let _ = tx.send(ReplicationEvent::Paused).await;
                    }
                    last_result = Some(r);
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
                            _ = tokio::time::sleep(delay) => continue 'live,
                            _ = cancel_clone.cancelled() => break 'live,
                        }
                    } else {
                        break 'live;
                    }
                }
            }

            // Wait for poll_interval or cancellation
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {},
                _ = cancel_clone.cancelled() => break 'live,
            }
        }

        // Emit a single terminal Complete on exit.
        if let Some(result) = last_result {
            let _ = tx.send(ReplicationEvent::Complete(result)).await;
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
    async fn replicate_carries_attachments() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Create a doc on the source and attach some bytes.
        let r = source
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
        let rev1 = r[0].rev.clone().unwrap();
        source
            .put_attachment("doc1", "hi.txt", &rev1, b"hi!".to_vec(), "text/plain")
            .await
            .unwrap();

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(result.ok);

        // The attachment bytes must be retrievable on the target.
        let bytes = target
            .get_attachment("doc1", "hi.txt", GetAttachmentOptions::default())
            .await
            .unwrap();
        assert_eq!(bytes, b"hi!");
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

    /// Target that answers `new_edits=false` writes the way CouchDB does:
    /// only failed docs are reported, so a fully successful batch is `[]`.
    struct CouchLikeTarget(MemoryAdapter);

    #[async_trait::async_trait]
    impl Adapter for CouchLikeTarget {
        async fn info(&self) -> Result<DbInfo> {
            self.0.info().await
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.0.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            let new_edits = opts.new_edits;
            let results = self.0.bulk_docs(docs, opts).await?;
            if new_edits {
                return Ok(results);
            }
            Ok(results.into_iter().filter(|r| !r.ok).collect())
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            self.0.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            self.0.changes(opts).await
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.0.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            self.0.bulk_get(docs).await
        }
        async fn put_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
            data: Vec<u8>,
            content_type: &str,
        ) -> Result<DocResult> {
            self.0
                .put_attachment(doc_id, att_id, rev, data, content_type)
                .await
        }
        async fn get_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            opts: GetAttachmentOptions,
        ) -> Result<Vec<u8>> {
            self.0.get_attachment(doc_id, att_id, opts).await
        }
        async fn remove_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
        ) -> Result<DocResult> {
            self.0.remove_attachment(doc_id, att_id, rev).await
        }
        async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
            self.0.get_local(id).await
        }
        async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
            self.0.put_local(id, doc).await
        }
        async fn remove_local(&self, id: &str) -> Result<()> {
            self.0.remove_local(id).await
        }
        async fn compact(&self) -> Result<()> {
            self.0.compact().await
        }
        async fn destroy(&self) -> Result<()> {
            self.0.destroy().await
        }
    }

    #[tokio::test]
    async fn docs_written_counts_couchdb_style_empty_replies() {
        let source = MemoryAdapter::new("source");
        let target = CouchLikeTarget(MemoryAdapter::new("target"));

        put_doc(&source, "doc1", serde_json::json!({"v": 1})).await;
        put_doc(&source, "doc2", serde_json::json!({"v": 2})).await;
        put_doc(&source, "doc3", serde_json::json!({"v": 3})).await;

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.docs_written, 3);
        assert_eq!(target.info().await.unwrap().doc_count, 3);
    }
}
