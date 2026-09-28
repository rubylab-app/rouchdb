//! Changes feed advanced options: since, limit, include_docs, selector, live changes.

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{ChangesOptions, ChangesStreamOptions, Database};

#[tokio::test]
#[ignore]
async fn changes_since_sequence() {
    let url = fresh_remote_db("ch_since").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();
    db.put("doc3", serde_json::json!({"v": 3})).await.unwrap();

    let all = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(all.results.len(), 3);

    let since_seq = all.results[1].seq.clone();
    let partial = db
        .changes(ChangesOptions {
            since: since_seq,
            ..Default::default()
        })
        .await
        .unwrap();

    // CouchDB (q=2) orders the feed by shard, not by write time: the exact
    // expectation is whatever followed results[1] in the full feed.
    let ids =
        |r: &[rouchdb::ChangeEvent]| -> Vec<String> { r.iter().map(|c| c.id.clone()).collect() };
    assert_eq!(ids(&partial.results), ids(&all.results[2..]));

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_with_doc_ids_lists_only_those_docs() {
    let url = fresh_remote_db("ch_docids").await;
    let db = Database::http(&url);
    for id in ["doc1", "doc2", "doc3"] {
        db.put(id, serde_json::json!({})).await.unwrap();
    }

    let filtered = db
        .changes(ChangesOptions {
            doc_ids: Some(vec!["doc2".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    let ids: Vec<&str> = filtered.results.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["doc2"]);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_with_limit() {
    let url = fresh_remote_db("ch_limit").await;
    let db = Database::http(&url);

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

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_include_docs() {
    let url = fresh_remote_db("ch_docs").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();

    let changes = db
        .changes(ChangesOptions {
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 1);
    let doc = changes.results[0].doc.as_ref().unwrap();
    assert_eq!(doc["name"], "Alice");

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn changes_after_updates_and_deletes() {
    let url = fresh_remote_db("ch_upddel").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 1})).await.unwrap();
    let r3 = db.put("doc3", serde_json::json!({"v": 1})).await.unwrap();

    assert!(
        db.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .ok
    );
    assert!(db.remove("doc3", &r3.rev.unwrap()).await.unwrap().ok);

    let changes = db.changes(ChangesOptions::default()).await.unwrap();

    let ids: Vec<&str> = changes.results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"doc1"));
    assert!(ids.contains(&"doc2"));
    assert!(ids.contains(&"doc3"));

    let doc3_change = changes.results.iter().find(|r| r.id == "doc3").unwrap();
    assert!(doc3_change.deleted);

    delete_remote_db(&url).await;
}

// =========================================================================
// Selector filter on changes (CouchDB _selector filter)
// =========================================================================

#[tokio::test]
#[ignore]
async fn changes_with_selector_filter() {
    let url = fresh_remote_db("ch_sel").await;
    let db = Database::http(&url);

    db.put(
        "user1",
        serde_json::json!({"type": "user", "name": "Alice"}),
    )
    .await
    .unwrap();
    db.put(
        "inv1",
        serde_json::json!({"type": "invoice", "amount": 100}),
    )
    .await
    .unwrap();
    db.put("user2", serde_json::json!({"type": "user", "name": "Bob"}))
        .await
        .unwrap();

    let changes = db
        .changes(ChangesOptions {
            selector: Some(serde_json::json!({"type": "user"})),
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();

    let mut got: Vec<(String, serde_json::Value)> = changes
        .results
        .iter()
        .map(|e| (e.id.clone(), e.doc.as_ref().unwrap()["name"].clone()))
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0)); // shard order
    assert_eq!(
        got,
        vec![
            ("user1".to_string(), serde_json::json!("Alice")),
            ("user2".to_string(), serde_json::json!("Bob")),
        ]
    );

    delete_remote_db(&url).await;
}

// =========================================================================
// Live changes via Database::live_changes()
// =========================================================================

#[tokio::test]
#[ignore]
async fn live_changes_picks_up_new_docs() {
    let url = fresh_remote_db("ch_live").await;
    let db = Database::http(&url);

    db.put("existing", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        poll_interval: std::time::Duration::from_millis(200),
        ..Default::default()
    });

    // Should receive the existing doc
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.id, "existing");

    // Add a new doc
    db.put("new1", serde_json::json!({"v": 2})).await.unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.id, "new1");

    handle.cancel();
    delete_remote_db(&url).await;
}
