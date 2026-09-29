//! Replication tests: push, pull, sync, incremental, batched, edge cases.

mod common;
mod snapshot;

use common::fresh_remote_db;
use rouchdb::{ChangesOptions, Database, ReplicationEvent, ReplicationFilter, ReplicationOptions};
use snapshot::assert_same_state;

// =========================================================================
// Basic replication (local ↔ remote)
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&remote, &local).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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

    assert_eq!(
        local.get("remote_doc").await.unwrap().data["from"],
        "remote"
    );
    assert_eq!(remote.get("local_doc").await.unwrap().data["from"], "local");
    let state = assert_same_state(&local, &remote).await;
    assert_eq!(
        state.keys().collect::<Vec<_>>(),
        vec!["local_doc", "remote_doc"]
    );
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn replicate_deletes_to_couchdb() {
    let url = fresh_remote_db("repl_del").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(local.remove("doc1", &r1.rev.unwrap()).await.unwrap().ok);

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok);

    let err = remote.get("doc1").await;
    assert!(
        matches!(err, Err(rouchdb::RouchError::NotFound(_))),
        "{err:?}"
    );
    // A tombstone with the same revision, not a missing document.
    let state = assert_same_state(&local, &remote).await;
    assert!(state["doc1"].deleted);
    assert!(state["doc1"].rev.starts_with("2-"));
    assert_eq!(remote.info().await.unwrap().doc_del_count, 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn replicate_updates_to_couchdb() {
    let url = fresh_remote_db("repl_upd").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();

    assert!(
        local
            .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .ok
    );

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);
    let state = assert_same_state(&local, &remote).await;
    assert_eq!(state["doc1"].history.len(), 2);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
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
    assert_same_state(&memory, &redb).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&local, &remote).await;
}

// =========================================================================
// Replication edge cases
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    // The whole history travels, not just the winning revision.
    let state = assert_same_state(&local, &remote).await;
    assert_eq!(state["doc1"].history.len(), 4);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn replicate_delete_and_recreate() {
    let url = fresh_remote_db("repl_delrec").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let r1 = local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    local.replicate_to(&remote).await.unwrap();
    assert!(local.remove("doc1", &r1.rev.unwrap()).await.unwrap().ok);
    local.replicate_to(&remote).await.unwrap();

    assert!(remote.get("doc1").await.is_err());

    // Find the tombstone rev via changes and update to "un-delete"
    let changes = local.changes(ChangesOptions::default()).await.unwrap();
    let doc1_change = changes.results.iter().find(|r| r.id == "doc1").unwrap();
    let tombstone_rev = &doc1_change.changes[0].rev;

    assert!(
        local
            .update(
                "doc1",
                tombstone_rev,
                serde_json::json!({"v": "resurrected"}),
            )
            .await
            .unwrap()
            .ok
    );

    local.replicate_to(&remote).await.unwrap();

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], "resurrected");
    let state = assert_same_state(&local, &remote).await;
    assert!(!state["doc1"].deleted);
    assert!(state["doc1"].rev.starts_with("3-"));
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert_same_state(&remote, &local).await;
}

#[tokio::test]
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

    assert_eq!(redb.get("from_mem").await.unwrap().data["source"], "memory");
    assert_eq!(
        memory.get("from_redb").await.unwrap().data["source"],
        "redb"
    );

    let redb_info = redb.info().await.unwrap();
    let mem_info = memory.info().await.unwrap();
    assert_eq!(redb_info.doc_count, 2);
    assert_eq!(mem_info.doc_count, 2);
    assert_same_state(&redb, &memory).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
}

// =========================================================================
// Pull updates and deletes back from CouchDB
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert!(
        remote
            .update("doc1", &remote_rev, serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .ok
    );

    local.replicate_from(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    assert_eq!(local_doc.data["v"], 2);
    // The remote edit extends the local branch: same rev, no conflict.
    let state = assert_same_state(&local, &remote).await;
    assert!(state["doc1"].rev.starts_with("2-"));
    assert_eq!(state["doc1"].leaves, vec![state["doc1"].rev.clone()]);
    assert!(state["doc1"].conflicts.is_empty());
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert!(remote.remove("doc1", &remote_rev).await.unwrap().ok);

    local.replicate_from(&remote).await.unwrap();

    let result = local.get("doc1").await;
    assert!(
        matches!(result, Err(rouchdb::RouchError::NotFound(_))),
        "{result:?}"
    );
    let state = assert_same_state(&local, &remote).await;
    assert!(state["doc1"].deleted);
    assert_eq!(local.info().await.unwrap().doc_count, 0);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    assert!(
        remote
            .update("doc1", &remote_rev, serde_json::json!({"v": 3}))
            .await
            .unwrap()
            .ok
    );
    local.sync(&remote).await.unwrap();

    let local_doc = local.get("doc1").await.unwrap();
    let local_rev = local_doc.rev.unwrap().to_string();
    assert!(
        local
            .update("doc1", &local_rev, serde_json::json!({"v": 4}))
            .await
            .unwrap()
            .ok
    );
    local.sync(&remote).await.unwrap();

    let final_local = local.get("doc1").await.unwrap();
    let final_remote = remote.get("doc1").await.unwrap();
    assert_eq!(final_local.data["v"], 4);
    assert_eq!(final_remote.data["v"], 4);
    assert_eq!(
        final_local.rev.unwrap().to_string(),
        final_remote.rev.unwrap().to_string()
    );
    let state = assert_same_state(&local, &remote).await;
    assert_eq!(state["doc1"].history.len(), 4);
    assert!(state["doc1"].conflicts.is_empty());
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
        assert!(
            local
                .update(
                    &format!("doc{:02}", i),
                    &revs[i],
                    serde_json::json!({"i": i, "updated": true}),
                )
                .await
                .unwrap()
                .ok
        );
    }

    // Delete odd-numbered docs
    for i in (1..10).step_by(2) {
        assert!(
            local
                .remove(&format!("doc{:02}", i), &revs[i])
                .await
                .unwrap()
                .ok
        );
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
    let state = assert_same_state(&local, &remote).await;
    let tombstones: Vec<&str> = state
        .iter()
        .filter(|(_, d)| d.deleted)
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(
        tombstones,
        vec!["doc01", "doc03", "doc05", "doc07", "doc09"]
    );
}

// =========================================================================
// Filtered replication
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    let state = snapshot::snapshot(&remote).await;
    assert_eq!(state.keys().collect::<Vec<_>>(), vec!["doc1", "doc3"]);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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
    let state = snapshot::snapshot(&local).await;
    assert_eq!(state.keys().collect::<Vec<_>>(), vec!["inv1", "inv2"]);
}

// =========================================================================
// Replication with event streaming
// =========================================================================

/// One line per event, to compare whole event sequences.
fn summary(events: &[ReplicationEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            ReplicationEvent::Active => "active".to_string(),
            ReplicationEvent::Paused => "paused".to_string(),
            ReplicationEvent::Change { docs_read } => format!("change {docs_read}"),
            ReplicationEvent::Complete(r) => {
                format!("complete ok={} written={}", r.ok, r.docs_written)
            }
            ReplicationEvent::Error(m) => format!("error {m}"),
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert_eq!(
        summary(&events),
        vec!["active", "change 5", "complete ok=true written=5"]
    );

    let info = remote.info().await.unwrap();
    assert_eq!(info.doc_count, 5);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
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

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    // One Change per batch, carrying the running total.
    assert_eq!(
        summary(&events),
        vec![
            "active",
            "change 5",
            "change 10",
            "complete ok=true written=10"
        ]
    );
}

// =========================================================================
// Live replication
// =========================================================================

/// Wait (bounded) for the first event matching `pred`.
async fn wait_for(
    rx: &mut tokio::sync::mpsc::Receiver<ReplicationEvent>,
    pred: impl Fn(&ReplicationEvent) -> bool,
) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
#[ignore = "requires CouchDB"]
async fn live_replicate_to_couchdb() {
    let url = fresh_remote_db("live_repl").await;
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

    let (mut rx, handle) = local.replicate_to_live(
        &remote,
        ReplicationOptions {
            poll_interval: std::time::Duration::from_millis(200),
            live: true,
            ..Default::default()
        },
    );

    // The first pass writes both docs; the first idle pass comes after it.
    assert!(
        wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await,
        "live replication never went idle"
    );
    handle.cancel();

    assert_eq!(remote.info().await.unwrap().doc_count, 2);
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn live_replicate_picks_up_new_docs() {
    let url = fresh_remote_db("live_new").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);

    let (mut rx, handle) = local.replicate_to_live(
        &remote,
        ReplicationOptions {
            poll_interval: std::time::Duration::from_millis(100),
            live: true,
            ..Default::default()
        },
    );
    assert!(
        wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await,
        "live replication never went idle"
    );

    // Written after the replication went idle: only a later poll sees it,
    // and its Change is emitted once the doc is on the target.
    local
        .put("late_doc", serde_json::json!({"arrived": "late"}))
        .await
        .unwrap();
    assert!(
        wait_for(&mut rx, |e| matches!(
            e,
            ReplicationEvent::Change { docs_read: 1 }
        ))
        .await,
        "late_doc was not replicated by live replication"
    );
    handle.cancel();

    let doc = remote.get("late_doc").await.unwrap();
    assert_eq!(doc.data["arrived"], "late");
}

// =========================================================================
// End state and checkpoints against CouchDB
// =========================================================================

async fn raw(url: &str) -> serde_json::Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

fn replication_id(source_id: &str, target_id: &str, filter: &str) -> String {
    rouchdb_replication::Checkpointer::new(source_id, target_id, filter)
        .replication_id()
        .to_string()
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn push_carries_attachment_bytes_to_couchdb() {
    let url = fresh_remote_db("push_att").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);
    let bytes = vec![0u8, 1, 2, 255, 254, 128];

    let r1 = local.put("d", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = local
        .put_attachment(
            "d",
            "a.bin",
            &r1.rev.unwrap(),
            bytes.clone(),
            "application/octet-stream",
        )
        .await
        .unwrap();
    // A later edit: the attachment's revpos is now below the generation.
    local
        .update("d", r2.rev.as_deref().unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);

    // Read straight from CouchDB, not through rouchdb.
    let stored = reqwest::get(format!("{url}/d/a.bin"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&stored[..], &bytes[..]);
    let doc = raw(&format!("{url}/d")).await;
    assert_eq!(doc["v"], 2);
    let state = assert_same_state(&local, &remote).await;
    assert_eq!(
        state["d"].attachments["a.bin"],
        ("application/octet-stream".to_string(), bytes)
    );
    // The revision that uploaded the bytes, not 0.
    assert_eq!(doc["_attachments"]["a.bin"]["revpos"], 2);
}

/// Run the attachment writes whose `revpos` CouchDB 3.5.1 reports as
/// `a.bin: 4, b.bin: 3`: an inline upload, a body edit with a stub, a
/// standalone upload, and a re-upload of identical bytes.
async fn attachment_history(db: &Database) {
    let hello = serde_json::json!({"content_type": "application/octet-stream", "data": "aGVsbG8="});
    let stub = serde_json::json!({"stub": true});
    let rev = |r: rouchdb::DocResult| r.rev.unwrap();
    let r1 = rev(db
        .put(
            "d",
            serde_json::json!({"v": 1, "_attachments": {"a.bin": hello}}),
        )
        .await
        .unwrap());
    let r2 = rev(db
        .update(
            "d",
            &r1,
            serde_json::json!({"v": 2, "_attachments": {"a.bin": stub}}),
        )
        .await
        .unwrap());
    let r3 = rev(db
        .put_attachment(
            "d",
            "b.bin",
            &r2,
            b"xyz".to_vec(),
            "application/octet-stream",
        )
        .await
        .unwrap());
    db.update(
        "d",
        &r3,
        serde_json::json!({"v": 4, "_attachments": {"a.bin": hello, "b.bin": stub}}),
    )
    .await
    .unwrap();
}

/// Item 4: attachment stubs (`revpos` included) are the same on CouchDB and
/// on a local database after the same writes, and replication carries them
/// unchanged in both directions. (`application/octet-stream` is stored
/// uncompressed by CouchDB, so the digests are the same too.)
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn attachments_match_couchdb() {
    let url = fresh_remote_db("att_revpos").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");
    attachment_history(&remote).await;
    attachment_history(&local).await;
    let couch = raw(&format!("{url}/d")).await["_attachments"].clone();
    assert_eq!(couch["a.bin"]["revpos"], 4, "{couch}");
    assert_eq!(couch["b.bin"]["revpos"], 3, "{couch}");
    assert_eq!(
        local.get("d").await.unwrap().to_json()["_attachments"],
        couch
    );
    assert_eq!(
        remote.get("d").await.unwrap().to_json()["_attachments"],
        couch
    );

    // Pushed to CouchDB, pulled from it.
    let pushed = fresh_remote_db("att_revpos_push").await;
    assert!(
        local
            .replicate_to(&Database::http(&pushed))
            .await
            .unwrap()
            .ok
    );
    assert_eq!(raw(&format!("{pushed}/d")).await["_attachments"], couch);
    let pulled = Database::memory("pulled");
    assert!(pulled.replicate_from(&remote).await.unwrap().ok);
    assert_eq!(
        pulled.get("d").await.unwrap().to_json()["_attachments"],
        couch
    );
    let bytes = pulled.get_attachment("d", "b.bin").await.unwrap();
    assert_eq!(bytes, b"xyz");
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn pull_carries_attachment_bytes_from_couchdb() {
    let url = fresh_remote_db("pull_att").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");
    let bytes = vec![255u8, 0, 128, 7];

    let r = remote.put("d", serde_json::json!({"v": 1})).await.unwrap();
    remote
        .put_attachment("d", "b.bin", &r.rev.unwrap(), bytes.clone(), "image/png")
        .await
        .unwrap();

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(local.get_attachment("d", "b.bin").await.unwrap(), bytes);
    assert_same_state(&remote, &local).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn push_checkpoints_both_sides_with_last_seq_and_session() {
    let url = fresh_remote_db("push_cp").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);
    for i in 0..5 {
        local
            .put(&format!("d{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                batch_size: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(result.last_seq, rouchdb::Seq::Num(5));

    let id = replication_id(
        &local.adapter().id().await.unwrap(),
        &remote.adapter().id().await.unwrap(),
        "nofilter",
    );
    let on_target = raw(&format!("{url}/_local/{id}")).await;
    let on_source = local.adapter().get_local(&id).await.unwrap();
    assert_eq!(on_target["last_seq"], 5);
    assert_eq!(on_source["last_seq"], 5);
    assert_eq!(on_target["session_id"], on_source["session_id"]);
    assert_eq!(
        on_target["history"][0]["session_id"],
        on_source["session_id"]
    );
    // One write per batch (2 + 2 + 1), each passing the rev it read back.
    assert_eq!(on_target["_rev"], "0-3");
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn pull_with_doc_ids_from_couchdb_brings_only_those_docs() {
    let url = fresh_remote_db("pull_docids").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");
    for id in ["doc1", "doc2", "doc3", "doc4"] {
        remote.put(id, serde_json::json!({})).await.unwrap();
    }

    let result = rouchdb::replicate(
        remote.adapter(),
        local.adapter(),
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
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!((result.docs_read, result.docs_written), (2, 2));
    let state = snapshot::snapshot(&local).await;
    assert_eq!(state.keys().collect::<Vec<_>>(), vec!["doc1", "doc3"]);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn incremental_pull_from_couchdb_resumes_from_the_checkpoint() {
    let url = fresh_remote_db("pull_incr").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");
    for i in 0..3 {
        remote
            .put(&format!("a{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }

    // CouchDB sequences are opaque strings.
    let r1 = local.replicate_from(&remote).await.unwrap();
    assert_eq!((r1.docs_read, r1.docs_written), (3, 3));
    for i in 0..2 {
        remote
            .put(&format!("b{i}"), serde_json::json!({}))
            .await
            .unwrap();
    }
    let r2 = local.replicate_from(&remote).await.unwrap();
    assert_eq!((r2.docs_read, r2.docs_written), (2, 2), "{r2:?}");
    let r3 = local.replicate_from(&remote).await.unwrap();
    assert_eq!((r3.docs_read, r3.docs_written), (0, 0), "{r3:?}");
    assert_same_state(&remote, &local).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn redb_checkpoint_to_couchdb_survives_reopening() {
    let url = fresh_remote_db("redb_cp").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.redb");
    {
        let local = Database::open(&path, "local").unwrap();
        for i in 0..3 {
            local
                .put(&format!("d{i}"), serde_json::json!({}))
                .await
                .unwrap();
        }
        let r = local.replicate_to(&Database::http(&url)).await.unwrap();
        assert_eq!((r.docs_read, r.docs_written), (3, 3));
    }

    let local = Database::open(&path, "local").unwrap();
    local.put("late", serde_json::json!({})).await.unwrap();
    let remote = Database::http(&url);
    let r = local.replicate_to(&remote).await.unwrap();
    assert_eq!((r.docs_read, r.docs_written), (1, 1), "{r:?}");
    assert_same_state(&local, &remote).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn doc_deleted_before_the_first_push_arrives_as_a_tombstone() {
    let url = fresh_remote_db("push_tomb").await;
    let local = Database::memory("local");
    let remote = Database::http(&url);
    let r = local.put("gone", serde_json::json!({})).await.unwrap();
    local.remove("gone", &r.rev.unwrap()).await.unwrap();
    local.put("kept", serde_json::json!({})).await.unwrap();

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);

    let info = raw(&url).await;
    assert_eq!(info["doc_count"], 1);
    assert_eq!(info["doc_del_count"], 1, "{info}");
    let state = assert_same_state(&local, &remote).await;
    assert!(state["gone"].deleted);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn bad_credentials_surface_unauthorized() {
    let url = fresh_remote_db("bad_creds").await;
    // A user that does not exist, so no real account can be locked out.
    let mut bad = reqwest::Url::parse(&url).unwrap();
    bad.set_username(&format!("nobody_{}", uuid::Uuid::new_v4().simple()))
        .unwrap();
    bad.set_password(Some("bad")).unwrap();
    let local = Database::memory("local");
    local.put("d", serde_json::json!({})).await.unwrap();

    let result = local.replicate_to(&Database::http(bad.as_str())).await;
    assert!(
        matches!(result, Err(rouchdb::RouchError::Unauthorized)),
        "{result:?}"
    );
    assert_eq!(raw(&url).await["doc_count"], 0);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn live_pull_from_couchdb_delivers_later_writes() {
    let url = fresh_remote_db("live_pull").await;
    let remote = Database::http(&url);
    let local = std::sync::Arc::new(rouchdb::MemoryAdapter::new("local"));
    remote.put("a", serde_json::json!({})).await.unwrap();

    let (mut rx, handle) = rouchdb::replicate_live(
        std::sync::Arc::new(rouchdb::HttpAdapter::new(&url)),
        local.clone(),
        ReplicationOptions {
            live: true,
            poll_interval: std::time::Duration::from_millis(50),
            ..Default::default()
        },
    );
    assert!(wait_for(&mut rx, |e| matches!(e, ReplicationEvent::Paused)).await);
    let local = Database::from_adapter(local);
    assert!(local.get("a").await.is_ok());

    // A Paused from a pass that started before this write proves nothing:
    // wait for the Change that carries it.
    remote.put("b", serde_json::json!({})).await.unwrap();
    assert!(
        wait_for(&mut rx, |e| matches!(
            e,
            ReplicationEvent::Change { docs_read: 1 }
        ))
        .await
    );
    handle.cancel();
    assert_same_state(&remote, &local).await;
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn validator_unauthorized_is_denied_not_retried() {
    let url = fresh_remote_db("vdu_unauth").await;
    reqwest::Client::new()
        .put(format!("{url}/_design/guard"))
        .json(&serde_json::json!({"validate_doc_update":
            "function(doc) { if (doc.bad) { throw({unauthorized: 'nope'}); } }"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let local = Database::memory("local");
    local.put("a", serde_json::json!({})).await.unwrap();
    local
        .put("x", serde_json::json!({"bad": true}))
        .await
        .unwrap();
    local.put("b", serde_json::json!({})).await.unwrap();
    let remote = Database::http(&url);
    let opts = || ReplicationOptions {
        batch_size: 1,
        ..Default::default()
    };

    // `x` is refused for good; the docs after it still arrive.
    let r1 = local.replicate_to_with_opts(&remote, opts()).await.unwrap();
    assert!(!r1.ok);
    assert_eq!((r1.docs_read, r1.docs_written), (3, 2));
    assert_eq!(r1.errors.len(), 1, "{:?}", r1.errors);
    assert!(
        r1.errors[0].starts_with("write error for x: unauthorized"),
        "{:?}",
        r1.errors
    );
    assert!(remote.get("b").await.is_ok());
    assert!(remote.get("x").await.is_err());

    // ...and it is not retried forever.
    let r2 = local.replicate_to_with_opts(&remote, opts()).await.unwrap();
    assert_eq!(r2.docs_read, 0, "{r2:?}");
}
