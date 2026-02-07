/// Streaming changes feed for RouchDB.
///
/// Provides a `ChangesStream` that wraps the adapter's `changes()` method
/// and supports:
/// - One-shot mode: fetch changes since a sequence and return
/// - Live/continuous mode: keep polling for new changes
/// - Filtering by document IDs

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use rouchdb_core::adapter::Adapter;
use rouchdb_core::document::{ChangeEvent, ChangesOptions};
use rouchdb_core::error::Result;

/// A notification that a change occurred, sent through the broadcast channel.
#[derive(Debug, Clone)]
pub struct ChangeNotification {
    pub seq: u64,
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

    pub fn notify(&self, seq: u64, doc_id: String) {
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
#[derive(Debug, Clone)]
pub struct ChangesStreamOptions {
    pub since: u64,
    pub live: bool,
    pub include_docs: bool,
    pub doc_ids: Option<Vec<String>>,
    pub limit: Option<u64>,
    /// Polling interval for live mode when no broadcast channel is available.
    pub poll_interval: Duration,
}

impl Default for ChangesStreamOptions {
    fn default() -> Self {
        Self {
            since: 0,
            live: false,
            include_docs: false,
            doc_ids: None,
            limit: None,
            poll_interval: Duration::from_millis(500),
        }
    }
}

/// Fetch changes from an adapter in one-shot mode.
pub async fn get_changes(
    adapter: &dyn Adapter,
    opts: ChangesStreamOptions,
) -> Result<Vec<ChangeEvent>> {
    let changes_opts = ChangesOptions {
        since: opts.since,
        limit: opts.limit,
        descending: false,
        include_docs: opts.include_docs,
        live: false,
        doc_ids: opts.doc_ids,
    };

    let response = adapter.changes(changes_opts).await?;
    Ok(response.results)
}

/// A live changes stream that yields change events as they happen.
///
/// In live mode, after fetching existing changes, it waits for
/// notifications via a broadcast channel or polls at regular intervals.
pub struct LiveChangesStream {
    adapter: Arc<dyn Adapter>,
    receiver: Option<ChangeReceiver>,
    opts: ChangesStreamOptions,
    last_seq: u64,
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
        let last_seq = opts.since;
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
            since: self.last_seq,
            limit: self.opts.limit.map(|l| l.saturating_sub(self.count)),
            descending: false,
            include_docs: self.opts.include_docs,
            live: false,
            doc_ids: self.opts.doc_ids.clone(),
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
            if let Some(limit) = self.opts.limit {
                if self.count >= limit {
                    return None;
                }
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
                    // Wait for a notification or poll
                    if let Some(ref mut receiver) = self.receiver {
                        // Wait for broadcast notification
                        if receiver.recv().await.is_none() {
                            return None; // Channel closed
                        }
                    } else {
                        // No broadcast channel, poll with interval
                        tokio::time::sleep(self.opts.poll_interval).await;
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

        let events = get_changes(
            db.as_ref(),
            ChangesStreamOptions::default(),
        )
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
                since: 2,
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
            sender_clone.notify(2, "new1".into());
            tokio::time::sleep(Duration::from_millis(50)).await;
            put_doc(db_clone.as_ref(), "new2", serde_json::json!({})).await;
            sender_clone.notify(3, "new2".into());
        });

        let event = stream.next_change().await.unwrap();
        assert_eq!(event.id, "new1");

        let event = stream.next_change().await.unwrap();
        assert_eq!(event.id, "new2");

        // Limit reached (3)
        assert!(stream.next_change().await.is_none());
    }

    #[tokio::test]
    async fn change_sender_subscribe() {
        let (sender, _rx) = ChangeSender::new(16);
        let mut sub = sender.subscribe();

        sender.notify(1, "doc1".into());

        let notification = sub.recv().await.unwrap();
        assert_eq!(notification.seq, 1);
        assert_eq!(notification.doc_id, "doc1");
    }
}
