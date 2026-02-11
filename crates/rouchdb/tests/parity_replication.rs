//! Tests for replication parity features:
//! - ReplicationOptions::since (override starting point)
//! - ReplicationOptions::checkpoint (disable checkpointing)
//! - Replication with events
//! - Live replication
//! - Bidirectional sync

use std::time::Duration;

use rouchdb::{Database, ReplicationEvent, ReplicationOptions};

// =========================================================================
// ReplicationOptions::since — override starting point
// =========================================================================

#[tokio::test]
async fn replication_with_since_override() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    source
        .put("doc3", serde_json::json!({"v": 3}))
        .await
        .unwrap();

    // Get the changes to find a sequence number
    let changes = source
        .changes(rouchdb::ChangesOptions::default())
        .await
        .unwrap();
    assert_eq!(changes.results.len(), 3);

    // Use since to skip the first event
    let since_seq = changes.results[0].seq.clone();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                since: Some(since_seq),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    // Should have replicated only docs after since_seq
    let target_info = target.info().await.unwrap();
    assert!(target_info.doc_count < 3, "since should skip some docs");
}

// =========================================================================
// ReplicationOptions::checkpoint = false
// =========================================================================

#[tokio::test]
async fn replication_without_checkpoint() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                checkpoint: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 1);

    // With checkpoint=false, replicating again should re-send all docs
    // (since no checkpoint was saved)
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let result2 = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                checkpoint: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result2.ok);
    // Without checkpoint, it processes all changes from the start
    // (though revs_diff will skip already-present docs)
}

// =========================================================================
// Replication with checkpoint (default behavior)
// =========================================================================

#[tokio::test]
async fn replication_incremental_with_checkpoint() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    // First replication
    let r1 = source.replicate_to(&target).await.unwrap();
    assert!(r1.ok);
    assert_eq!(r1.docs_written, 1);

    // Add another doc
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    // Second replication should only transfer the new doc
    let r2 = source.replicate_to(&target).await.unwrap();
    assert!(r2.ok);
    assert_eq!(r2.docs_written, 1);

    let target_info = target.info().await.unwrap();
    assert_eq!(target_info.doc_count, 2);
}

// =========================================================================
// Replication with events
// =========================================================================

#[tokio::test]
async fn replication_with_events_emits_lifecycle() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let (result, mut rx) = source
        .replicate_to_with_events(&target, ReplicationOptions::default())
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    assert!(
        events.iter().any(|e| matches!(e, ReplicationEvent::Active)),
        "Should emit Active event"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReplicationEvent::Complete(_))),
        "Should emit Complete event"
    );
}

#[tokio::test]
async fn replication_events_include_change_with_docs_count() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    for i in 0..5 {
        source
            .put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let (result, mut rx) = source
        .replicate_to_with_events(&target, ReplicationOptions::default())
        .await
        .unwrap();

    assert!(result.ok);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let change_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ReplicationEvent::Change { .. }))
        .collect();
    assert!(
        !change_events.is_empty(),
        "Should emit at least one Change event"
    );
}

// =========================================================================
// Live replication
// =========================================================================

#[tokio::test]
async fn live_replication_picks_up_new_docs() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    let (mut rx, handle) = source.replicate_to_live(
        &target,
        ReplicationOptions {
            poll_interval: Duration::from_millis(50),
            live: true,
            ..Default::default()
        },
    );

    // Wait for initial replication
    let mut initial_done = false;
    let timeout = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ReplicationEvent::Complete(r)) if r.docs_written > 0 => {
                        initial_done = true;
                        break;
                    }
                    Some(ReplicationEvent::Paused) => {
                        if target.get("doc1").await.is_ok() {
                            initial_done = true;
                            break;
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = &mut timeout => break,
        }
    }

    assert!(initial_done || target.get("doc1").await.is_ok());
    handle.cancel();
}

// =========================================================================
// Bidirectional sync
// =========================================================================

#[tokio::test]
async fn sync_merges_both_directions() {
    let local = Database::memory("local");
    let remote = Database::memory("remote");

    local
        .put("local_doc", serde_json::json!({"from": "local"}))
        .await
        .unwrap();
    remote
        .put("remote_doc", serde_json::json!({"from": "remote"}))
        .await
        .unwrap();

    let (push, pull) = local.sync(&remote).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    // Both should have both docs
    assert!(local.get("remote_doc").await.is_ok());
    assert!(remote.get("local_doc").await.is_ok());
}

#[tokio::test]
async fn sync_with_no_changes() {
    let local = Database::memory("local");
    let remote = Database::memory("remote");

    // No docs — sync should still succeed
    let (push, pull) = local.sync(&remote).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);
}

// =========================================================================
// Replication with batch_size
// =========================================================================

#[tokio::test]
async fn replication_with_small_batch_size() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    for i in 0..10 {
        source
            .put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                batch_size: 3,
                batches_limit: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 10);

    let target_info = target.info().await.unwrap();
    assert_eq!(target_info.doc_count, 10);
}

// =========================================================================
// Filtered replication (doc_ids, selector, custom)
// =========================================================================

#[tokio::test]
async fn replication_filtered_by_doc_ids() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source.put("a", serde_json::json!({"v": 1})).await.unwrap();
    source.put("b", serde_json::json!({"v": 2})).await.unwrap();
    source.put("c", serde_json::json!({"v": 3})).await.unwrap();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                filter: Some(rouchdb::ReplicationFilter::DocIds(vec![
                    "a".into(),
                    "c".into(),
                ])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    let target_info = target.info().await.unwrap();
    assert_eq!(target_info.doc_count, 2);
    assert!(target.get("a").await.is_ok());
    assert!(target.get("b").await.is_err());
    assert!(target.get("c").await.is_ok());
}

#[tokio::test]
async fn replication_filtered_by_selector() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
        .put(
            "alice",
            serde_json::json!({"type": "user", "name": "Alice"}),
        )
        .await
        .unwrap();
    source
        .put(
            "inv1",
            serde_json::json!({"type": "invoice", "amount": 100}),
        )
        .await
        .unwrap();
    source
        .put("bob", serde_json::json!({"type": "user", "name": "Bob"}))
        .await
        .unwrap();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                filter: Some(rouchdb::ReplicationFilter::Selector(
                    serde_json::json!({"type": "user"}),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(target.info().await.unwrap().doc_count, 2);
}
