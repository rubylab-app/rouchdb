/// Streaming changes feed for RouchDB.
///
/// Provides a `ChangesStream` that wraps the adapter's `changes()` method
/// and supports:
/// - One-shot mode: fetch changes since a sequence and return
/// - Live/continuous mode: keep polling for new changes
/// - Filtering by document IDs
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::{ChangeEvent, ChangesOptions, ChangesStyle, Seq};

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
    /// How long to keep the connection open before closing in live mode.
    pub timeout: Option<Duration>,
    /// Interval for heartbeat signals in live mode (prevents connection timeout).
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

/// Fetch changes from an adapter in one-shot mode.
pub async fn get_changes(
    adapter: &dyn Adapter,
    opts: ChangesStreamOptions,
) -> Result<Vec<ChangeEvent>> {
    let filter = opts.filter.clone();
    let changes_opts = ChangesOptions {
        since: opts.since,
        limit: opts.limit,
        descending: false,
        include_docs: opts.include_docs,
        live: false,
        doc_ids: opts.doc_ids,
        conflicts: opts.conflicts,
        style: opts.style,
        ..Default::default()
    };

    let response = adapter.changes(changes_opts).await?;
    let results = if let Some(f) = filter {
        response.results.into_iter().filter(|e| f(e)).collect()
    } else {
        response.results
    };
    Ok(results)
}

/// A live changes stream that yields change events as they happen.
///
/// In live mode, after fetching existing changes, it waits for
/// notifications via a broadcast channel or polls at regular intervals.
pub struct LiveChangesStream {
    adapter: Arc<dyn Adapter>,
    receiver: Option<ChangeReceiver>,
    opts: ChangesStreamOptions,
    last_seq: Seq,
    buffer: Vec<ChangeEvent>,
    buffer_idx: usize,
    state: LiveStreamState,
    count: u64,
}

enum LiveStreamState {
    /// Fetching the initial batch of changes.
    FetchingInitial,
    /// Yielding buffered results.
    Yielding,
    /// Waiting for new notifications.
    Waiting,
    /// Done (limit reached or adapter closed).
    Done,
}

impl LiveChangesStream {
    pub fn new(
        adapter: Arc<dyn Adapter>,
        receiver: Option<ChangeReceiver>,
        opts: ChangesStreamOptions,
    ) -> Self {
        let last_seq = opts.since.clone();
        Self {
            adapter,
            receiver,
            opts,
            last_seq,
            buffer: Vec::new(),
            buffer_idx: 0,
            state: LiveStreamState::FetchingInitial,
            count: 0,
        }
    }

    /// Fetch changes since `last_seq` and buffer them.
    async fn fetch_changes(&mut self) -> Result<()> {
        let changes_opts = ChangesOptions {
            since: self.last_seq.clone(),
            limit: self.opts.limit.map(|l| l.saturating_sub(self.count)),
            descending: false,
            include_docs: self.opts.include_docs,
            live: false,
            doc_ids: self.opts.doc_ids.clone(),
            conflicts: self.opts.conflicts,
            style: self.opts.style.clone(),
            ..Default::default()
        };

        let response = self.adapter.changes(changes_opts).await?;
        if !response.results.is_empty() {
            self.last_seq = response.last_seq;
        }
        self.buffer = response.results;
        self.buffer_idx = 0;
        Ok(())
    }

    /// Get the next change event, blocking if in live mode.
    pub async fn next_change(&mut self) -> Option<ChangeEvent> {
        loop {
            // Check limit
            if let Some(limit) = self.opts.limit
                && self.count >= limit
            {
                return None;
            }

            match self.state {
                LiveStreamState::FetchingInitial => {
                    if self.fetch_changes().await.is_err() {
                        return None;
                    }
                    self.state = if self.buffer.is_empty() {
                        if self.opts.live {
                            LiveStreamState::Waiting
                        } else {
                            LiveStreamState::Done
                        }
                    } else {
                        LiveStreamState::Yielding
                    };
                }
                LiveStreamState::Yielding => {
                    if self.buffer_idx < self.buffer.len() {
                        let event = self.buffer[self.buffer_idx].clone();
                        self.buffer_idx += 1;
                        self.count += 1;
                        return Some(event);
                    }
                    // Buffer exhausted
                    self.state = if self.opts.live {
                        LiveStreamState::Waiting
                    } else {
                        LiveStreamState::Done
                    };
                }
                LiveStreamState::Waiting => {
                    // Wait for a notification or poll, with optional timeout
                    let wait_result = if let Some(ref mut receiver) = self.receiver {
                        if let Some(timeout_dur) = self.opts.timeout {
                            match tokio::time::timeout(timeout_dur, receiver.recv()).await {
                                Ok(Some(_)) => true,
                                Ok(None) => return None, // Channel closed
                                Err(_) => return None,   // Timeout elapsed
                            }
                        } else {
                            receiver.recv().await.as_ref().is_some()
                        }
                    } else {
                        // No broadcast channel, poll with interval
                        if let Some(timeout_dur) = self.opts.timeout {
                            match tokio::time::timeout(
                                timeout_dur,
                                tokio::time::sleep(self.opts.poll_interval),
                            )
                            .await
                            {
                                Ok(()) => true,
                                Err(_) => return None, // Timeout elapsed
                            }
                        } else {
                            tokio::time::sleep(self.opts.poll_interval).await;
                            true
                        }
                    };

                    if !wait_result {
                        return None;
                    }

                    // Fetch new changes
                    if self.fetch_changes().await.is_err() {
                        return None;
                    }
                    if !self.buffer.is_empty() {
                        self.state = LiveStreamState::Yielding;
                    }
                    // If still empty, stay in Waiting state
                }
                LiveStreamState::Done => {
                    return None;
                }
            }
        }
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

/// Start a live changes stream that sends events through an mpsc channel.
///
/// Spawns a background task that polls the adapter for changes and sends
/// each `ChangeEvent` through the returned receiver. The `ChangesHandle`
/// controls the stream's lifecycle.
pub fn live_changes(
    adapter: Arc<dyn Adapter>,
    opts: ChangesStreamOptions,
) -> (mpsc::Receiver<ChangeEvent>, ChangesHandle) {
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let filter = opts.filter.clone();

    tokio::spawn(async move {
        let mut stream =
            LiveChangesStream::new(adapter, None, ChangesStreamOptions { live: true, ..opts });

        loop {
            tokio::select! {
                change = stream.next_change() => {
                    match change {
                        Some(event) => {
                            // Apply filter if set
                            if let Some(ref f) = filter
                                && !f(&event)
                            {
                                continue;
                            }
                            if tx.send(event).await.is_err() {
                                break; // Receiver dropped
                            }
                        }
                        None => break, // Stream ended (limit reached)
                    }
                }
                _ = cancel_clone.cancelled() => break,
            }
        }
    });

    (rx, ChangesHandle { cancel })
}

/// Start a live changes stream that emits lifecycle events.
///
/// Like `live_changes()` but wraps each event in a `ChangesEvent` enum
/// that includes `Active`, `Paused`, `Complete`, and `Error` lifecycle events
/// alongside the actual `Change` events.
pub fn live_changes_events(
    adapter: Arc<dyn Adapter>,
    opts: ChangesStreamOptions,
) -> (mpsc::Receiver<ChangesEvent>, ChangesHandle) {
    let (tx, rx) = mpsc::channel(64);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let filter = opts.filter.clone();

    tokio::spawn(async move {
        let mut stream =
            LiveChangesStream::new(adapter, None, ChangesStreamOptions { live: true, ..opts });

        let mut was_paused = false;

        loop {
            tokio::select! {
                change = stream.next_change() => {
                    match change {
                        Some(event) => {
                            // Emit Active if we were paused
                            if was_paused {
                                was_paused = false;
                                let _ = tx.send(ChangesEvent::Active).await;
                            }

                            // Apply filter if set
                            if let Some(ref f) = filter
                                && !f(&event)
                            {
                                continue;
                            }

                            if tx.send(ChangesEvent::Change(event)).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            // Stream ended
                            let _ = tx.send(ChangesEvent::Complete {
                                last_seq: stream.last_seq.clone(),
                            }).await;
                            break;
                        }
                    }
                }
                _ = cancel_clone.cancelled() => {
                    let _ = tx.send(ChangesEvent::Complete {
                        last_seq: stream.last_seq.clone(),
                    }).await;
                    break;
                },
            }

            // If the buffer is exhausted and we're in waiting state, emit Paused
            if stream.buffer_idx >= stream.buffer.len()
                && matches!(stream.state, LiveStreamState::Waiting)
                && !was_paused
            {
                was_paused = true;
                let _ = tx.send(ChangesEvent::Paused).await;
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
    async fn change_sender_subscribe() {
        let (sender, _rx) = ChangeSender::new(16);
        let mut sub = sender.subscribe();

        sender.notify(Seq::Num(1), "doc1".into());

        let notification = sub.recv().await.unwrap();
        assert_eq!(notification.seq, Seq::Num(1));
        assert_eq!(notification.doc_id, "doc1");
    }
}
