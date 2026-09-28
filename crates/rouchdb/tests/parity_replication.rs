//! Replication options through the `Database` facade:
//! - ReplicationOptions::since (override starting point)
//! - ReplicationOptions::checkpoint (disable checkpointing)
//! - Replication events
//! - Live replication
//!
//! Protocol details (batching, filters, conflicts, checkpoints) are covered
//! next to the implementation in `rouchdb-replication`.

use std::time::Duration;

use rouchdb::{AllDocsOptions, Database, ReplicationEvent, ReplicationOptions, RouchError};

async fn ids(db: &Database) -> Vec<String> {
    db.all_docs(AllDocsOptions::new())
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.id)
        .collect()
}

// =========================================================================
// ReplicationOptions::since — override starting point
// =========================================================================

#[tokio::test]
async fn replication_with_since_override() {
    let source = Database::memory("source");
    let target = Database::memory("target");
    for (i, id) in ["doc1", "doc2", "doc3"].into_iter().enumerate() {
        source.put(id, serde_json::json!({"v": i})).await.unwrap();
    }

    let changes = source
        .changes(rouchdb::ChangesOptions::default())
        .await
        .unwrap();
    assert_eq!(changes.results.len(), 3);

    // Start after the first change: exactly the other two arrive.
    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                since: Some(changes.results[0].seq.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok, "{:?}", result.errors);
    assert_eq!((result.docs_read, result.docs_written), (2, 2));
    assert_eq!(ids(&target).await, vec!["doc2", "doc3"]);
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
    let opts = || ReplicationOptions {
        checkpoint: false,
        ..Default::default()
    };

    let result = source
        .replicate_to_with_opts(&target, opts())
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 1);

    // No checkpoint on either side...
    let rep_id = rouchdb_replication::Checkpointer::new("source", "target", "nofilter")
        .replication_id()
        .to_string();
    for db in [&source, &target] {
        assert!(matches!(
            db.adapter().get_local(&rep_id).await,
            Err(RouchError::NotFound(_))
        ));
    }

    // ...so the next run reads the whole feed again.
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let result2 = source
        .replicate_to_with_opts(&target, opts())
        .await
        .unwrap();
    assert!(result2.ok);
    assert_eq!((result2.docs_read, result2.docs_written), (2, 1));
    assert_eq!(ids(&target).await, vec!["doc1", "doc2"]);
}

// =========================================================================
// Replication with events
// =========================================================================

#[tokio::test]
async fn replication_events_report_each_batch_then_complete() {
    let source = Database::memory("source");
    let target = Database::memory("target");
    for i in 0..5 {
        source
            .put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let (result, mut rx) = source
        .replicate_to_with_events(
            &target,
            ReplicationOptions {
                batch_size: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 5);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let summary: Vec<String> = events
        .iter()
        .map(|e| match e {
            ReplicationEvent::Active => "active".into(),
            ReplicationEvent::Change { docs_read } => format!("change {docs_read}"),
            ReplicationEvent::Complete(r) => format!("complete {}", r.docs_written),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        summary,
        vec!["active", "change 2", "change 4", "change 5", "complete 5"]
    );
}

// =========================================================================
// Live replication
// =========================================================================

async fn wait_for(
    rx: &mut tokio::sync::mpsc::Receiver<ReplicationEvent>,
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

    // The first idle pass comes after doc1 was written.
    assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
    assert_eq!(ids(&target).await, vec!["doc1"]);

    // A doc written while idle is picked up by a later poll.
    source
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(
        wait_for(&mut rx, |e| matches!(
            e,
            ReplicationEvent::Change { docs_read: 1 }
        ))
        .await
    );
    assert_eq!(target.get("doc2").await.unwrap().data["v"], 2);
    handle.cancel();
}
