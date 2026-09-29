//! Integration tests for new PouchDB parity features against CouchDB.
//! These require a running CouchDB instance:
//!   docker compose up -d
//!   cargo test -p rouchdb --test parity_http -- --ignored

mod common;

use common::fresh_remote_db;
use rouchdb::{
    AllDocsOptions, ChangesOptions, ChangesStreamOptions, Database, FindOptions, IndexDefinition,
    ReplicationOptions, SortField,
};
use std::time::Duration;

// =========================================================================
// db.explain() on HTTP
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn explain_on_http_without_index() {
    let url = fresh_remote_db("explain").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();

    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gt": 20}}),
            ..Default::default()
        })
        .await;

    // Should fall back to _all_docs since no Mango index
    assert_eq!(explanation.index.name, "_all_docs");
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn explain_on_http_with_index() {
    let url = fresh_remote_db("explain_idx").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();

    db.create_index(IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gt": 20}}),
            ..Default::default()
        })
        .await;

    assert_eq!(explanation.index.name, "idx-age");
    assert_eq!(explanation.index.index_type, "json");
}

// =========================================================================
// Design documents on HTTP
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn design_doc_crud_on_http() {
    let url = fresh_remote_db("ddoc").await;
    let db = Database::http(&url);

    let ddoc = rouchdb::DesignDocument {
        id: "_design/myapp".into(),
        rev: None,
        views: {
            let mut v = std::collections::HashMap::new();
            v.insert(
                "by_type".into(),
                rouchdb::ViewDef {
                    map: "function(doc) { emit(doc.type, 1); }".into(),
                    reduce: Some("_count".into()),
                    ..Default::default()
                },
            );
            v
        },
        filters: std::collections::HashMap::new(),
        validate_doc_update: None,
        shows: std::collections::HashMap::new(),
        lists: std::collections::HashMap::new(),
        updates: std::collections::HashMap::new(),
        language: Some("javascript".into()),
        ..Default::default()
    };

    let result = db.put_design(ddoc).await.unwrap();
    assert!(result.ok);

    let retrieved = db.get_design("myapp").await.unwrap();
    assert_eq!(retrieved.id, "_design/myapp");
    assert!(retrieved.views.contains_key("by_type"));

    // Delete
    let rev = retrieved.rev.unwrap();
    let del = db.delete_design("myapp", &rev).await.unwrap();
    assert!(del.ok);

    // Should be gone
    assert!(db.get_design("myapp").await.is_err());
}

// =========================================================================
// Security document on HTTP
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn security_document_round_trips_on_http() {
    let url = fresh_remote_db("security").await;
    let db = Database::http(&url);

    let wanted = serde_json::json!({
        "admins": {"names": ["ann"], "roles": ["ops"]},
        "members": {"names": [], "roles": ["_admin", "staff"]},
    });
    db.put_security(serde_json::from_value(wanted.clone()).unwrap())
        .await
        .unwrap();
    let stored = db.get_security().await.unwrap();
    assert_eq!(serde_json::to_value(&stored).unwrap(), wanted);
}

// =========================================================================
// Replication with since override (HTTP)
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn replication_since_override_http() {
    let url = fresh_remote_db("repl_since").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    remote
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    remote
        .put("doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    remote
        .put("doc3", serde_json::json!({"v": 3}))
        .await
        .unwrap();

    // CouchDB (q=2) orders the feed by shard, so whatever followed the
    // first entry is what a replication from its seq must bring.
    let changes = remote.changes(ChangesOptions::default()).await.unwrap();
    let mut expected: Vec<String> = changes.results[1..].iter().map(|c| c.id.clone()).collect();
    expected.sort();

    let result = rouchdb::replicate(
        remote.adapter(),
        local.adapter(),
        ReplicationOptions {
            since: Some(changes.results[0].seq.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!((result.docs_read, result.docs_written), (2, 2));
    let ids: Vec<String> = local
        .all_docs(AllDocsOptions::new())
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert_eq!(ids, expected);
}

// =========================================================================
// Replication without checkpoint (HTTP)
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn replication_no_checkpoint_http() {
    let url = fresh_remote_db("repl_nockpt").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    local
        .put("doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    let result = local
        .replicate_to_with_opts(
            &remote,
            ReplicationOptions {
                checkpoint: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 1);

    let doc = remote.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);

    // No checkpoint was stored on either side.
    let rep_id = rouchdb_replication::Checkpointer::new(
        &local.adapter().id().await.unwrap(),
        &remote.adapter().id().await.unwrap(),
        "nofilter",
    )
    .replication_id()
    .to_string();
    for db in [&local, &remote] {
        let cp = db.adapter().get_local(&rep_id).await;
        assert!(
            matches!(cp, Err(rouchdb::RouchError::NotFound(_))),
            "{cp:?}"
        );
    }
}

// =========================================================================
// allDocs with conflicts option on HTTP
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_conflicts_http() {
    let url = fresh_remote_db("alldocs_conflicts").await;
    let db = Database::http(&url);

    // doc1 gets two branches under 1-aaa (2-ccc wins); doc2 has one.
    for (id, ids) in [
        ("doc1", ["bbb", "aaa"]),
        ("doc1", ["ccc", "aaa"]),
        ("doc2", ["ddd", "aaa"]),
    ] {
        let doc = rouchdb::Document::from_json(serde_json::json!({
            "_id": id,
            "_rev": format!("2-{}", ids[0]),
            "_revisions": {"start": 2, "ids": ids},
        }))
        .unwrap();
        db.bulk_docs(vec![doc], rouchdb::BulkDocsOptions::replication())
            .await
            .unwrap();
    }

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            conflicts: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    let rows: Vec<(String, String, serde_json::Value)> = result
        .rows
        .iter()
        .map(|r| {
            let doc = r.doc.as_ref().unwrap();
            (
                r.key.clone(),
                r.rev().unwrap().to_string(),
                doc["_conflicts"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "doc1".to_string(),
                "2-ccc".to_string(),
                serde_json::json!(["2-bbb"])
            ),
            (
                "doc2".to_string(),
                "2-ddd".to_string(),
                serde_json::Value::Null
            ),
        ]
    );
}

// =========================================================================
// Live changes on HTTP with events
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn live_changes_events_http() {
    let url = fresh_remote_db("ch_events").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        include_docs: true,
        poll_interval: Duration::from_millis(200),
        ..Default::default()
    });

    let mut got_change = false;
    let timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(rouchdb::ChangesEvent::Change(ce)) => {
                        assert_eq!(ce.id, "doc1");
                        got_change = true;
                        break;
                    }
                    Some(_) => continue,
                    None => break,
                }
            }
            _ = &mut timeout => break,
        }
    }

    assert!(got_change);
    handle.cancel();
}

// =========================================================================
// Mango find with index on HTTP
// =========================================================================

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn mango_find_with_index_http() {
    let url = fresh_remote_db("mango_idx").await;
    let db = Database::http(&url);

    db.put(
        "alice",
        serde_json::json!({"name": "Alice", "age": 30, "type": "user"}),
    )
    .await
    .unwrap();
    db.put(
        "bob",
        serde_json::json!({"name": "Bob", "age": 25, "type": "user"}),
    )
    .await
    .unwrap();
    db.put(
        "charlie",
        serde_json::json!({"name": "Charlie", "age": 35, "type": "user"}),
    )
    .await
    .unwrap();

    // Create index
    db.create_index(IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    // Find using index
    let result = db
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 30}}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);
}
