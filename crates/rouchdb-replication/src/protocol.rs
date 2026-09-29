use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rouchdb_core::adapter::{Adapter, ChangeNotice};
use rouchdb_core::document::*;
use rouchdb_core::error::Result;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::checkpoint::Checkpointer;

/// Filter for selective replication.
///
/// `#[non_exhaustive]`: new kinds of filters (such as CouchDB design
/// document filters) may be added in minor releases, so a `match` on it
/// needs a catch-all arm.
#[non_exhaustive]
pub enum ReplicationFilter {
    /// Replicate only these document IDs.
    DocIds(Vec<String>),

    /// Replicate documents matching a Mango selector.
    Selector(serde_json::Value),

    /// Replicate documents passing a custom predicate.
    /// Receives the ChangeEvent (id, deleted, seq).
    ///
    /// A closure cannot be fingerprinted, so two different predicates would
    /// share one checkpoint and the second would silently skip everything
    /// the first had already scanned. Checkpoints are therefore neither read
    /// nor written for custom filters: each run scans from `since` (or the
    /// start); documents the target already has are not transferred again.
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
///
/// Set the options you need and fill the rest with `..Default::default()`:
/// fields may be added in minor releases, and a literal that lists every
/// field would then stop compiling.
///
/// ```
/// use rouchdb_replication::ReplicationOptions;
///
/// let opts = ReplicationOptions {
///     batch_size: 50,
///     ..Default::default()
/// };
/// # let _ = opts;
/// ```
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
    /// Polling interval for live replication (default: 500ms), used when
    /// the source cannot announce its changes ([`Adapter::subscribe`]
    /// returns `None`, as for a remote CouchDB).
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
///
/// `#[non_exhaustive]`: fields may be added in minor releases.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReplicationResult {
    pub ok: bool,
    pub docs_read: u64,
    pub docs_written: u64,
    pub errors: Vec<String>,
    pub last_seq: Seq,
}

/// Events emitted during replication for progress tracking.
///
/// `#[non_exhaustive]`: new events may be added in minor releases, so a
/// `match` on it needs a catch-all arm, and `Change` may gain fields (match
/// it as `Change { docs_read, .. }`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ReplicationEvent {
    #[non_exhaustive]
    Change {
        docs_read: u64,
    },
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
        // Never used for a checkpoint: see `ReplicationFilter::Custom`.
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

/// Whether `new` is strictly later than `old`. Opaque CouchDB sequences are
/// only comparable by their numeric prefix.
fn seq_after(new: &Seq, old: &Seq) -> bool {
    match (new, old) {
        (Seq::Num(n), Seq::Num(o)) => n > o,
        _ => new != old && new.as_num() >= old.as_num(),
    }
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
    Ok(Checkpointer::new(
        &source.id().await?,
        &target.id().await?,
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
    let use_checkpoint =
        opts.checkpoint && !matches!(opts.filter, Some(ReplicationFilter::Custom(_)));
    let since = if let Some(override_since) = since {
        override_since
    } else if use_checkpoint {
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
    // Last sequence stored in (or read from) the checkpoint.
    let mut checkpointed_seq = current_seq.clone();
    let mut failed = false;
    let mut checkpoint_failed = false;

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
            // No more changes, though the feed may have moved past changes
            // excluded by a doc_ids filter.
            if seq_after(&changes.last_seq, &current_seq) {
                current_seq = changes.last_seq;
            }
            break;
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
                bulk_get_items.push(BulkGetItem::new(doc_id).with_rev(missing_rev));
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
        if use_checkpoint {
            if let Err(e) = checkpointer
                .write_checkpoint(source, target, current_seq.clone())
                .await
            {
                errors.push(format!("checkpoint write failed: {}", e));
                failed = true;
                checkpoint_failed = true;
                break;
            }
            checkpointed_seq = current_seq.clone();
        }

        // Check if we got fewer results than batch_size (last batch)
        if (changes.results.len() as u64) < opts.batch_size {
            break;
        }
    }

    // Batches that needed no writes (filtered out, or already on the
    // target) are progress too: save it once so the next run does not
    // rescan them. After a failure `current_seq` still stops before the
    // failed batch, so this never skips anything.
    if use_checkpoint
        && !checkpoint_failed
        && current_seq != checkpointed_seq
        && let Err(e) = checkpointer
            .write_checkpoint(source, target, current_seq.clone())
            .await
    {
        errors.push(format!("checkpoint write failed: {}", e));
        failed = true;
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
/// Performs an initial one-shot replication, then replicates again each
/// time the source announces a change ([`Adapter::subscribe`]), or, for a
/// source that cannot, every `poll_interval`. Runs until the returned
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
        // Subscribed before the first pass, so a change made during a pass
        // wakes the next one up.
        let mut notices = source.subscribe();
        let mut attempt: u32 = 0;
        // Track the last successful result so a single terminal Complete can
        // be emitted when the live loop finally exits.
        let mut last_result: Option<ReplicationResult> = None;
        // Starts at the caller's `since` (or the checkpoint when unset), then
        // follows each pass's last_seq so polls never rescan the feed. One
        // checkpointer (one session) spans the live replication until a pass
        // fails with an error.
        let mut since = opts.since.clone();
        let mut checkpointer: Option<Checkpointer> = None;

        'live: loop {
            let result = async {
                if checkpointer.is_none() {
                    checkpointer = Some(
                        new_checkpointer(source.as_ref(), target.as_ref(), &opts.filter).await?,
                    );
                }
                let checkpointer = checkpointer.as_ref().expect("initialized above");
                run_replication(
                    source.as_ref(),
                    target.as_ref(),
                    &opts,
                    checkpointer,
                    since.clone(),
                    Some(&tx),
                )
                .await
            }
            .await;

            // Whether the pass caught up with the source without reporting
            // Paused yet.
            let mut caught_up = false;
            let failure = match result {
                Ok(outcome) => {
                    // Resume the next pass where this one stopped.
                    since = Some(outcome.result.last_seq.clone());
                    if outcome.failure.is_none() {
                        attempt = 0; // Reset retry counter on success
                        if outcome.result.docs_read == 0 {
                            // No changes — emit Paused and wait
                            let _ = tx.send(ReplicationEvent::Paused).await;
                        } else {
                            caught_up = true;
                        }
                    }
                    last_result = Some(outcome.result);
                    outcome.failure
                }
                Err(e) => {
                    // The peer may have been unreachable when the id was
                    // derived (an HTTP adapter then falls back to its URL):
                    // derive it again on the next attempt.
                    checkpointer = None;
                    Some(e.to_string())
                }
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

            // Wait for a change on the source (its notifications, or
            // poll_interval without them) or cancellation.
            match notices.as_mut() {
                Some(rx) => {
                    // Changes made during the pass are replicated right away.
                    if !drain_notices(rx) {
                        if caught_up {
                            let _ = tx.send(ReplicationEvent::Paused).await;
                        }
                        tokio::select! {
                            notice = rx.recv() => {
                                if matches!(notice, Err(broadcast::error::RecvError::Closed)) {
                                    notices = None; // poll from now on
                                } else {
                                    drain_notices(rx);
                                }
                            }
                            _ = cancel_clone.cancelled() => break 'live,
                        }
                    }
                }
                None => {
                    tokio::select! {
                        _ = tokio::time::sleep(poll_interval) => {},
                        _ = cancel_clone.cancelled() => break 'live,
                    }
                }
            }
        }

        // Emit a single terminal Complete on exit.
        if let Some(result) = last_result {
            let _ = tx.send(ReplicationEvent::Complete(result)).await;
        }
    });

    (rx, ReplicationHandle { cancel })
}

/// Consume the change notices already queued; whether there were any (a
/// lagging receiver had some too).
fn drain_notices(rx: &mut broadcast::Receiver<ChangeNotice>) -> bool {
    let mut any = false;
    while let Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) = rx.try_recv() {
        any = true;
    }
    any
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
    use rouchdb_core::error::RouchError;

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

    /// A memory adapter reporting a custom `id()`, like two remote
    /// databases that share a name on different servers.
    struct NamedAt(MemoryAdapter, &'static str);

    #[async_trait::async_trait]
    impl Adapter for NamedAt {
        async fn info(&self) -> Result<DbInfo> {
            self.0.info().await
        }
        async fn id(&self) -> Result<String> {
            Ok(self.1.to_string())
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.0.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            self.0.bulk_docs(docs, opts).await
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
    async fn same_named_peers_get_distinct_replication_ids() {
        let source = MemoryAdapter::new("local");
        let a = NamedAt(MemoryAdapter::new("userdb"), "http://a.example/userdb");
        let b = NamedAt(MemoryAdapter::new("userdb"), "http://b.example/userdb");

        let id_a = new_checkpointer(&source, &a, &None).await.unwrap();
        let id_b = new_checkpointer(&source, &b, &None).await.unwrap();
        assert_ne!(id_a.replication_id(), id_b.replication_id());

        // Local adapters keep deriving it from the name, so their existing
        // checkpoints stay valid.
        let local = new_checkpointer(&source, &MemoryAdapter::new("userdb"), &None)
            .await
            .unwrap();
        let before = Checkpointer::new("local", "userdb", "nofilter");
        assert_eq!(local.replication_id(), before.replication_id());
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
                rows.into_iter().map(|r| r.key).collect::<Vec<_>>()
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
    async fn custom_filters_do_not_share_a_checkpoint() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");
        put_doc(&source, "public:1", serde_json::json!({})).await;
        put_doc(&source, "private:1", serde_json::json!({})).await;
        let prefix_filter = |prefix: &'static str| {
            Some(ReplicationFilter::Custom(Arc::new(
                move |c: &ChangeEvent| c.id.starts_with(prefix),
            )))
        };

        let r1 = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: prefix_filter("public:"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(r1.docs_written, 1);

        // A different closure must not resume from the first one's checkpoint.
        let r2 = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: prefix_filter("private:"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(r2.docs_written, 1);
        assert!(target.get("private:1", GetOptions::default()).await.is_ok());
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

    /// Target that behaves like CouchDB 3.5 where the memory adapter does not:
    /// - `new_edits=false` writes report only the failed docs, so a fully
    ///   successful batch is `[]`;
    /// - `reject` models a `validate_doc_update` that refuses one doc id
    ///   (per-doc `forbidden`, the rest of the batch is stored);
    /// - `_local` docs get revs `0-N`: a `_rev` that is not `0-<n>` is a 400,
    ///   and a write with `_rev: 0-n` stores `0-(n+1)` (CouchDB does not check
    ///   that `n` is the current rev, and a write without `_rev` stores `0-1`).
    struct CouchLikeTarget {
        inner: MemoryAdapter,
        reject: Option<&'static str>,
    }

    impl CouchLikeTarget {
        fn new(inner: MemoryAdapter) -> Self {
            Self {
                inner,
                reject: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl Adapter for CouchLikeTarget {
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
            let new_edits = opts.new_edits;
            let (rejected, docs): (Vec<_>, Vec<_>) = docs
                .into_iter()
                .partition(|d| Some(d.id.as_str()) == self.reject);
            let mut results = self.inner.bulk_docs(docs, opts).await?;
            results.extend(rejected.into_iter().map(|d| {
                let mut result =
                    DocResult::error(d.id, "forbidden", "rejected by validate_doc_update");
                result.rev = d.rev.map(|r| r.to_string());
                result
            }));
            if new_edits {
                return Ok(results);
            }
            Ok(results.into_iter().filter(|r| !r.ok).collect())
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
            self.inner.bulk_get(docs).await
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
        async fn put_local(&self, id: &str, mut doc: serde_json::Value) -> Result<()> {
            let n = match doc.get("_rev") {
                None => 0,
                Some(rev) => rev
                    .as_str()
                    .and_then(|r| r.strip_prefix("0-"))
                    .and_then(|n| n.parse::<u64>().ok())
                    .ok_or_else(|| RouchError::BadRequest("Invalid rev format".into()))?,
            };
            doc["_rev"] = serde_json::json!(format!("0-{}", n + 1));
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
    async fn docs_written_counts_couchdb_style_empty_replies() {
        let source = MemoryAdapter::new("source");
        let target = CouchLikeTarget::new(MemoryAdapter::new("target"));

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

    #[tokio::test]
    async fn docs_written_excludes_docs_couchdb_rejected_in_a_batch() {
        let source = MemoryAdapter::new("source");
        let target = CouchLikeTarget {
            inner: MemoryAdapter::new("target"),
            reject: Some("x"),
        };
        for id in ["a", "x", "b"] {
            put_doc(&source, id, serde_json::json!({})).await;
        }

        // One batch of three; CouchDB answers with the one failure only.
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        assert!(!result.ok);
        assert_eq!((result.docs_read, result.docs_written), (3, 2));
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].starts_with("write error for x: forbidden"),
            "{:?}",
            result.errors
        );
        let ids: Vec<String> = target
            .all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn checkpoint_round_trips_the_couchdb_local_rev() {
        let source = MemoryAdapter::new("source");
        let target = CouchLikeTarget::new(MemoryAdapter::new("target"));
        for i in 0..3 {
            put_doc(&source, &format!("d{i}"), serde_json::json!({})).await;
        }

        // One checkpoint write per batch, each carrying the rev just read.
        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                batch_size: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.ok, "{:?}", result.errors);

        let rep_id = new_checkpointer(&source, &target, &None)
            .await
            .unwrap()
            .replication_id()
            .to_string();
        let cp = target.get_local(&rep_id).await.unwrap();
        assert_eq!(cp["_rev"], "0-3");
        assert_eq!(cp["last_seq"], 3);
    }

    /// Faults a [`Faulty`] adapter injects around a memory adapter.
    #[derive(Default)]
    struct Faults {
        /// `bulk_get` answers this doc id with an error item of this kind.
        bulk_get_error: Option<(String, String)>,
        /// `bulk_get` answers `not_found` for this (doc id, rev) only, though
        /// the rev is still a leaf.
        bulk_get_missing_rev: Option<(String, String)>,
        /// Before answering `bulk_get`, edit this doc and compact, so the
        /// requested rev no longer exists (one-shot).
        supersede_on_bulk_get: Option<String>,
        /// `bulk_docs` rejects this doc id with this error kind.
        write_error: Option<(String, String)>,
        /// The n-th `bulk_docs` call (1-based) fails as a whole, like a
        /// connection reset.
        bulk_docs_fails_on_call: Option<usize>,
        /// `get_local` fails with this error.
        get_local_error: Option<fn() -> RouchError>,
        /// `put_local` fails with this error.
        put_local_error: Option<fn() -> RouchError>,
        /// The next this many `put_local` calls fail with `Conflict`.
        put_local_conflicts: usize,
        /// Unreachable: the calls a replication makes (`info`, `changes`,
        /// `revs_diff`, `bulk_get`, `bulk_docs`, `get_local`, `put_local`)
        /// fail, while `id()` falls back to this value (as an HTTP adapter may
        /// without a server answer).
        offline_id: Option<String>,
    }

    struct Faulty {
        inner: MemoryAdapter,
        faults: std::sync::Mutex<Faults>,
        bulk_docs_calls: std::sync::atomic::AtomicUsize,
        changes_calls: std::sync::atomic::AtomicUsize,
    }

    impl Faulty {
        fn new(inner: MemoryAdapter, faults: Faults) -> Self {
            Self {
                inner,
                faults: std::sync::Mutex::new(faults),
                bulk_docs_calls: std::sync::atomic::AtomicUsize::new(0),
                changes_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn heal(&self) {
            *self.faults.lock().unwrap() = Faults::default();
        }
    }

    impl Faulty {
        fn check_online(&self) -> Result<()> {
            match self.faults.lock().unwrap().offline_id {
                Some(_) => Err(RouchError::DatabaseError("offline".into())),
                None => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Adapter for Faulty {
        async fn info(&self) -> Result<DbInfo> {
            self.check_online()?;
            self.inner.info().await
        }
        async fn id(&self) -> Result<String> {
            if let Some(id) = self.faults.lock().unwrap().offline_id.clone() {
                return Ok(id);
            }
            self.inner.id().await
        }
        async fn get(&self, id: &str, opts: GetOptions) -> Result<Document> {
            self.inner.get(id, opts).await
        }
        async fn bulk_docs(
            &self,
            docs: Vec<Document>,
            opts: BulkDocsOptions,
        ) -> Result<Vec<DocResult>> {
            self.check_online()?;
            let call = 1 + self
                .bulk_docs_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.faults.lock().unwrap().bulk_docs_fails_on_call == Some(call) {
                return Err(RouchError::DatabaseError("connection reset".into()));
            }
            let write_error = self.faults.lock().unwrap().write_error.clone();
            let Some((bad_id, error)) = write_error else {
                return self.inner.bulk_docs(docs, opts).await;
            };
            let (rejected, docs): (Vec<_>, Vec<_>) = docs.into_iter().partition(|d| d.id == bad_id);
            let mut results = self.inner.bulk_docs(docs, opts).await?;
            results.extend(
                rejected
                    .into_iter()
                    .map(|d| DocResult::error(d.id, error.clone(), "injected")),
            );
            Ok(results)
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            self.inner.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            self.check_online()?;
            self.changes_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.changes(opts).await
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.check_online()?;
            self.inner.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            self.check_online()?;
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
            if let Some((id, rev)) = self.faults.lock().unwrap().bulk_get_missing_rev.clone() {
                for doc in resp
                    .results
                    .iter_mut()
                    .filter(|r| r.id == id)
                    .flat_map(|r| r.docs.iter_mut())
                    .filter(|d| d.ok.as_ref().is_some_and(|ok| ok["_rev"] == rev.as_str()))
                {
                    doc.ok = None;
                    doc.error = Some(BulkGetError::new(&id, &rev, "not_found", "missing"));
                }
            }
            if let Some((id, error)) = self.faults.lock().unwrap().bulk_get_error.clone() {
                for result in resp.results.iter_mut().filter(|r| r.id == id) {
                    for doc in &mut result.docs {
                        let rev = doc.ok.as_ref().unwrap()["_rev"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        doc.ok = None;
                        doc.error = Some(BulkGetError::new(&id, rev, &error, "injected"));
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
            self.check_online()?;
            if let Some(error) = self.faults.lock().unwrap().get_local_error {
                return Err(error());
            }
            self.inner.get_local(id).await
        }
        async fn put_local(&self, id: &str, doc: serde_json::Value) -> Result<()> {
            self.check_online()?;
            {
                let mut faults = self.faults.lock().unwrap();
                if let Some(error) = faults.put_local_error {
                    return Err(error());
                }
                if faults.put_local_conflicts > 0 {
                    faults.put_local_conflicts -= 1;
                    return Err(RouchError::Conflict);
                }
            }
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

    /// Wait (bounded) for the first event matching `pred`.
    async fn wait_for(
        rx: &mut mpsc::Receiver<ReplicationEvent>,
        pred: impl Fn(&ReplicationEvent) -> bool,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = rx.recv().await {
                if pred(&event) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    async fn live_replication_honors_since() {
        let source = Arc::new(MemoryAdapter::new("source"));
        let target = Arc::new(MemoryAdapter::new("target"));
        put_doc(source.as_ref(), "old", serde_json::json!({})).await;
        let now = source.info().await.unwrap().update_seq;

        let (mut rx, handle) = replicate_live(
            source.clone(),
            target.clone(),
            ReplicationOptions {
                live: true,
                since: Some(now),
                checkpoint: false,
                poll_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
        put_doc(source.as_ref(), "new", serde_json::json!({})).await;
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Change { .. })).await);
        handle.cancel();

        assert!(target.get("new", GetOptions::default()).await.is_ok());
        assert!(target.get("old", GetOptions::default()).await.is_err());
    }

    /// A memory source that gets a new document written while the first
    /// replication pass reads its changes feed (after the read).
    struct WritesDuringFirstPass {
        inner: MemoryAdapter,
        written: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Adapter for WritesDuringFirstPass {
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
            self.inner.bulk_docs(docs, opts).await
        }
        async fn all_docs(&self, opts: AllDocsOptions) -> Result<AllDocsResponse> {
            self.inner.all_docs(opts).await
        }
        async fn changes(&self, opts: ChangesOptions) -> Result<ChangesResponse> {
            let changes = self.inner.changes(opts).await;
            if !self.written.swap(true, std::sync::atomic::Ordering::SeqCst) {
                put_doc(&self.inner, "late", serde_json::json!({})).await;
            }
            changes
        }
        fn subscribe(&self) -> Option<broadcast::Receiver<ChangeNotice>> {
            self.inner.subscribe()
        }
        async fn revs_diff(&self, revs: HashMap<String, Vec<String>>) -> Result<RevsDiffResponse> {
            self.inner.revs_diff(revs).await
        }
        async fn bulk_get(&self, docs: Vec<BulkGetItem>) -> Result<BulkGetResponse> {
            self.inner.bulk_get(docs).await
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

    /// Live replication only reports `Paused` once caught up: a change made
    /// during a pass is replicated by another pass first.
    #[tokio::test(start_paused = true)]
    async fn live_replication_pauses_only_when_caught_up() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "a", serde_json::json!({})).await;
        let source = Arc::new(WritesDuringFirstPass {
            inner,
            written: std::sync::atomic::AtomicBool::new(false),
        });
        let target = Arc::new(MemoryAdapter::new("target"));
        let (mut rx, handle) = replicate_live(
            source,
            target.clone(),
            ReplicationOptions {
                live: true,
                poll_interval: Duration::from_secs(3600),
                ..Default::default()
            },
        );
        // Events are sent in order: both passes report their change before
        // the first Paused.
        let mut changes = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(ReplicationEvent::Change { .. })) => changes += 1,
                Ok(Some(ReplicationEvent::Paused)) => break,
                Ok(Some(_)) => {}
                other => panic!("no Paused: {other:?}"),
            }
        }
        assert_eq!(changes, 2, "Paused before the change made during a pass");
        for id in ["a", "late"] {
            assert!(target.get(id, GetOptions::default()).await.is_ok(), "{id}");
        }
        handle.cancel();
    }

    /// F96: a local source announces its changes, so live replication
    /// replicates them without waiting for a poll (on a paused clock a poll
    /// would take an hour, and `wait_for` gives up after 5 s), and a change
    /// made while a pass runs is not lost.
    #[tokio::test(start_paused = true)]
    async fn live_replication_wakes_on_source_notifications() {
        let hour = Duration::from_secs(3600);
        let source = Arc::new(MemoryAdapter::new("source"));
        let target = Arc::new(MemoryAdapter::new("target"));
        put_doc(source.as_ref(), "a", serde_json::json!({})).await;
        let start = tokio::time::Instant::now();
        let (mut rx, handle) = replicate_live(
            source.clone(),
            target.clone(),
            ReplicationOptions {
                live: true,
                poll_interval: hour,
                ..Default::default()
            },
        );
        // Caught up after the first pass, which read `a`.
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
        assert!(target.get("a", GetOptions::default()).await.is_ok());
        for id in ["b", "c"] {
            put_doc(source.as_ref(), id, serde_json::json!({})).await;
            assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
            assert!(target.get(id, GetOptions::default()).await.is_ok(), "{id}");
        }
        // Writes racing with the passes.
        let writer = {
            let source = source.clone();
            tokio::spawn(async move {
                for i in 0..100 {
                    put_doc(source.as_ref(), &format!("r{i}"), serde_json::json!({})).await;
                    tokio::task::yield_now().await;
                }
            })
        };
        writer.await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(5), async {
            while target.info().await.unwrap().doc_count < 103 {
                assert!(rx.recv().await.is_some());
            }
        })
        .await;
        assert!(
            done.is_ok(),
            "{} docs",
            target.info().await.unwrap().doc_count
        );
        assert!(start.elapsed() < hour, "waited {:?}", start.elapsed());
        handle.cancel();
    }

    #[tokio::test]
    async fn live_replication_without_checkpoints_does_not_rescan() {
        let source = Arc::new(MemoryAdapter::new("source"));
        let target = Arc::new(MemoryAdapter::new("target"));
        for i in 0..3 {
            put_doc(source.as_ref(), &format!("d{i}"), serde_json::json!({})).await;
        }

        let (mut rx, handle) = replicate_live(
            source,
            target.clone(),
            ReplicationOptions {
                live: true,
                checkpoint: false,
                poll_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        // Once caught up, later polls must start from where the last one
        // ended instead of re-reading the whole feed, i.e. go idle.
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
        handle.cancel();
        assert_eq!(target.info().await.unwrap().doc_count, 3);
    }

    #[tokio::test]
    async fn checkpoint_saved_after_already_synced_batches() {
        let a = MemoryAdapter::new("a");
        let b = MemoryAdapter::new("b");
        for i in 0..10 {
            put_doc(&a, &format!("d{i}"), serde_json::json!({})).await;
        }
        replicate(&a, &b, ReplicationOptions::default())
            .await
            .unwrap();
        let opts = || ReplicationOptions {
            batch_size: 3,
            ..Default::default()
        };

        // B's feed only holds docs A already has: nothing is written...
        let r1 = replicate(&b, &a, opts()).await.unwrap();
        assert_eq!((r1.docs_read, r1.docs_written), (10, 0));
        // ...but the progress is kept, so the next run does not rescan.
        let r2 = replicate(&b, &a, opts()).await.unwrap();
        assert_eq!(r2.docs_read, 0);
    }

    #[tokio::test]
    async fn checkpoint_saved_when_filter_matches_nothing() {
        let source = MemoryAdapter::new("source");
        let target = MemoryAdapter::new("target");
        for i in 0..5 {
            put_doc(&source, &format!("d{i}"), serde_json::json!({})).await;
        }
        let filter = Some(ReplicationFilter::DocIds(vec!["missing".into()]));

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: filter.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.ok);
        assert_eq!(result.last_seq, Seq::Num(5));

        let checkpointer = new_checkpointer(&source, &target, &filter).await.unwrap();
        let since = checkpointer
            .read_checkpoint(&source, &target)
            .await
            .unwrap();
        assert_eq!(since, Seq::Num(5));
    }

    #[tokio::test]
    async fn read_only_source_still_checkpoints() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "a", serde_json::json!({})).await;
        put_doc(&inner, "b", serde_json::json!({})).await;
        let source = Faulty::new(
            inner,
            Faults {
                put_local_error: Some(|| RouchError::Forbidden("read only".into())),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r1.ok, "{:?}", r1.errors);
        assert_eq!(r1.docs_read, 2);

        // The target's checkpoint alone lets the next pull resume.
        put_doc(&source.inner, "c", serde_json::json!({})).await;
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(r2.docs_read, 1);
    }

    #[tokio::test]
    async fn checkpoint_write_errors_are_reported() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({})).await;
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                put_local_error: Some(|| RouchError::DatabaseError("disk full".into())),
                ..Default::default()
            },
        );

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(result.errors[0].contains("disk full"));
    }

    #[tokio::test]
    async fn live_replication_started_offline_checkpoints_under_the_real_id() {
        let source = Arc::new(MemoryAdapter::new("source"));
        for i in 0..3 {
            put_doc(source.as_ref(), &format!("d{i}"), serde_json::json!({})).await;
        }
        let target = Arc::new(Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                offline_id: Some("http://target/db".into()),
                ..Default::default()
            },
        ));

        let (mut rx, handle) = replicate_live(
            source.clone(),
            target.clone(),
            ReplicationOptions {
                live: true,
                retry: true,
                back_off_function: Some(Box::new(|_| Duration::from_millis(10))),
                poll_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Error(_))).await);
        target.heal(); // back online
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
        handle.cancel();

        // The progress is stored under the id computed once the target
        // answered, so the next session resumes instead of rescanning.
        let checkpointer = new_checkpointer(source.as_ref(), target.as_ref(), &None)
            .await
            .unwrap();
        let since = checkpointer
            .read_checkpoint(source.as_ref(), target.as_ref())
            .await
            .unwrap();
        assert_eq!(since, Seq::Num(3));
    }

    #[test]
    fn seq_after_orders_numeric_and_opaque_sequences() {
        let num = Seq::Num;
        assert!(seq_after(&num(5), &num(3)));
        assert!(!seq_after(&num(3), &num(3)));
        assert!(!seq_after(&num(2), &num(3)));

        // Opaque CouchDB sequences compare by their numeric prefix only.
        let s = |v: &str| Seq::Str(v.into());
        assert!(seq_after(&s("5-g1AAAAB"), &s("3-g1AAAAA")));
        assert!(!seq_after(&s("3-g1AAAAA"), &s("3-g1AAAAA")));
        assert!(!seq_after(&s("3-g1AAAAA"), &s("5-g1AAAAB")));
        // 12 > 9 as numbers, though "12" < "9" as text.
        assert!(seq_after(&s("12-g1AAAAB"), &s("9-g1AAAAA")));
        assert!(!seq_after(&s("9-g1AAAAA"), &s("12-g1AAAAB")));
    }

    async fn target_ids(target: &dyn Adapter) -> Vec<String> {
        target
            .all_docs(AllDocsOptions::new())
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.key)
            .collect()
    }

    #[tokio::test]
    async fn transport_error_mid_run_resumes_from_the_last_checkpointed_batch() {
        let source = MemoryAdapter::new("source");
        for i in 0..10 {
            put_doc(&source, &format!("d{i}"), serde_json::json!({})).await;
        }
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                bulk_docs_fails_on_call: Some(3),
                ..Default::default()
            },
        );
        let opts = || ReplicationOptions {
            batch_size: 2,
            ..Default::default()
        };

        let first = replicate(&source, &target, opts()).await;
        assert!(
            matches!(first, Err(RouchError::DatabaseError(ref m)) if m == "connection reset"),
            "{first:?}"
        );
        assert_eq!(target_ids(&target).await, vec!["d0", "d1", "d2", "d3"]);

        // Batches 1 and 2 were checkpointed before the failure: the next run
        // starts at the third batch, neither from zero nor past it.
        target.heal();
        let second = replicate(&source, &target, opts()).await.unwrap();
        assert!(second.ok, "{:?}", second.errors);
        assert_eq!((second.docs_read, second.docs_written), (6, 6));
        assert_eq!(second.last_seq, Seq::Num(10));
        assert_eq!(target.info().await.unwrap().doc_count, 10);
    }

    #[tokio::test]
    async fn bulk_get_not_found_for_a_live_leaf_fails_the_batch() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "a", serde_json::json!({"v": 1})).await;
        put_doc(&inner, "b", serde_json::json!({"v": 2})).await;
        let source = Faulty::new(
            inner,
            Faults {
                bulk_get_error: Some(("b".into(), "not_found".into())),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        // `b` is still a leaf on the source: not_found there is a failure to
        // retry, not a superseded rev to skip.
        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(!r1.ok);
        assert_eq!(r1.errors.len(), 1, "{:?}", r1.errors);
        assert!(
            r1.errors[0].starts_with("fetch error for b 1-"),
            "{:?}",
            r1.errors
        );

        source.heal();
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(target_ids(&target).await, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn bulk_get_not_found_for_a_conflicting_leaf_fails_the_batch() {
        let inner = MemoryAdapter::new("source");
        put_rev(&inner, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
        put_rev(&inner, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;
        let source = Faulty::new(
            inner,
            Faults {
                bulk_get_missing_rev: Some(("d".into(), "2-bbb".into())),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        // The losing branch is a leaf too: it must not be skipped as if it
        // had been superseded.
        let r1 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(!r1.ok);
        assert_eq!(
            r1.errors,
            vec!["fetch error for d 2-bbb: not_found: missing".to_string()]
        );

        source.heal();
        let r2 = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(conflicts_of(&target, "d").await, vec!["2-bbb"]);
    }

    #[tokio::test]
    async fn unauthorized_write_is_denied_like_forbidden() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({})).await;
        put_doc(&source, "x", serde_json::json!({})).await;
        put_doc(&source, "b", serde_json::json!({})).await;
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                write_error: Some(("x".into(), "unauthorized".into())),
                ..Default::default()
            },
        );
        let opts = || ReplicationOptions {
            batch_size: 1,
            ..Default::default()
        };

        // CouchDB validators may throw `unauthorized` as well as `forbidden`;
        // both are final, so the replication moves past the doc.
        let r1 = replicate(&source, &target, opts()).await.unwrap();
        assert!(!r1.ok);
        assert_eq!((r1.docs_read, r1.docs_written), (3, 2));
        assert_eq!(
            r1.errors,
            vec!["write error for x: unauthorized: injected".to_string()]
        );
        assert_eq!(target_ids(&target).await, vec!["a", "b"]);

        let r2 = replicate(&source, &target, opts()).await.unwrap();
        assert!(r2.ok, "{:?}", r2.errors);
        assert_eq!(r2.docs_read, 0);
    }

    #[tokio::test]
    async fn checkpoint_read_errors_are_surfaced() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({})).await;
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                get_local_error: Some(|| RouchError::Unauthorized),
                ..Default::default()
            },
        );

        // Only a missing checkpoint means "start over"; a denied read must
        // not silently rescan (or write) anything.
        let result = replicate(&source, &target, ReplicationOptions::default()).await;
        assert!(
            matches!(result, Err(RouchError::Unauthorized)),
            "{result:?}"
        );
        assert_eq!(target.info().await.unwrap().doc_count, 0);
    }

    #[tokio::test]
    async fn source_checkpoint_write_errors_are_reported() {
        let inner = MemoryAdapter::new("source");
        put_doc(&inner, "a", serde_json::json!({})).await;
        let source = Faulty::new(
            inner,
            Faults {
                put_local_error: Some(|| RouchError::DatabaseError("disk full".into())),
                ..Default::default()
            },
        );
        let target = MemoryAdapter::new("target");

        // Unlike Forbidden/Unauthorized (a read-only source, see
        // read_only_source_still_checkpoints), a failed write is an error.
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(
            result.errors,
            vec!["checkpoint write failed: database error: disk full".to_string()]
        );
        assert_eq!(result.docs_written, 1);
    }

    #[tokio::test]
    async fn failed_probe_of_a_replaced_source_rescans_from_the_start() {
        let target = MemoryAdapter::new("target");
        let old_source = MemoryAdapter::new("src");
        for i in 0..5 {
            put_doc(&old_source, &format!("old{i}"), serde_json::json!({})).await;
        }
        replicate(&old_source, &target, ReplicationOptions::default())
            .await
            .unwrap();

        // Same name (so same replication id), fresh feed at seq 2, and the
        // probe write fails for a reason other than permissions: the source
        // is not known to be read-only, so the target's seq 5 is not trusted.
        let inner = MemoryAdapter::new("src");
        put_doc(&inner, "x", serde_json::json!({})).await;
        put_doc(&inner, "y", serde_json::json!({})).await;
        let source = Faulty::new(
            inner,
            Faults {
                put_local_error: Some(|| RouchError::DatabaseError("disk full".into())),
                ..Default::default()
            },
        );
        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert_eq!((result.docs_read, result.docs_written), (2, 2));
        assert!(target.get("x", GetOptions::default()).await.is_ok());
        assert!(target.get("y", GetOptions::default()).await.is_ok());
    }

    #[tokio::test]
    async fn checkpoint_write_conflict_is_retried_once() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "a", serde_json::json!({})).await;
        // CouchDB 3.5 never answers 409 to a `_local` write (it does not check
        // `_rev`), but another writer or server may: re-read and retry once.
        let target = Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                put_local_conflicts: 1,
                ..Default::default()
            },
        );

        let result = replicate(&source, &target, ReplicationOptions::default())
            .await
            .unwrap();
        assert!(result.ok, "{:?}", result.errors);
        let checkpointer = new_checkpointer(&source, &target, &None).await.unwrap();
        let cp = target
            .get_local(checkpointer.replication_id())
            .await
            .unwrap();
        assert_eq!(cp["last_seq"], 1);
    }

    #[tokio::test]
    async fn a_short_batch_ends_the_pass_without_another_changes_request() {
        let inner = MemoryAdapter::new("source");
        for id in ["a", "b", "c"] {
            put_doc(&inner, id, serde_json::json!({})).await;
        }
        let source = Faulty::new(inner, Faults::default());
        let target = MemoryAdapter::new("target");
        // Fewer changes than batch_size means the feed is exhausted, whether
        // the batch was written, already on the target, or filtered out.
        let runs = [
            ReplicationOptions::default(),
            ReplicationOptions {
                checkpoint: false,
                ..Default::default()
            },
            ReplicationOptions {
                filter: Some(ReplicationFilter::Custom(Arc::new(|_| false))),
                ..Default::default()
            },
        ];
        for (i, opts) in runs.into_iter().enumerate() {
            let before = source
                .changes_calls
                .load(std::sync::atomic::Ordering::SeqCst);
            let result = replicate(&source, &target, opts).await.unwrap();
            assert!(result.ok, "{:?}", result.errors);
            assert_eq!(result.docs_read, [3, 3, 0][i]);
            let calls = source
                .changes_calls
                .load(std::sync::atomic::Ordering::SeqCst)
                - before;
            assert_eq!(calls, 1, "run {i} asked for changes {calls} times");
        }
    }

    #[tokio::test]
    async fn custom_filter_keeps_scanning_past_a_fully_rejected_batch() {
        let source = MemoryAdapter::new("source");
        for id in ["a1", "a2", "b1", "b2", "b3"] {
            put_doc(&source, id, serde_json::json!({})).await;
        }
        let target = MemoryAdapter::new("target");

        // The first batch of two is entirely filtered out; later batches
        // still hold matching docs.
        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                batch_size: 2,
                filter: Some(ReplicationFilter::Custom(Arc::new(|c| {
                    c.id.starts_with('b')
                }))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.ok, "{:?}", result.errors);
        assert_eq!((result.docs_read, result.docs_written), (3, 3));
        assert_eq!(target_ids(&target).await, vec!["b1", "b2", "b3"]);
    }

    #[tokio::test]
    async fn selector_on_deleted_replicates_only_tombstones() {
        let source = MemoryAdapter::new("source");
        put_doc(&source, "kept", serde_json::json!({"v": 1})).await;
        put_doc(&source, "gone", serde_json::json!({"v": 2})).await;
        let gone = source.get("gone", GetOptions::default()).await.unwrap();
        let tombstone = source
            .bulk_docs(
                vec![Document {
                    id: "gone".into(),
                    rev: gone.rev,
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
        let target = MemoryAdapter::new("target");

        let result = replicate(
            &source,
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::Selector(
                    serde_json::json!({"_deleted": true}),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.ok, "{:?}", result.errors);
        assert_eq!((result.docs_read, result.docs_written), (2, 1));

        let feed = target.changes(ChangesOptions::default()).await.unwrap();
        let got: Vec<(&str, &str, bool)> = feed
            .results
            .iter()
            .map(|c| (c.id.as_str(), c.changes[0].rev.as_str(), c.deleted))
            .collect();
        assert_eq!(got, vec![("gone", tombstone.as_str(), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn live_retry_backs_off_exponentially_by_default() {
        let source = Arc::new(MemoryAdapter::new("source"));
        put_doc(source.as_ref(), "d", serde_json::json!({})).await;
        let target = Arc::new(Faulty::new(
            MemoryAdapter::new("target"),
            Faults {
                offline_id: Some("http://target/db".into()),
                ..Default::default()
            },
        ));

        let (mut rx, handle) = replicate_live(
            source,
            target,
            ReplicationOptions {
                live: true,
                retry: true,
                ..Default::default()
            },
        );
        // Virtual time: the gaps between failed attempts are the delays.
        let mut failed_at = Vec::new();
        while failed_at.len() < 4 {
            match rx.recv().await {
                Some(ReplicationEvent::Error(_)) => failed_at.push(tokio::time::Instant::now()),
                Some(_) => {}
                None => panic!("live replication ended while retrying"),
            }
        }
        handle.cancel();
        let gaps: Vec<Duration> = failed_at.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            gaps,
            [2, 4, 8].map(Duration::from_secs),
            "default backoff is 2^attempt seconds"
        );
    }

    /// Start a live replication of one doc and wait until it is idle.
    async fn idle_live_replication() -> (mpsc::Receiver<ReplicationEvent>, ReplicationHandle) {
        let source = Arc::new(MemoryAdapter::new("source"));
        put_doc(source.as_ref(), "d", serde_json::json!({})).await;
        let (mut rx, handle) = replicate_live(
            source,
            Arc::new(MemoryAdapter::new("target")),
            ReplicationOptions {
                live: true,
                poll_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
        (rx, handle)
    }

    /// Drain the channel until it closes (bounded).
    async fn remaining_events(rx: &mut mpsc::Receiver<ReplicationEvent>) -> Vec<ReplicationEvent> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        })
        .await
        .expect("live replication kept running")
    }

    #[tokio::test]
    async fn cancel_ends_live_replication_with_one_complete() {
        let (mut rx, handle) = idle_live_replication().await;
        handle.cancel();
        let events = remaining_events(&mut rx).await;
        let completes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ReplicationEvent::Complete(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(completes.len(), 1, "{events:?}");
        assert!(completes[0].ok);
        assert!(matches!(events.last(), Some(ReplicationEvent::Complete(_))));
        drop(handle);
    }

    #[tokio::test]
    async fn dropping_the_handle_ends_live_replication() {
        let (mut rx, handle) = idle_live_replication().await;
        drop(handle);
        let events = remaining_events(&mut rx).await;
        assert!(
            matches!(events.last(), Some(ReplicationEvent::Complete(r)) if r.ok),
            "{events:?}"
        );
    }
}
