/// Streaming changes feed for RouchDB.
///
/// Provides a `ChangesStream` that wraps the adapter's `changes()` method
/// and supports:
/// - One-shot mode: fetch changes since a sequence and return
/// - Live/continuous mode: keep polling for new changes
/// - Filtering by document IDs
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::{ChangeEvent, ChangesOptions, ChangesResponse, ChangesStyle, Seq};

/// A filter function for changes events.
pub type ChangesFilter = Arc<dyn Fn(&ChangeEvent) -> bool + Send + Sync>;

/// Lifecycle events emitted by a live changes stream.
///
/// Mirrors PouchDB's changes event model: `change`, `complete`, `error`,
/// `paused`, and `active`.
#[derive(Debug, Clone)]
pub enum ChangesEvent {
    /// A document changed.
    Change(ChangeEvent),
    /// The stream completed (limit reached or non-live mode ended).
    Complete { last_seq: Seq },
    /// An error occurred while fetching changes.
    Error(String),
    /// The stream is caught up and waiting for new changes.
    Paused,
    /// The stream resumed fetching after being paused.
    Active,
    /// A periodic keep-alive emitted while waiting, per the `heartbeat` option.
    Heartbeat,
}
use rouchdb_core::error::Result;

/// A notification that a change occurred, sent through the broadcast channel.
#[derive(Debug, Clone)]
pub struct ChangeNotification {
    pub seq: Seq,
    pub doc_id: String,
}

/// A sender for change notifications. Adapters use this to notify listeners
/// when documents are written.
#[derive(Debug, Clone)]
pub struct ChangeSender {
    tx: broadcast::Sender<ChangeNotification>,
}

impl ChangeSender {
    pub fn new(capacity: usize) -> (Self, ChangeReceiver) {
        let (tx, rx) = broadcast::channel(capacity);
        (ChangeSender { tx }, ChangeReceiver { rx })
    }

    pub fn notify(&self, seq: Seq, doc_id: String) {
        // Ignore send errors (no receivers)
        let _ = self.tx.send(ChangeNotification { seq, doc_id });
    }

    pub fn subscribe(&self) -> ChangeReceiver {
        ChangeReceiver {
            rx: self.tx.subscribe(),
        }
    }
}

/// A receiver for change notifications.
pub struct ChangeReceiver {
    rx: broadcast::Receiver<ChangeNotification>,
}

impl ChangeReceiver {
    pub async fn recv(&mut self) -> Option<ChangeNotification> {
        loop {
            match self.rx.recv().await {
                Ok(notification) => return Some(notification),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Missed some messages, continue receiving
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Configuration for a changes stream.
#[derive(Clone)]
pub struct ChangesStreamOptions {
    pub since: Seq,
    pub live: bool,
    pub include_docs: bool,
    pub doc_ids: Option<Vec<String>>,
    /// Mango selector: only changes whose doc matches are emitted. The docs
    /// are fetched to evaluate it and dropped again unless `include_docs`.
    pub selector: Option<serde_json::Value>,
    pub limit: Option<u64>,
    /// Include conflicting revisions per change event.
    pub conflicts: bool,
    /// Changes style: `MainOnly` (default) or `AllDocs`.
    pub style: ChangesStyle,
    /// A filter function applied post-fetch to each change event.
    pub filter: Option<ChangesFilter>,
    /// Polling interval for live mode when no broadcast channel is available.
    pub poll_interval: Duration,
    /// In live mode, end the stream (with `Complete`) after this long
    /// without new changes.
    pub timeout: Option<Duration>,
    /// In live mode, emit a `Heartbeat` event at this interval while
    /// waiting for changes.
    pub heartbeat: Option<Duration>,
}

impl Default for ChangesStreamOptions {
    fn default() -> Self {
        Self {
            since: Seq::default(),
            live: false,
            include_docs: false,
            doc_ids: None,
            selector: None,
            limit: None,
            conflicts: false,
            style: ChangesStyle::default(),
            filter: None,
            poll_interval: Duration::from_millis(500),
            timeout: None,
            heartbeat: None,
        }
    }
}

impl std::fmt::Debug for ChangesStreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangesStreamOptions")
            .field("since", &self.since)
            .field("live", &self.live)
            .field("include_docs", &self.include_docs)
            .field("doc_ids", &self.doc_ids)
            .field("selector", &self.selector)
            .field("limit", &self.limit)
            .field("conflicts", &self.conflicts)
            .field("style", &self.style)
            .field("filter", &self.filter.as_ref().map(|_| "<fn>"))
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("heartbeat", &self.heartbeat)
            .finish()
    }
}

/// Fold `opts.selector` into `opts.filter`: the selector needs the docs, so
/// they are requested. Returns whether they must be stripped again because
/// the caller did not ask for them.
fn apply_selector(opts: &mut ChangesStreamOptions) -> bool {
    let Some(selector) = opts.selector.take() else {
        return false;
    };
    let existing = opts.filter.take();
    opts.filter = Some(Arc::new(move |e: &ChangeEvent| {
        existing.as_ref().is_none_or(|f| f(e))
            && e.doc
                .as_ref()
                .is_some_and(|d| rouchdb_query::matches_selector(d, &selector))
    }));
    let strip_docs = !opts.include_docs;
    opts.include_docs = true;
    strip_docs
}

/// Whether `new` is strictly later than `old`. Opaque CouchDB sequences are
/// only comparable by their numeric prefix.
fn seq_after(new: &Seq, old: &Seq) -> bool {
    match (new, old) {
        (Seq::Num(n), Seq::Num(o)) => n > o,
        _ => new != old && new.as_num() >= old.as_num(),
    }
}

/// Fetch changes from an adapter in one-shot mode.
pub async fn get_changes(
    adapter: &dyn Adapter,
    mut opts: ChangesStreamOptions,
) -> Result<Vec<ChangeEvent>> {
    let strip_docs = apply_selector(&mut opts);
    let filter = opts.filter.clone();
    let limit = opts.limit;
    let changes_opts = ChangesOptions {
        since: opts.since,
        // With a filter, the limit applies to POST-filter results, so don't
        // let the adapter cap the fetch by limit (it would under-deliver).
        limit: if filter.is_some() { None } else { opts.limit },
        descending: false,
        include_docs: opts.include_docs,
        live: false,
        doc_ids: opts.doc_ids,
        conflicts: opts.conflicts,
        style: opts.style,
        ..Default::default()
    };

    let response = adapter.changes(changes_opts).await?;
    let mut results: Vec<ChangeEvent> = if let Some(f) = filter {
        response.results.into_iter().filter(|e| f(e)).collect()
    } else {
        response.results
    };
    if let Some(l) = limit {
        results.truncate(l as usize);
    }
    if strip_docs {
        for event in &mut results {
            event.doc = None;
        }
    }
    Ok(results)
}

/// Longest wait between retries after the adapter failed.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// A live changes stream that yields change events as they happen.
///
/// In live mode, after fetching existing changes, it waits for
/// notifications via a broadcast channel or polls at regular intervals.
/// A failed fetch is reported as [`ChangesEvent::Error`]; a live stream then
/// retries with a growing delay, a one-shot stream ends.
pub struct LiveChangesStream {
    adapter: Arc<dyn Adapter>,
    receiver: Option<ChangeReceiver>,
    opts: ChangesStreamOptions,
    /// Sequence of the last change consumed (emitted or filtered out), or
    /// the feed position once the whole fetched batch is consumed.
    last_seq: Seq,
    buffer: Vec<ChangeEvent>,
    buffer_idx: usize,
    /// `last_seq` reported with the buffered batch.
    buffer_last_seq: Seq,
    state: LiveStreamState,
    count: u64,
    /// Docs were only fetched to evaluate the selector.
    strip_docs: bool,
    /// Next poll (polling mode, while waiting).
    next_poll: Option<Instant>,
    /// When a waiting stream times out (the `timeout` option).
    idle_deadline: Option<Instant>,
    /// Next heartbeat (the `heartbeat` option).
    next_heartbeat: Option<Instant>,
    /// Paused was emitted and no change has arrived since.
    paused: bool,
    /// Consecutive failed fetches.
    failures: u32,
    /// A fetch in flight, kept here so that dropping `next_event()` midway
    /// resumes it on the next call instead of aborting it.
    pending_fetch: Option<PendingFetch>,
}

type PendingFetch = Pin<Box<dyn Future<Output = Result<ChangesResponse>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveStreamState {
    /// Fetching the initial batch of changes.
    FetchingInitial,
    /// Yielding buffered results.
    Yielding,
    /// Waiting for new notifications.
    Waiting,
    /// Done (limit reached, timed out, or closed); Complete not yet emitted.
    Done,
    /// Complete (or a terminal Error) was emitted.
    Finished,
}

impl LiveChangesStream {
    pub fn new(
        adapter: Arc<dyn Adapter>,
        receiver: Option<ChangeReceiver>,
        mut opts: ChangesStreamOptions,
    ) -> Self {
        let strip_docs = apply_selector(&mut opts);
        let last_seq = opts.since.clone();
        let next_heartbeat = opts
            .heartbeat
            .filter(|_| opts.live)
            .map(|d| Instant::now() + d);
        Self {
            adapter,
            receiver,
            buffer_last_seq: last_seq.clone(),
            last_seq,
            opts,
            buffer: Vec::new(),
            buffer_idx: 0,
            state: LiveStreamState::FetchingInitial,
            count: 0,
            strip_docs,
            next_poll: None,
            idle_deadline: None,
            next_heartbeat,
            paused: false,
            failures: 0,
            pending_fetch: None,
        }
    }

    /// The sequence to resume from: the last change consumed.
    pub fn last_seq(&self) -> &Seq {
        &self.last_seq
    }

    /// Fetch changes since `last_seq` and buffer them (resuming a fetch
    /// left in flight by a dropped call).
    async fn fetch_changes(&mut self) -> Result<()> {
        if self.pending_fetch.is_none() {
            let changes_opts = ChangesOptions {
                since: self.last_seq.clone(),
                // A post-fetch filter decides what counts toward the limit, so
                // the adapter must not cap the batch (it would under-deliver).
                limit: match self.opts.filter {
                    Some(_) => None,
                    None => self.opts.limit.map(|l| l.saturating_sub(self.count)),
                },
                descending: false,
                include_docs: self.opts.include_docs,
                live: false,
                doc_ids: self.opts.doc_ids.clone(),
                conflicts: self.opts.conflicts,
                style: self.opts.style.clone(),
                ..Default::default()
            };
            let adapter = self.adapter.clone();
            self.pending_fetch = Some(Box::pin(async move { adapter.changes(changes_opts).await }));
        }
        let fetched = self
            .pending_fetch
            .as_mut()
            .expect("pending fetch set above")
            .await;
        self.pending_fetch = None;
        let response = fetched?;
        self.buffer = response.results;
        self.buffer_idx = 0;
        self.buffer_last_seq = response.last_seq;
        self.failures = 0;
        // Nothing to consume: the feed may still have moved past changes a
        // doc_ids filter excluded.
        if self.buffer.is_empty() && seq_after(&self.buffer_last_seq, &self.last_seq) {
            self.last_seq = self.buffer_last_seq.clone();
        }
        Ok(())
    }

    /// Next buffered event that passes the filter, advancing `last_seq`.
    fn pop_buffered(&mut self) -> Option<ChangeEvent> {
        while self.buffer_idx < self.buffer.len() {
            let mut event = self.buffer[self.buffer_idx].clone();
            self.buffer_idx += 1;
            self.last_seq = event.seq.clone();
            if self.buffer_idx == self.buffer.len()
                && seq_after(&self.buffer_last_seq, &self.last_seq)
            {
                self.last_seq = self.buffer_last_seq.clone();
            }
            // Apply the user filter here so `limit` (and `count`) reflect
            // emitted, not merely scanned, events.
            if let Some(ref f) = self.opts.filter
                && !f(&event)
            {
                continue;
            }
            if self.strip_docs {
                event.doc = None;
            }
            self.count += 1;
            return Some(event);
        }
        None
    }

    /// Start waiting for new changes.
    fn enter_waiting(&mut self) {
        let now = Instant::now();
        self.state = LiveStreamState::Waiting;
        self.next_poll = Some(now + self.opts.poll_interval);
        if self.idle_deadline.is_none() {
            self.idle_deadline = self.opts.timeout.map(|t| now + t);
        }
    }

    /// A fetch failed: a live stream retries later, a one-shot stream ends.
    fn fetch_failed(&mut self, error: rouchdb_core::error::RouchError) -> ChangesEvent {
        if self.opts.live {
            self.failures = self.failures.saturating_add(1);
            let delay = self
                .opts
                .poll_interval
                .saturating_mul(1u32 << self.failures.min(8))
                .min(MAX_RETRY_DELAY);
            self.enter_waiting();
            self.next_poll = Some(Instant::now() + delay);
        } else {
            self.state = LiveStreamState::Finished;
        }
        ChangesEvent::Error(error.to_string())
    }

    /// Fetched while waiting: switch to yielding if there is anything new.
    fn resume_if_changed(&mut self) -> Option<ChangesEvent> {
        if self.buffer.is_empty() {
            return None;
        }
        self.state = LiveStreamState::Yielding;
        self.idle_deadline = None;
        if self.paused {
            self.paused = false;
            return Some(ChangesEvent::Active);
        }
        None
    }

    /// Get the next change event, blocking if in live mode.
    ///
    /// Lifecycle events are skipped; `None` means the stream ended. In live
    /// mode a failed fetch is retried (see [`Self::next_event`] to observe it).
    pub async fn next_change(&mut self) -> Option<ChangeEvent> {
        loop {
            match self.next_event().await? {
                ChangesEvent::Change(event) => return Some(event),
                ChangesEvent::Complete { .. } => return None,
                ChangesEvent::Error(_) if !self.opts.live => return None,
                _ => {}
            }
        }
    }

    /// Get the next event: changes plus the `Paused`/`Active` transitions,
    /// `Heartbeat`s while waiting, `Error`s, and a final `Complete`. Returns
    /// `None` once the stream has ended.
    pub async fn next_event(&mut self) -> Option<ChangesEvent> {
        loop {
            if let Some(limit) = self.opts.limit
                && self.count >= limit
                && self.state != LiveStreamState::Finished
            {
                self.state = LiveStreamState::Done;
            }

            match self.state {
                LiveStreamState::FetchingInitial => {
                    if let Err(e) = self.fetch_changes().await {
                        return Some(self.fetch_failed(e));
                    }
                    self.state = LiveStreamState::Yielding;
                }
                LiveStreamState::Yielding => {
                    if let Some(event) = self.pop_buffered() {
                        return Some(ChangesEvent::Change(event));
                    }
                    // Buffer exhausted: caught up.
                    if !self.opts.live {
                        self.state = LiveStreamState::Done;
                        continue;
                    }
                    self.enter_waiting();
                    if !self.paused {
                        self.paused = true;
                        return Some(ChangesEvent::Paused);
                    }
                }
                LiveStreamState::Waiting => {
                    let now = Instant::now();

                    // Timed out: look once more so a change made just before
                    // the deadline is not lost, then end.
                    if self.idle_deadline.is_some_and(|d| now >= d) {
                        if let Err(e) = self.fetch_changes().await {
                            self.state = LiveStreamState::Finished;
                            return Some(ChangesEvent::Error(e.to_string()));
                        }
                        match self.resume_if_changed() {
                            Some(event) => return Some(event),
                            None if self.state == LiveStreamState::Yielding => continue,
                            None => {
                                self.state = LiveStreamState::Done;
                                continue;
                            }
                        }
                    }

                    if let Some(heartbeat) = self.next_heartbeat
                        && now >= heartbeat
                    {
                        self.next_heartbeat = self.opts.heartbeat.map(|d| now + d);
                        return Some(ChangesEvent::Heartbeat);
                    }

                    // Sleep until the next poll, heartbeat or deadline (or a
                    // notification), unless a fetch was left in flight.
                    // Deadlines and the fetch live in `self`, so dropping this
                    // future at any point loses nothing.
                    if self.pending_fetch.is_none() && !self.wait_for_fetch().await {
                        continue;
                    }
                    let fetched = self.fetch_changes().await;
                    self.next_poll = Some(Instant::now() + self.opts.poll_interval);
                    if let Err(e) = fetched {
                        return Some(self.fetch_failed(e));
                    }
                    if let Some(event) = self.resume_if_changed() {
                        return Some(event);
                    }
                    // Caught up after recovering from a failed fetch.
                    if self.state == LiveStreamState::Waiting && !self.paused {
                        self.paused = true;
                        return Some(ChangesEvent::Paused);
                    }
                }
                LiveStreamState::Done => {
                    self.state = LiveStreamState::Finished;
                    return Some(ChangesEvent::Complete {
                        last_seq: self.last_seq.clone(),
                    });
                }
                LiveStreamState::Finished => return None,
            }
        }
    }

    /// Wait until a fetch is due (poll time or notification). Returns false
    /// when woken for something else (heartbeat, deadline, closed channel),
    /// which the caller re-evaluates.
    async fn wait_for_fetch(&mut self) -> bool {
        let polling = self.receiver.is_none() || self.failures > 0;
        let wake = [
            if polling { self.next_poll } else { None },
            self.next_heartbeat,
            self.idle_deadline,
        ]
        .into_iter()
        .flatten()
        .min();
        match &mut self.receiver {
            Some(receiver) if !polling => {
                tokio::select! {
                    n = receiver.recv() => {
                        if n.is_none() {
                            // Channel closed.
                            self.state = LiveStreamState::Done;
                            return false;
                        }
                        true
                    }
                    _ = sleep_until(wake) => false,
                }
            }
            _ => {
                sleep_until(wake).await;
                self.next_poll.is_some_and(|p| Instant::now() >= p)
            }
        }
    }
}

/// Sleep until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Handle for a live changes stream. Dropping or cancelling stops the stream.
pub struct ChangesHandle {
    cancel: CancellationToken,
}

impl ChangesHandle {
    /// Cancel the live changes stream.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for ChangesHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Send `value`, giving up if the stream is cancelled meanwhile. Returns
/// whether the stream should go on.
async fn send_or_cancel<T>(tx: &mpsc::Sender<T>, value: T, cancel: &CancellationToken) -> bool {
    tokio::select! {
        sent = tx.send(value) => sent.is_ok(),
        _ = cancel.cancelled() => false,
    }
}

/// Start a live changes stream that sends events through an mpsc channel.
///
/// Spawns a background task that polls the adapter for changes and sends
/// each `ChangeEvent` through the returned receiver. The `ChangesHandle`
/// controls the stream's lifecycle; the task also stops once the receiver
/// is dropped. Failed fetches are retried with a growing delay (use
/// [`live_changes_events`] to observe them).
pub fn live_changes(
    adapter: Arc<dyn Adapter>,
    opts: ChangesStreamOptions,
) -> (mpsc::Receiver<ChangeEvent>, ChangesHandle) {
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        // The user filter is applied inside the stream so `limit` counts only
        // emitted (post-filter) events.
        let mut stream =
            LiveChangesStream::new(adapter, None, ChangesStreamOptions { live: true, ..opts });

        loop {
            let event = tokio::select! {
                event = stream.next_event() => event,
                _ = cancel_clone.cancelled() => break,
                _ = tx.closed() => break, // Receiver dropped
            };
            match event {
                Some(ChangesEvent::Change(change)) => {
                    if !send_or_cancel(&tx, change, &cancel_clone).await {
                        break;
                    }
                }
                Some(ChangesEvent::Complete { .. }) | None => break, // limit reached
                Some(_) => {}
            }
        }
    });

    (rx, ChangesHandle { cancel })
}

/// Start a live changes stream that emits lifecycle events.
///
/// Like `live_changes()` but wraps each event in a `ChangesEvent` enum
/// that includes `Active`, `Paused`, `Complete`, `Error` and `Heartbeat`
/// lifecycle events alongside the actual `Change` events.
pub fn live_changes_events(
    adapter: Arc<dyn Adapter>,
    opts: ChangesStreamOptions,
) -> (mpsc::Receiver<ChangesEvent>, ChangesHandle) {
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        let mut stream =
            LiveChangesStream::new(adapter, None, ChangesStreamOptions { live: true, ..opts });

        loop {
            let event = tokio::select! {
                event = stream.next_event() => event,
                _ = cancel_clone.cancelled() => {
                    let _ = tx.try_send(ChangesEvent::Complete {
                        last_seq: stream.last_seq().clone(),
                    });
                    break;
                }
                _ = tx.closed() => break, // Receiver dropped
            };
            let Some(event) = event else { break };
            if !send_or_cancel(&tx, event, &cancel_clone).await {
                break;
            }
        }
    });

    (rx, ChangesHandle { cancel })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_adapter_memory::MemoryAdapter;
    use rouchdb_core::document::{BulkDocsOptions, Document};
    use std::collections::HashMap;

    async fn setup() -> (Arc<MemoryAdapter>, ChangeSender) {
        let db = Arc::new(MemoryAdapter::new("test"));
        let (sender, _rx) = ChangeSender::new(64);
        (db, sender)
    }

    async fn put_doc(db: &dyn Adapter, id: &str, data: serde_json::Value) -> String {
        let doc = Document {
            id: id.into(),
            rev: None,
            deleted: false,
            data,
            attachments: HashMap::new(),
        };
        let results = db
            .bulk_docs(vec![doc], BulkDocsOptions::new())
            .await
            .unwrap();
        results[0].rev.clone().unwrap()
    }

    #[tokio::test]
    async fn one_shot_changes() {
        let (db, _sender) = setup().await;
        put_doc(db.as_ref(), "a", serde_json::json!({"v": 1})).await;
        put_doc(db.as_ref(), "b", serde_json::json!({"v": 2})).await;

        let events = get_changes(db.as_ref(), ChangesStreamOptions::default())
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "a");
        assert_eq!(events[1].id, "b");
    }

    #[tokio::test]
    async fn one_shot_changes_since() {
        let (db, _sender) = setup().await;
        put_doc(db.as_ref(), "a", serde_json::json!({})).await;
        put_doc(db.as_ref(), "b", serde_json::json!({})).await;
        put_doc(db.as_ref(), "c", serde_json::json!({})).await;

        let events = get_changes(
            db.as_ref(),
            ChangesStreamOptions {
                since: Seq::Num(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "c");
    }

    #[tokio::test]
    async fn one_shot_with_limit() {
        let (db, _sender) = setup().await;
        for i in 0..5 {
            put_doc(db.as_ref(), &format!("d{}", i), serde_json::json!({})).await;
        }

        let events = get_changes(
            db.as_ref(),
            ChangesStreamOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn live_stream_initial_then_new() {
        let (db, sender) = setup().await;
        put_doc(db.as_ref(), "existing", serde_json::json!({})).await;

        let receiver = sender.subscribe();
        let db_clone = db.clone();

        let mut stream = LiveChangesStream::new(
            db.clone(),
            Some(receiver),
            ChangesStreamOptions {
                live: true,
                limit: Some(3),
                ..Default::default()
            },
        );

        // First event should be the existing doc
        let event = stream.next_change().await.unwrap();
        assert_eq!(event.id, "existing");

        // Now add more docs in the background
        let sender_clone = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            put_doc(db_clone.as_ref(), "new1", serde_json::json!({})).await;
            sender_clone.notify(Seq::Num(2), "new1".into());
            tokio::time::sleep(Duration::from_millis(50)).await;
            put_doc(db_clone.as_ref(), "new2", serde_json::json!({})).await;
            sender_clone.notify(Seq::Num(3), "new2".into());
        });

        let event = stream.next_change().await.unwrap();
        assert_eq!(event.id, "new1");

        let event = stream.next_change().await.unwrap();
        assert_eq!(event.id, "new2");

        // Limit reached (3)
        assert!(stream.next_change().await.is_none());
    }

    #[tokio::test]
    async fn live_changes_via_channel() {
        let db = Arc::new(MemoryAdapter::new("test"));
        put_doc(db.as_ref(), "a", serde_json::json!({"v": 1})).await;

        let (mut rx, handle) = live_changes(
            db.clone(),
            ChangesStreamOptions {
                live: true,
                poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );

        // Should receive the existing doc
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "a");

        // Add a new doc — should be picked up by polling
        put_doc(db.as_ref(), "b", serde_json::json!({"v": 2})).await;

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, "b");

        handle.cancel();
    }

    #[tokio::test]
    async fn limit_counts_post_filter_events() {
        // 5 docs; a filter that only accepts even-indexed ids; limit 2.
        // The limit must yield 2 MATCHING events, not stop after scanning 2.
        let (db, _sender) = setup().await;
        for i in 0..6 {
            put_doc(db.as_ref(), &format!("d{}", i), serde_json::json!({"i": i})).await;
        }

        let filter: ChangesFilter = Arc::new(|e: &ChangeEvent| {
            // accept d0, d2, d4 (ids ending in an even digit)
            e.id.trim_start_matches('d')
                .parse::<u64>()
                .map(|n| n % 2 == 0)
                .unwrap_or(false)
        });

        let events = get_changes(
            db.as_ref(),
            ChangesStreamOptions {
                limit: Some(2),
                filter: Some(filter),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "d0");
        assert_eq!(events[1].id, "d2");
    }

    #[tokio::test]
    async fn change_sender_subscribe() {
        let (sender, _rx) = ChangeSender::new(16);
        let mut sub = sender.subscribe();

        sender.notify(Seq::Num(1), "doc1".into());

        let notification = sub.recv().await.unwrap();
        assert_eq!(notification.seq, Seq::Num(1));
        assert_eq!(notification.doc_id, "doc1");
    }

    // -----------------------------------------------------------------------
    // Live-feed lifecycle (virtual time: sleeps auto-advance when idle)
    // -----------------------------------------------------------------------

    use rouchdb_core::document::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A memory adapter whose `changes()` can be made to fail, and which
    /// counts `changes()` calls.
    struct Flaky {
        inner: MemoryAdapter,
        fail: AtomicBool,
        calls: AtomicUsize,
        /// How long each `changes()` call takes, in milliseconds.
        latency_ms: std::sync::atomic::AtomicU64,
    }

    impl Flaky {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                inner: MemoryAdapter::new("flaky"),
                fail: AtomicBool::new(fail),
                calls: AtomicUsize::new(0),
                latency_ms: std::sync::atomic::AtomicU64::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl Adapter for Flaky {
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            let latency = self.latency_ms.load(Ordering::SeqCst);
            if latency > 0 {
                tokio::time::sleep(Duration::from_millis(latency)).await;
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(rouchdb_core::error::RouchError::DatabaseError(
                    "connection refused".into(),
                ));
            }
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

    /// Next event that is not a heartbeat, within `secs` of virtual time.
    async fn next_non_heartbeat(
        rx: &mut mpsc::Receiver<ChangesEvent>,
        secs: u64,
    ) -> Option<ChangesEvent> {
        tokio::time::timeout(Duration::from_secs(secs), async {
            loop {
                match rx.recv().await {
                    Some(ChangesEvent::Heartbeat) => continue,
                    other => return other,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_shorter_than_poll_still_delivers_changes() {
        let db = Arc::new(MemoryAdapter::new("test"));
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                heartbeat: Some(Duration::from_millis(100)),
                poll_interval: Duration::from_millis(500),
                ..Default::default()
            },
        );
        // Written once the feed is already waiting between polls.
        tokio::time::sleep(Duration::from_secs(1)).await;
        put_doc(db.as_ref(), "a", serde_json::json!({})).await;

        let delivered = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, ChangesEvent::Change(ref c) if c.id == "a") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(delivered, "only heartbeats arrived");
    }

    #[tokio::test(start_paused = true)]
    async fn events_signal_paused_and_active() {
        let db = Arc::new(MemoryAdapter::new("test"));
        put_doc(db.as_ref(), "a", serde_json::json!({})).await;
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );

        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(next_non_heartbeat(&mut rx, 5).await);
        }
        put_doc(db.as_ref(), "b", serde_json::json!({})).await;
        for _ in 0..2 {
            seen.push(next_non_heartbeat(&mut rx, 5).await);
        }
        let names: Vec<String> = seen
            .iter()
            .map(|e| match e {
                Some(ChangesEvent::Change(c)) => format!("change:{}", c.id),
                Some(ChangesEvent::Paused) => "paused".into(),
                Some(ChangesEvent::Active) => "active".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["change:a", "paused", "active", "change:b"]);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_errors_are_reported_and_retried() {
        let db = Flaky::new(true);
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );

        let first = next_non_heartbeat(&mut rx, 5).await;
        assert!(matches!(first, Some(ChangesEvent::Error(_))), "{first:?}");

        // Once the server is back, the same feed resumes.
        db.fail.store(false, Ordering::SeqCst);
        put_doc(&db.inner, "a", serde_json::json!({})).await;
        let resumed = tokio::time::timeout(Duration::from_secs(300), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, ChangesEvent::Change(ref c) if c.id == "a") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(resumed);
    }

    #[tokio::test(start_paused = true)]
    async fn live_changes_survives_fetch_errors() {
        let db = Flaky::new(true);
        let (mut rx, _handle) = live_changes(
            db.clone(),
            ChangesStreamOptions {
                poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        db.fail.store(false, Ordering::SeqCst);
        put_doc(&db.inner, "a", serde_json::json!({})).await;

        let event = tokio::time::timeout(Duration::from_secs(300), rx.recv())
            .await
            .expect("feed stalled");
        assert_eq!(event.expect("feed ended on a fetch error").id, "a");
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_ends_an_idle_feed() {
        let db = Arc::new(MemoryAdapter::new("test"));
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                timeout: Some(Duration::from_secs(2)),
                poll_interval: Duration::from_millis(500),
                ..Default::default()
            },
        );
        let ended = tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, ChangesEvent::Complete { .. }) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(ended, "idle feed never timed out");
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_shorter_than_poll_keeps_late_changes() {
        let db = Arc::new(MemoryAdapter::new("test"));
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                timeout: Some(Duration::from_millis(100)),
                poll_interval: Duration::from_millis(500),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        put_doc(db.as_ref(), "late", serde_json::json!({})).await;

        let mut ids = Vec::new();
        while let Some(event) = next_non_heartbeat(&mut rx, 10).await {
            match event {
                ChangesEvent::Change(c) => ids.push(c.id),
                ChangesEvent::Complete { .. } => break,
                _ => {}
            }
        }
        assert_eq!(ids, vec!["late"]);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_reports_scanned_last_seq() {
        let db = Arc::new(MemoryAdapter::new("test"));
        for i in 0..3 {
            put_doc(db.as_ref(), &format!("d{i}"), serde_json::json!({})).await;
        }
        let (mut rx, _handle) = live_changes_events(
            db.clone(),
            ChangesStreamOptions {
                doc_ids: Some(vec!["nope".into()]),
                timeout: Some(Duration::from_millis(100)),
                poll_interval: Duration::from_millis(500),
                ..Default::default()
            },
        );
        let last_seq = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                if let ChangesEvent::Complete { last_seq } = event {
                    return Some(last_seq);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(last_seq, Some(Seq::Num(3)));
    }

    #[tokio::test]
    async fn filtered_limited_stream_delivers_the_limit() {
        let (db, _sender) = setup().await;
        for i in 0..6 {
            put_doc(db.as_ref(), &format!("d{i}"), serde_json::json!({"i": i})).await;
        }
        let even: ChangesFilter = Arc::new(|e: &ChangeEvent| {
            e.id.trim_start_matches('d')
                .parse::<u64>()
                .is_ok_and(|n| n % 2 == 0)
        });
        let mut stream = LiveChangesStream::new(
            db,
            None,
            ChangesStreamOptions {
                limit: Some(2),
                filter: Some(even),
                ..Default::default()
            },
        );
        let mut ids = Vec::new();
        while let Some(event) = stream.next_change().await {
            ids.push(event.id);
        }
        assert_eq!(ids, vec!["d0", "d2"]);
    }

    #[tokio::test]
    async fn free_functions_apply_the_selector() {
        let db = Arc::new(MemoryAdapter::new("test"));
        put_doc(db.as_ref(), "alice", serde_json::json!({"type": "user"})).await;
        put_doc(db.as_ref(), "inv1", serde_json::json!({"type": "invoice"})).await;
        put_doc(db.as_ref(), "bob", serde_json::json!({"type": "user"})).await;
        let opts = || ChangesStreamOptions {
            selector: Some(serde_json::json!({"type": "user"})),
            poll_interval: Duration::from_millis(20),
            ..Default::default()
        };

        let one_shot = get_changes(db.as_ref(), opts()).await.unwrap();
        let ids: Vec<&str> = one_shot.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["alice", "bob"]);
        assert!(one_shot.iter().all(|e| e.doc.is_none()));

        let (mut rx, handle) = live_changes(db.clone(), opts());
        let mut live = Vec::new();
        for _ in 0..2 {
            let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(e.doc.is_none());
            live.push(e.id);
        }
        put_doc(db.as_ref(), "inv2", serde_json::json!({"type": "invoice"})).await;
        put_doc(db.as_ref(), "carol", serde_json::json!({"type": "user"})).await;
        let e = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        live.push(e.id);
        handle.cancel();
        assert_eq!(live, vec!["alice", "bob", "carol"]);
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_receiver_stops_polling() {
        let db = Flaky::new(false);
        let (rx, _handle) = live_changes(
            db.clone(),
            ChangesStreamOptions {
                poll_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(rx);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let calls = db.calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(db.calls.load(Ordering::SeqCst), calls, "still polling");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_interrupts_a_blocked_send() {
        let db = Arc::new(MemoryAdapter::new("test"));
        for i in 0..100 {
            put_doc(db.as_ref(), &format!("d{i:03}"), serde_json::json!({})).await;
        }
        for events in [false, true] {
            // Nobody reads: the task blocks once the channel is full.
            let handle = if events {
                let (rx, handle) = live_changes_events(db.clone(), Default::default());
                tokio::time::sleep(Duration::from_secs(1)).await;
                handle.cancel();
                tokio::time::sleep(Duration::from_secs(1)).await;
                assert!(rx.is_closed(), "events task still blocked after cancel");
                handle
            } else {
                let (rx, handle) = live_changes(db.clone(), Default::default());
                tokio::time::sleep(Duration::from_secs(1)).await;
                handle.cancel();
                tokio::time::sleep(Duration::from_secs(1)).await;
                assert!(rx.is_closed(), "task still blocked after cancel");
                handle
            };
            drop(handle);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn next_event_survives_being_dropped_mid_fetch() {
        let db = Flaky::new(false);
        db.latency_ms.store(50, Ordering::SeqCst);
        let mut stream = LiveChangesStream::new(
            db.clone(),
            None,
            ChangesStreamOptions {
                live: true,
                poll_interval: Duration::from_millis(500),
                ..Default::default()
            },
        );
        let mut ticks = tokio::time::interval(Duration::from_millis(20));
        let mut written = false;

        // A caller racing next_event() against its own timer drops the
        // future, sometimes in the middle of a fetch.
        let delivered = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    event = stream.next_event() => match event {
                        Some(ChangesEvent::Change(c)) if c.id == "late" => return true,
                        Some(ChangesEvent::Paused) if !written => {
                            written = true;
                            put_doc(&db.inner, "late", serde_json::json!({})).await;
                        }
                        None => return false,
                        _ => {}
                    },
                    _ = ticks.tick() => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(delivered, "change never delivered");
    }
}
