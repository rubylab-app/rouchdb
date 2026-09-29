//! Tests for changes feed parity features:
//! - ChangesFilter (custom filter closures)
//! - ChangesEvent lifecycle (Active, Paused, Complete, Error)
//! - live_changes_events()
//! - Changes with conflicts/style options
//! - Timeout support

use std::sync::Arc;
use std::time::Duration;

use rouchdb::{ChangesEvent, ChangesFilter, ChangesOptions, ChangesStreamOptions, Database};

/// Ids of the `Change` events a live stream delivers before its first
/// `Paused`, i.e. everything it had to report about the existing docs.
async fn ids_until_paused(rx: &mut tokio::sync::mpsc::Receiver<ChangesEvent>) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut ids = Vec::new();
        loop {
            match rx.recv().await {
                Some(ChangesEvent::Change(ce)) => ids.push(ce.id),
                Some(ChangesEvent::Paused) => return ids,
                Some(_) => {}
                None => panic!("stream ended before Paused (after {ids:?})"),
            }
        }
    })
    .await
    .expect("the live stream never went idle")
}

// =========================================================================
// ChangesFilter — custom filter closures
// =========================================================================

#[tokio::test]
async fn live_changes_with_filter_closure() {
    let db = Database::memory("test");
    db.put(
        "user:1",
        serde_json::json!({"type": "user", "name": "Alice"}),
    )
    .await
    .unwrap();
    db.put("order:1", serde_json::json!({"type": "order", "total": 50}))
        .await
        .unwrap();
    db.put("user:2", serde_json::json!({"type": "user", "name": "Bob"}))
        .await
        .unwrap();

    let filter: ChangesFilter = Arc::new(|event| event.id.starts_with("user:"));

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        filter: Some(filter),
        include_docs: false,
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // Should only get user docs, not orders
    let e1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(e1.id.starts_with("user:"));

    let e2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(e2.id.starts_with("user:"));

    // Third event should be timeout (no more user docs)
    let timeout_result = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(timeout_result.is_err()); // Should timeout, no more matching events

    handle.cancel();
}

#[tokio::test]
async fn live_changes_filter_allows_all() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();

    let filter: ChangesFilter = Arc::new(|_| true); // accept all

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        filter: Some(filter),
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    let e1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let e2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();

    let ids: Vec<String> = vec![e1.id, e2.id];
    assert!(ids.contains(&"a".to_string()));
    assert!(ids.contains(&"b".to_string()));

    handle.cancel();
}

#[tokio::test]
async fn live_changes_filter_rejects_all() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();

    let filter: ChangesFilter = Arc::new(|_| false); // reject all

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        filter: Some(filter),
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    assert_eq!(ids_until_paused(&mut rx).await, Vec::<String>::new());

    handle.cancel();
}

// =========================================================================
// live_changes_events() — lifecycle events
// =========================================================================

#[tokio::test]
async fn live_changes_events_emits_change_events() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        include_docs: true,
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    let mut got_change = false;
    let timeout = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ChangesEvent::Change(ce)) => {
                        assert_eq!(ce.id, "doc1");
                        got_change = true;
                        break;
                    }
                    Some(_) => continue, // lifecycle events
                    None => break,
                }
            }
            _ = &mut timeout => break,
        }
    }

    assert!(got_change, "Should have received a Change event");
    handle.cancel();
}

#[tokio::test]
async fn live_changes_events_with_filter() {
    let db = Database::memory("test");
    db.put("keep", serde_json::json!({"important": true}))
        .await
        .unwrap();
    db.put("skip", serde_json::json!({"important": false}))
        .await
        .unwrap();

    let filter: ChangesFilter = Arc::new(|event| event.id == "keep");

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        filter: Some(filter),
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // Every existing doc is reported before the first Paused: `skip`, written
    // after `keep`, must not be among them.
    assert_eq!(ids_until_paused(&mut rx).await, vec!["keep"]);
    handle.cancel();
}

#[tokio::test]
async fn live_changes_events_with_selector() {
    let db = Database::memory("test");
    db.put(
        "alice",
        serde_json::json!({"type": "user", "name": "Alice"}),
    )
    .await
    .unwrap();
    db.put("inv1", serde_json::json!({"type": "invoice", "amount": 99}))
        .await
        .unwrap();
    db.put("bob", serde_json::json!({"type": "user", "name": "Bob"}))
        .await
        .unwrap();

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        selector: Some(serde_json::json!({"type": "user"})),
        include_docs: true,
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    assert_eq!(ids_until_paused(&mut rx).await, vec!["alice", "bob"]);
    handle.cancel();
}

// =========================================================================
// Changes handle cancellation
// =========================================================================

#[tokio::test]
async fn live_changes_handle_cancel() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({})).await.unwrap();

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // Receive the change for the existing doc.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for the first change")
        .expect("channel closed before the first change");
    assert_eq!(first.id, "doc1");

    handle.cancel();

    // After cancel the background task must stop and close the channel. No
    // other document was written, so nothing else may be delivered.
    let extra = tokio::time::timeout(Duration::from_secs(2), async {
        let mut extra = Vec::new();
        while let Some(event) = rx.recv().await {
            extra.push(event.id);
        }
        extra
    })
    .await
    .expect("channel should close after cancel");
    assert!(
        extra.is_empty(),
        "unexpected events after cancel: {extra:?}"
    );
}

// =========================================================================
// Changes with doc_ids filter
// =========================================================================

#[tokio::test]
async fn changes_with_doc_ids_filter() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"v": 1})).await.unwrap();
    db.put("b", serde_json::json!({"v": 2})).await.unwrap();
    db.put("c", serde_json::json!({"v": 3})).await.unwrap();

    let changes = db
        .changes(ChangesOptions {
            doc_ids: Some(vec!["a".into(), "c".into()]),
            ..Default::default()
        })
        .await
        .unwrap();

    let ids: Vec<&str> = changes.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "c"]);
}

// =========================================================================
// Changes with limit and since
// =========================================================================

#[tokio::test]
async fn changes_with_limit() {
    let db = Database::memory("test");
    for i in 0..10 {
        db.put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let changes = db
        .changes(ChangesOptions {
            limit: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 3);
}

#[tokio::test]
async fn changes_since_sequence() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({})).await.unwrap();
    db.put("doc2", serde_json::json!({})).await.unwrap();
    db.put("doc3", serde_json::json!({})).await.unwrap();

    let all = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(all.results.len(), 3);

    // Changes after the second event: exactly the third one.
    let since = all.results[1].seq.clone();
    let partial = db
        .changes(ChangesOptions {
            since,
            ..Default::default()
        })
        .await
        .unwrap();

    let entries = |r: &[rouchdb::ChangeEvent]| -> Vec<(String, rouchdb::Seq)> {
        r.iter().map(|c| (c.id.clone(), c.seq.clone())).collect()
    };
    assert_eq!(entries(&partial.results), entries(&all.results[2..]));
    assert_eq!(partial.results[0].id, "doc3");
}

// =========================================================================
// Changes showing deleted docs
// =========================================================================

#[tokio::test]
async fn changes_shows_deleted_docs() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    assert!(db.remove("doc1", &r1.rev.unwrap()).await.unwrap().ok);

    let changes = db.changes(ChangesOptions::default()).await.unwrap();
    let deleted = changes.results.iter().find(|r| r.id == "doc1").unwrap();
    assert!(deleted.deleted);
}
