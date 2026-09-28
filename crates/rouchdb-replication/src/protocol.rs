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

/// The JSON a replication selector is evaluated against: the body plus
/// `_id`, `_rev` and `_deleted` (attachments are left out).
fn selector_view(doc: &Document) -> serde_json::Value {
    let mut obj = match &doc.data {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    obj.insert("_id".into(), serde_json::Value::String(doc.id.clone()));
    if let Some(rev) = &doc.rev {
        obj.insert("_rev".into(), serde_json::Value::String(rev.to_string()));
    }
    if doc.deleted {
        obj.insert("_deleted".into(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(obj)
}

/// Whether `rev` is still a leaf of document `id` on `adapter`.
async fn is_leaf(adapter: &dyn Adapter, id: &str, rev: &str) -> Result<bool> {
    let changes = adapter
        .changes(ChangesOptions {
            doc_ids: Some(vec![id.to_string()]),
            style: ChangesStyle::AllDocs,
            ..Default::default()
        })
        .await?;
    Ok(changes
        .results
        .iter()
        .filter(|c| c.id == id)
        .any(|c| c.changes.iter().any(|r| r.rev == rev)))
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
    /// Set when the pass stopped at a batch it could not fully replicate
    /// (the checkpoint was not advanced past it), so it must be retried.
    failure: Option<String>,
}

/// Per-doc write errors that will never succeed on retry.
fn is_denied(error: &str) -> bool {
    error.eq_ignore_ascii_case("forbidden") || error.eq_ignore_ascii_case("unauthorized")
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
    let mut failed = false;

    emit(events, ReplicationEvent::Active).await;

    loop {
        // Step 2: Fetch changes from source. `all_docs` style lists every
        // leaf, so conflicting branches are replicated, not just the winner.
        let changes = source
            .changes(ChangesOptions {
                since: current_seq.clone(),
                limit: Some(opts.batch_size),
                include_docs: false,
                doc_ids: filter_doc_ids.clone(),
                style: ChangesStyle::AllDocs,
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
                } else if let Some(ref err) = doc.error {
                    // A rev that vanished after the feed listed it (edited
                    // then compacted, or purged) is not needed: its successor
                    // has a later seq. Any other failure must be retried.
                    if err.error == "not_found" && !is_leaf(source, &result.id, &err.rev).await? {
                        continue;
                    }
                    errors.push(format!(
                        "fetch error for {} {}: {}: {}",
                        result.id, err.rev, err.error, err.reason
                    ));
                    batch_failed = true;
                }
            }
        }

        // Step 4.5: Apply Selector filter to fetched documents, including
        // the reserved fields a selector may test (`_id`, `_rev`, `_deleted`).
        if let Some(ReplicationFilter::Selector(ref selector)) = opts.filter {
            docs_to_write
                .retain(|doc| rouchdb_query::matches_selector(&selector_view(doc), selector));
        }

        if !docs_to_write.is_empty() {
            let attempted = docs_to_write.len() as u64;
            let write_results = target
                .bulk_docs(docs_to_write, BulkDocsOptions::replication())
                .await?;

            for wr in write_results.iter().filter(|wr| !wr.ok) {
                let error = wr.error.as_deref().unwrap_or("unknown");
                let message = format!(
                    "write error for {}: {}: {}",
                    wr.id,
                    error,
                    wr.reason.as_deref().unwrap_or("unknown")
                );
                if is_denied(error) {
                    // Rejected for good (validation or permissions). Like
                    // PouchDB's `denied`, report it and move on so a single
                    // doc cannot stall the replication forever.
                    emit(events, ReplicationEvent::Error(message.clone())).await;
                } else {
                    batch_failed = true;
                }
                errors.push(message);
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

        // Do not advance the checkpoint past a batch that had any fetch,
        // parse or (non-denied) write failure; stop so the next run retries
        // from the un-advanced sequence rather than silently losing docs.
        if batch_failed {
            failed = true;
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

    let failure = failed.then(|| errors.join("; "));
    Ok(RunOutcome {
        result: ReplicationResult {
            ok: errors.is_empty(),
            docs_read: total_docs_read,
            docs_written: total_docs_written,
            errors,
            last_seq: current_seq,
        },
        failure,
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
            .await;

            let failure = match result {
                Ok(outcome) => {
                    if outcome.failure.is_none() {
                        attempt = 0; // Reset retry counter on success
                        if outcome.result.docs_read == 0 {
                            // No changes — emit Paused and wait
                            let _ = tx.send(ReplicationEvent::Paused).await;
                        }
                    }
                    last_result = Some(outcome.result);
                    outcome.failure
                }
                Err(e) => Some(e.to_string()),
            };

            if let Some(message) = failure {
                let _ = tx.send(ReplicationEvent::Error(message)).await;
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

    /// Write one revision with its full ancestry, as replication does.
    async fn put_rev(adapter: &dyn Adapter, id: &str, ids: &[&str], data: serde_json::Value) {
        let start = ids.len() as u64;
        let mut json = data;
        json["_id"] = serde_json::json!(id);
        json["_rev"] = serde_json::json!(format!("{}-{}", start, ids[0]));
        json["_revisions"] = serde_json::json!({"start": start, "ids": ids});
        let doc = Document::from_json(json).unwrap();
        let res = adapter
            .bulk_docs(vec![doc], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(res.iter().all(|r| r.ok), "{res:?}");
    }

    async fn conflicts_of(adapter: &dyn Adapter, id: &str) -> Vec<String> {
        let doc = adapter
            .get(
                id,
                GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        doc.data["_conflicts"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn replicate_propagates_conflict_branches() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");

        // Two leaves under 1-aaa: 2-ccc wins, 2-bbb is the conflict.
        put_rev(&source, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
        put_rev(&source, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;
        assert_eq!(conflicts_of(&source, "d").await, vec!["2-bbb"]);

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(result.ok);

        assert_eq!(conflicts_of(&target, "d").await, vec!["2-bbb"]);
    }

    #[tokio::test]
    async fn push_then_pull_converges_on_conflicts() {
        let a = MemoryAdapter::new("a");
        let b = MemoryAdapter::new("b");

        // A edits to 2-bbb (the winner), B to 2-aaa (the loser).
        put_rev(&a, "d", &["bbb", "111"], serde_json::json!({"side": "a"})).await;
        put_rev(&b, "d", &["aaa", "111"], serde_json::json!({"side": "b"})).await;

        replicate(&a, &b, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(conflicts_of(&b, "d").await, vec!["2-aaa"]);

        // Pulling back must bring B's losing branch to A as well.
        replicate(&b, &a, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!(conflicts_of(&a, "d").await, vec!["2-aaa"]);
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
    async fn replicate_selector_sees_reserved_fields() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "user:1", serde_json::json!({"n": 1})).await;
        put_doc(&source, "user:2", serde_json::json!({"n": 2})).await;
        put_doc(&source, "x", serde_json::json!({"n": 3})).await;

        let run = |selector: serde_json::Value| {
            let source = &source;
            async move {
                let target = MemoryAdapter::new("target");
                let result = replicate(
                    source,
                    &target,
                    ReplicationOptions {
                        filter: Some(ReplicationFilter::Selector(selector)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                assert!(result.ok);
                let rows = target.all_docs(AllDocsOptions::new()).await.unwrap().rows;
                rows.into_iter().map(|r| r.id).collect::<Vec<_>>()
            }
        };

        assert_eq!(
            run(serde_json::json!({"_id": {"$regex": "^user:"}})).await,
            vec!["user:1", "user:2"]
        );
        assert_eq!(
            run(serde_json::json!({"_id": {"$ne": "x"}})).await,
            vec!["user:1", "user:2"]
        );
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

    /// Faults a [`Faulty`] adapter injects around a memory adapter.
    #[derive(Default)]
    struct Faults {
        /// `bulk_get` answers this doc id with an error item of this kind.
        bulk_get_error: Option<(String, String)>,
        /// Before answering `bulk_get`, edit this doc and compact, so the
        /// requested rev no longer exists (one-shot).
        supersede_on_bulk_get: Option<String>,
        /// `bulk_docs` rejects this doc id with this error kind.
        write_error: Option<(String, String)>,
    }

    struct Faulty {
        inner: MemoryAdapter,
        faults: std::sync::Mutex<Faults>,
    }

    impl Faulty {
        fn new(inner: MemoryAdapter, faults: Faults) -> Self {
            Self {
                inner,
                faults: std::sync::Mutex::new(faults),
            }
        }

        fn heal(&self) {
            *self.faults.lock().unwrap() = Faults::default();
        }
    }

    #[async_trait::async_trait]
    impl Adapter for Faulty {
        async fn info(&self) -> Result<DbInfo> {
            self.inner.info().await
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.inner.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            let write_error = self.faults.lock().unwrap().write_error.clone();
            let Some((bad_id, error)) = write_error else {
                return self.inner.bulk_docs(docs, opts).await;
            };
            let (rejected, docs): (Vec<_>, Vec<_>) = docs.into_iter().partition(|d| d.id == bad_id);
            let mut results = self.inner.bulk_docs(docs, opts).await?;
            results.extend(rejected.into_iter().map(|d| DocResult {
                ok: false,
                id: d.id,
                rev: None,
                error: Some(error.clone()),
                reason: Some("injected".into()),
            }));
            Ok(results)
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            self.inner.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            self.inner.changes(opts).await
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.inner.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            let supersede = self.faults.lock().unwrap().supersede_on_bulk_get.take();
            if let Some(id) = supersede {
                let current = self.inner.get(&id, GetOptions::default()).await?;
                let mut next = current.clone();
                next.data = serde_json::json!({"v": "next"});
                self.inner
                    .bulk_docs(vec![next], BulkDocsOptions::new())
                    .await?;
                self.inner.compact().await?;
            }
            let mut resp = self.inner.bulk_get(docs).await?;
            if let Some((id, error)) = self.faults.lock().unwrap().bulk_get_error.clone() {
                for result in resp.results.iter_mut().filter(|r| r.id == id) {
                    for doc in &mut result.docs {
                        let rev = doc.ok.as_ref().unwrap()["_rev"].as_str().unwrap().into();
                        doc.ok = None;
                        doc.error = Some(BulkGetError {
                            id: id.clone(),
                            rev,
                            error: error.clone(),
                            reason: "injected".into(),
                        });
                    }
                }
            }
            Ok(resp)
        }
        async fn put_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
            data: Vec<u8>,
            content_type: &str,
        ) -> Result<DocResult> {
            self.inner
                .put_attachment(doc_id, att_id, rev, data, content_type)
                .await
        }
        async fn get_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            opts: GetAttachmentOptions,
        ) -> Result<Vec<u8>> {
            self.inner.get_attachment(doc_id, att_id, opts).await
        }
        async fn remove_attachment(
            &self,
            doc_id: &str,
            att_id: &str,
            rev: &str,
        ) -> Result<DocResult> {
            self.inner.remove_attachment(doc_id, att_id, rev).await
        }
        async fn get_local(&self, id: &str) -> Result<serde_json::Value> {
            self.inner.get_local(id).await
        }
        async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
            self.inner.put_local(id, doc).await
        }
        async fn remove_local(&self, id: &str) -> Result<()> {
            self.inner.remove_local(id).await
        }
        async fn compact(&self) -> Result<()> {
            self.inner.compact().await
        }
        async fn destroy(&self) -> Result<()> {
            self.inner.destroy().await
        }
    }

    #[tokio::test]
    async fn bulk_get_errors_fail_the_batch() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "a", serde_json::json!({"v": 1})).await;
        put_doc(&inner, "b", serde_json::json!({"v": 2})).await;
        let source = Faulty::new(
            inner,
            Faults {
                bulk_get_error: Some(("b".into(), "unknown_error".into())),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(!r1.ok);
        assert!(r1.errors.iter().any(|e| e.contains("b")), "{:?}", r1.errors);

        // The checkpoint did not skip `b`: once the source recovers, the next
        // run delivers it.
        source.heal();
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(
            target.get("b", GetOptions::default()).await.unwrap().data["v"],
            2
        );
    }

    #[tokio::test]
    async fn bulk_get_not_found_for_superseded_rev_is_skipped() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "d", serde_json::json!({"v": 1})).await;
        let source = Faulty::new(
            inner,
            Faults {
                supersede_on_bulk_get: Some("d".into()),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        // The listed rev was edited and compacted away before it could be
        // fetched: nothing is lost, its successor comes later in the feed.
        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r1.ok, "{:?}", r1.errors);

        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(
            target.get("d", GetOptions::default()).await.unwrap().data["v"],
            "next"
        );
    }

    #[tokio::test]
    async fn denied_doc_does_not_stall_replication() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({"v": 1})).await;
        put_doc(&source, "x", serde_json::json!({"v": 2})).await;
        put_doc(&source, "b", serde_json::json!({"v": 3})).await;
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                write_error: Some(("x".into(), "forbidden".into())),
                ..Default::default()
            },
        );
        let opts = || ReplicationOptions {
            batch_size: 1,
            ..Default::default()
        };

        // `x` is rejected for good (e.g. by validate_doc_update): it is
        // reported, and the docs after it still replicate.
        let (tx, mut rx) = mpsc::channel(64);
        let r1 = replicate_with_events(&source, &target, opts(), tx)
            .await
            .unwrap();
        assert!(!r1.ok);
        assert_eq!(r1.docs_written, 2);
        assert!(r1.errors.iter().any(|e| e.contains("x")), "{:?}", r1.errors);
        assert!(target.get("b", GetOptions::default()).await.is_ok());
        let mut denied_reported = false;
        while let Ok(event) = rx.try_recv() {
            denied_reported |= matches!(event, ReplicationEvent::Error(ref m) if m.contains("x"));
        }
        assert!(denied_reported);

        // The checkpoint moved past `x`, so it is not retried forever.
        let r2 = replicate(&source, &target, opts()).await.unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(r2.docs_read, 0);
    }

    #[tokio::test]
    async fn transient_write_error_stops_without_advancing() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({"v": 1})).await;
        put_doc(&source, "x", serde_json::json!({"v": 2})).await;
        put_doc(&source, "b", serde_json::json!({"v": 3})).await;
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                write_error: Some(("x".into(), "unknown_error".into())),
                ..Default::default()
            },
        );
        let opts = || ReplicationOptions {
            batch_size: 1,
            ..Default::default()
        };

        let r1 = replicate(&source, &target, opts()).await.unwrap();
        assert!(!r1.ok);
        assert!(target.get("b", GetOptions::default()).await.is_err());

        target.heal();
        let r2 = replicate(&source, &target, opts()).await.unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert!(target.get("x", GetOptions::default()).await.is_ok());
        assert!(target.get("b", GetOptions::default()).await.is_ok());
    }

    #[tokio::test]
    async fn live_replication_reports_write_failures() {
        let source = Arc::new(MemoryAdapter::new("source"));
        put_doc(source.as_ref(), "x", serde_json::json!({"v": 1})).await;
        let target = Arc::new(Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                write_error: Some(("x".into(), "unknown_error".into())),
                ..Default::default()
            },
        ));

        let (mut rx, _handle) = replicate_live(
            source,
            target,
            ReplicationOptions {
                live: true,
                poll_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );

        // Without retry the failure is reported and ends the replication.
        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("live replication kept running after a write failure");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReplicationEvent::Error(m) if m.contains("x")))
        );
        assert!(matches!(events.last(), Some(ReplicationEvent::Complete(r)) if !r.ok));
    }
}
