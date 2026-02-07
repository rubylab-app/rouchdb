//! Replication tests: push, pull, sync, incremental, batched, edge cases.

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{
    ChangesOptions, Database, ReplicationEvent, ReplicationFilter, ReplicationOptions,
};

// =========================================================================
// Basic replication (local ↔ remote)
// =========================================================================

#[tokio::test]
#[ignore]
async fn replicate_memory_to_couchdb() {
    let url = fresh_remote_db("repl_to_couch").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    local
        .put("doc2", serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();
    local
        .put("doc3", serde_json::json!({"name": "Charlie"}))
        .await
        .unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 3);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "Alice");

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_couchdb_to_memory() {
    let url = fresh_remote_db("repl_from_couch").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("doc1", serde_json::json!({"city": "NYC"}))
        .await
        .unwrap();
    remote
        .put("doc2", serde_json::json!({"city": "LA"}))
        .await
        .unwrap();

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = local.get("doc1").await.unwrap();
    assert_eq!(doc.data["city"], "NYC");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn bidirectional_sync_with_couchdb() {
    let url = fresh_remote_db("bidir_sync").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

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

    let _ = local.get("remote_doc").await.unwrap();
    let _ = remote.get("local_doc").await.unwrap();

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn incremental_replication_to_couchdb() {
    let url = fresh_remote_db("incr_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let r1 = local.replicate_to(&remote).await.unwrap();
    assert_eq!(r1.docs_written, 1);

    local
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    local
        .put("doc3", serde_json::json!({"v": 3}))
        .await
        .unwrap();
    let r2 = local.replicate_to(&remote).await.unwrap();
    assert_eq!(r2.docs_read, 2);
    assert_eq!(r2.docs_written, 2);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 3);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_deletes_to_couchdb() {
    let url = fresh_remote_db("repl_del").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    local.remove("doc1", &r1.rev.unwrap()).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);

    let err = remote.get("doc1").await;
    assert!(err.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_updates_to_couchdb() {
    let url = fresh_remote_db("repl_upd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    local
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn batched_replication_to_couchdb() {
    let url = fresh_remote_db("batch_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..25 {
        local
            .put(&format!("doc{:03}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 25);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 25);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn already_synced_noop() {
    let url = fresh_remote_db("synced_noop").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_memory_to_redb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let memory = Database::memory("source");
    let redb = Database::open(&path, "target").unwrap();

    memory
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    memory
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let result = memory.replicate_to(&redb).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = redb.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);
}

#[tokio::test]
#[ignore]
async fn replicate_redb_to_couchdb() {
    let url = fresh_remote_db("redb_to_couch").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let local = Database::open(&path, "local").unwrap();
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"origin": "redb"}))
        .await
        .unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 1);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["origin"], "redb");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn mango_query_against_couchdb_data() {
    let url = fresh_remote_db("mango_couch").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("alice", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();
    remote
        .put("bob", serde_json::json!({"name": "Bob", "age": 25}))
        .await
        .unwrap();
    remote
        .put("charlie", serde_json::json!({"name": "Charlie", "age": 35}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local
        .find(rouchdb::FindOptions {
            selector: serde_json::json!({"age": {"$gte": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn multiple_sync_rounds() {
    let url = fresh_remote_db("multi_sync").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Round 1: local creates, syncs
    local
        .put("doc1", serde_json::json!({"round": 1}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    // Round 2: remote creates, syncs
    remote
        .put("doc2", serde_json::json!({"round": 2}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    // Round 3: both create, sync
    local
        .put("doc3", serde_json::json!({"round": 3}))
        .await
        .unwrap();
    remote
        .put("doc4", serde_json::json!({"round": 4}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    let local_info = local.info().await.unwrap();
    let remote_info = remote.info().await.unwrap();
    assert_eq!(local_info.doc_count, 4);
    assert_eq!(remote_info.doc_count, 4);

    delete_remote_db(&url).await;
}

// =========================================================================
// Replication edge cases
// =========================================================================

#[tokio::test]
#[ignore]
async fn replicate_multiple_updates_same_doc() {
    let url = fresh_remote_db("repl_multiupd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    let r2 = local
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let r3 = local
        .update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3}))
        .await
        .unwrap();
    let _r4 = local
        .update("doc1", &r3.rev.unwrap(), serde_json::json!({"v": 4}))
        .await
        .unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 4);
    assert!(doc.rev.unwrap().to_string().starts_with("4-"));

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_delete_and_recreate() {
    let url = fresh_remote_db("repl_delrec").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();
    local.remove("doc1", &r1.rev.unwrap()).await.unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(remote.get("doc1").await.is_err());

    // Find the tombstone rev via changes and update to "un-delete"
    let changes = local.changes(ChangesOptions::default()).await.unwrap();
    let doc1_change = changes.results.iter().find(|r| r.id == "doc1").unwrap();
    let tombstone_rev = &doc1_change.changes[0].rev;

    local
        .update(
            "doc1",
            tombstone_rev,
            serde_json::json!({"v": "resurrected"}),
        )
        .await
        .unwrap();

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], "resurrected");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_empty_databases() {
    let url = fresh_remote_db("repl_empty").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_read, 0);
    assert_eq!(result.docs_written, 0);

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_read, 0);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_couchdb_to_redb() {
    let url = fresh_remote_db("couch_to_redb").await;
    let remote = Database::http(&url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let local = Database::open(&path, "local").unwrap();

    remote
        .put("doc1", serde_json::json!({"source": "couchdb"}))
        .await
        .unwrap();
    remote
        .put("doc2", serde_json::json!({"source": "couchdb"}))
        .await
        .unwrap();

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let doc = local.get("doc1").await.unwrap();
    assert_eq!(doc.data["source"], "couchdb");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_redb_bidirectional_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let redb = Database::open(&path, "redb").unwrap();
    let memory = Database::memory("mem");

    redb.put("from_redb", serde_json::json!({"source": "redb"}))
        .await
        .unwrap();
    memory
        .put("from_mem", serde_json::json!({"source": "memory"}))
        .await
        .unwrap();

    let (push, pull) = redb.sync(&memory).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);

    let _ = redb.get("from_mem").await.unwrap();
    let _ = memory.get("from_redb").await.unwrap();

    let redb_info = redb.info().await.unwrap();
    let mem_info = memory.info().await.unwrap();
    assert_eq!(redb_info.doc_count, 2);
    assert_eq!(mem_info.doc_count, 2);
}

#[tokio::test]
#[ignore]
async fn replicate_single_doc_batches() {
    let url = fresh_remote_db("batch_one").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..5 {
        local
            .put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 5);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 5);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_exact_batch_boundary() {
    let url = fresh_remote_db("batch_exact").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..10 {
        local
            .put(&format!("doc{:02}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 10);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_large_batch() {
    let url = fresh_remote_db("batch_large").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..200 {
        local
            .put(&format!("doc{:04}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.docs_written, 200);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 200);

    delete_remote_db(&url).await;
}

// =========================================================================
// Pull updates and deletes back from CouchDB
// =========================================================================

#[tokio::test]
#[ignore]
async fn replicate_remote_updates_back_to_local() {
    let url = fresh_remote_db("pull_updates").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote
        .update("doc1", &remote_rev, serde_json::json!({"v": 2}))
        .await
        .unwrap();

    local.replicate_from(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_remote_deletes_back_to_local() {
    let url = fresh_remote_db("pull_deletes").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote.remove("doc1", &remote_rev).await.unwrap();

    local.replicate_from(&remote).await.unwrap();

    let result = local.get("doc1").await;
    assert!(result.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn sync_interleaved_updates() {
    let url = fresh_remote_db("interleave").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    let _r2 = local
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    let remote_doc = remote.get("doc1").await.unwrap();
    let remote_rev = remote_doc.rev.unwrap().to_string();
    remote
        .update("doc1", &remote_rev, serde_json::json!({"v": 3}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    let local_rev = local_doc.rev.unwrap().to_string();
    local
        .update("doc1", &local_rev, serde_json::json!({"v": 4}))
        .await
        .unwrap();
    local.sync(&remote).await.unwrap();

    let final_local = local.get("doc1").await.unwrap();
    let final_remote = remote.get("doc1").await.unwrap();
    assert_eq!(final_local.data["v"], 4);
    assert_eq!(final_remote.data["v"], 4);
    assert_eq!(
        final_local.rev.unwrap().to_string(),
        final_remote.rev.unwrap().to_string()
    );

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn sync_many_docs_diverse_operations() {
    let url = fresh_remote_db("diverse_ops").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let mut revs = Vec::new();
    for i in 0..10 {
        let r = local
            .put(&format!("doc{:02}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
        revs.push(r.rev.unwrap());
    }
    local.sync(&remote).await.unwrap();

    // Update even-numbered docs
    for i in (0..10).step_by(2) {
        local
            .update(
                &format!("doc{:02}", i),
                &revs[i],
                serde_json::json!({"i": i, "updated": true}),
            )
            .await
            .unwrap();
    }

    // Delete odd-numbered docs
    for i in (1..10).step_by(2) {
        local
            .remove(&format!("doc{:02}", i), &revs[i])
            .await
            .unwrap();
    }

    local.sync(&remote).await.unwrap();

    let remote_info = remote.info().await.unwrap();
    assert_eq!(remote_info.doc_count, 5);

    for i in (0..10).step_by(2) {
        let doc = remote.get(&format!("doc{:02}", i)).await.unwrap();
        assert_eq!(doc.data["updated"], true);
    }

    for i in (1..10).step_by(2) {
        let result = remote.get(&format!("doc{:02}", i)).await;
        assert!(result.is_err(), "doc{:02} should be deleted", i);
    }

    delete_remote_db(&url).await;
}

// =========================================================================
// Filtered replication
// =========================================================================

#[tokio::test]
#[ignore]
async fn replicate_filtered_doc_ids_to_couchdb() {
    let url = fresh_remote_db("filter_docids").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    local
        .put("doc3", serde_json::json!({"v": 3}))
        .await
        .unwrap();
    local
        .put("doc4", serde_json::json!({"v": 4}))
        .await
        .unwrap();

    let result = local
        .replicate_to_with_opts(
            &remote,
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

    assert!(result.ok);
    assert_eq!(result.docs_written, 2);

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 2);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);
    let doc = remote.get("doc3").await.unwrap();
    assert_eq!(doc.data["v"], 3);

    assert!(remote.get("doc2").await.is_err());
    assert!(remote.get("doc4").await.is_err());

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_filtered_selector_from_couchdb() {
    let url = fresh_remote_db("filter_selector").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put(
            "inv1",
            serde_json::json!({"type": "invoice", "amount": 100}),
        )
        .await
        .unwrap();
    remote
        .put(
            "inv2",
            serde_json::json!({"type": "invoice", "amount": 200}),
        )
        .await
        .unwrap();
    remote
        .put(
            "user1",
            serde_json::json!({"type": "user", "name": "Alice"}),
        )
        .await
        .unwrap();

    let result = rouchdb::replicate(
        remote.adapter(),
        local.adapter(),
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

    let info = local.info().await.unwrap();
    assert_eq!(info.doc_count, 2);

    let doc = local.get("inv1").await.unwrap();
    assert_eq!(doc.data["amount"], 100);

    assert!(local.get("user1").await.is_err());

    delete_remote_db(&url).await;
}

// =========================================================================
// Replication with event streaming
// =========================================================================

#[tokio::test]
#[ignore]
async fn replicate_to_couchdb_with_events() {
    let url = fresh_remote_db("repl_events").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..5 {
        local
            .put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let (result, mut rx) = local
        .replicate_to_with_events(&remote, ReplicationOptions::default())
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 5);

    // Collect all events
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Should have at least Active and Complete events
    assert!(events.iter().any(|e| matches!(e, ReplicationEvent::Active)));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReplicationEvent::Complete(_)))
    );

    // Verify data actually arrived
    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 5);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn replicate_events_include_change_progress() {
    let url = fresh_remote_db("repl_evt_prog").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    for i in 0..10 {
        local
            .put(&format!("doc{:02}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let (result, mut rx) = local
        .replicate_to_with_events(
            &remote,
            ReplicationOptions {
                batch_size: 5,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 10);

    let mut change_events = 0;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, ReplicationEvent::Change { .. }) {
            change_events += 1;
        }
    }

    // With batch_size=5 and 10 docs, should have at least 2 Change events
    assert!(change_events >= 2, "got {} change events", change_events);

    delete_remote_db(&url).await;
}

// =========================================================================
// Live replication
// =========================================================================

#[tokio::test]
#[ignore]
async fn live_replicate_to_couchdb() {
    let url = fresh_remote_db("live_repl").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Add initial docs
    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let (mut rx, handle) = local.replicate_to_live(
        &remote,
        ReplicationOptions {
            poll_interval: std::time::Duration::from_millis(200),
            live: true,
            ..Default::default()
        },
    );

    // Wait for the initial batch to complete
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ReplicationEvent::Complete(r)) if r.docs_written > 0 => break,
                    Some(ReplicationEvent::Paused) => {
                        if remote.get("doc1").await.is_ok() {
                            break;
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                panic!("live replication timed out waiting for initial sync");
            }
        }
    }

    // Verify docs arrived
    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);

    let info = remote.info().await.unwrap();
    assert!(info.doc_count >= 2);

    // Cancel live replication
    handle.cancel();

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn live_replicate_picks_up_new_docs() {
    let url = fresh_remote_db("live_new").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    // Start live replication with no initial data
    let (mut rx, handle) = local.replicate_to_live(
        &remote,
        ReplicationOptions {
            poll_interval: std::time::Duration::from_millis(100),
            live: true,
            ..Default::default()
        },
    );

    // Wait for initial paused (empty database)
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ReplicationEvent::Paused | ReplicationEvent::Complete(_)) => break,
                    None => break,
                    _ => {}
                }
            }
            _ = &mut timeout => break,
        }
    }

    // Now add a document — live replication should pick it up
    local
        .put("late_doc", serde_json::json!({"arrived": "late"}))
        .await
        .unwrap();

    // Wait for it to replicate
    let timeout2 = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout2);
    let mut replicated = false;
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ReplicationEvent::Complete(r)) if r.docs_written > 0 => {
                        replicated = true;
                        break;
                    }
                    Some(ReplicationEvent::Paused) => {
                        if remote.get("late_doc").await.is_ok() {
                            replicated = true;
                            break;
                        }
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = &mut timeout2 => break,
        }
    }

    handle.cancel();

    assert!(replicated, "late_doc was not replicated by live replication");
    let doc = remote.get("late_doc").await.unwrap();
    assert_eq!(doc.data["arrived"], "late");

    delete_remote_db(&url).await;
}
