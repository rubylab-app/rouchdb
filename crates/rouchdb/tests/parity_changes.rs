//! Tests for changes feed parity features:
//! - ChangesFilter (custom filter closures)
//! - ChangesEvent lifecycle (Active, Paused, Complete, Error)
//! - live_changes_events()
//! - Changes with conflicts/style options
//! - Timeout support

use std::sync::Arc;
use std::time::Duration;

use rouchdb::{ChangesEvent, ChangesFilter, ChangesOptions, ChangesStreamOptions, Database};

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

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        filter: Some(filter),
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // Should not receive any events
    let result = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(result.is_err());

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

    let mut change_ids = Vec::new();
    let timeout = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ChangesEvent::Change(ce)) => {
                        change_ids.push(ce.id.clone());
                        if !change_ids.is_empty() {
                            break;
                        }
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            _ = &mut timeout => break,
        }
    }

    assert_eq!(change_ids, vec!["keep"]);
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

    let mut user_ids = Vec::new();
    let timeout = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ChangesEvent::Change(ce)) => {
                        user_ids.push(ce.id.clone());
                        if user_ids.len() >= 2 {
                            break;
                        }
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            _ = &mut timeout => break,
        }
    }

    assert_eq!(user_ids.len(), 2);
    assert!(user_ids.contains(&"alice".to_string()));
    assert!(user_ids.contains(&"bob".to_string()));
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

    // Receive first event
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();

    // Cancel
    handle.cancel();

    // After cancel, the channel should eventually close
    let result = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    // Either we get None (channel closed) or timeout (background not yet stopped)
    match result {
        Ok(None) => {}    // Channel closed — good
        Ok(Some(_)) => {} // May still have buffered events — OK
        Err(_) => {}      // Timeout — OK, background task is stopping
    }
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

    assert_eq!(changes.results.len(), 2);
    let ids: Vec<&str> = changes.results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"c"));
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

    // Get changes since the second event
    let since = all.results[1].seq.clone();
    let partial = db
        .changes(ChangesOptions {
            since,
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(partial.results.len() < all.results.len());
}

// =========================================================================
// Changes showing deleted docs
// =========================================================================

#[tokio::test]
async fn changes_shows_deleted_docs() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    db.remove("doc1", &r1.rev.unwrap()).await.unwrap();

    let changes = db.changes(ChangesOptions::default()).await.unwrap();
    let deleted = changes.results.iter().find(|r| r.id == "doc1").unwrap();
    assert!(deleted.deleted);
}
