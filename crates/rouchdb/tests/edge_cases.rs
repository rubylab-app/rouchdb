//! Comprehensive edge-case tests found during code review.
//! Covers: concurrent writes, plugins changing documents, partition edge
//! cases, attachment errors, index staleness, replication filters, Unicode
//! fields, changes feed boundaries, design documents and more.

mod backends;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use backends::{Backend, KINDS, backends, row_ids};
use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesEvent, ChangesOptions, ChangesStreamOptions, Database,
    DesignDocument, DocResult, Document, FindOptions, GetOptions, Plugin, ReplicationFilter,
    ReplicationOptions, Result, RouchError, Seq, SortField,
};

fn ids_of(docs: &[serde_json::Value]) -> Vec<&str> {
    docs.iter().map(|d| d["_id"].as_str().unwrap()).collect()
}

// =========================================================================
// Concurrent writes to the same document
// =========================================================================

#[tokio::test]
async fn concurrent_puts_to_different_docs() {
    let db = Database::memory("test");
    let db_arc = Arc::new(db);

    let mut handles = vec![];
    for i in 0..20 {
        let db = db_arc.clone();
        handles.push(tokio::spawn(async move {
            db.put(&format!("doc{:02}", i), serde_json::json!({"i": i}))
                .await
                .unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let info = db_arc.info().await.unwrap();
    assert_eq!((info.doc_count, info.update_seq), (20, Seq::Num(20)));
    let all = db_arc.all_docs(AllDocsOptions::new()).await.unwrap();
    let expected: Vec<String> = (0..20).map(|i| format!("doc{i:02}")).collect();
    assert_eq!(row_ids(&all), expected);
}

#[tokio::test]
async fn concurrent_updates_same_doc_produces_conflicts() {
    let db = Arc::new(Database::memory("test"));
    let r = db.put("doc1", serde_json::json!({"v": 0})).await.unwrap();
    let rev = r.rev.unwrap();

    // Two concurrent updates with the same rev — one should succeed, one should conflict
    let db1 = db.clone();
    let db2 = db.clone();
    let rev1 = rev.clone();
    let rev2 = rev;

    let (r1, r2): (rouchdb::Result<DocResult>, rouchdb::Result<DocResult>) = tokio::join!(
        db1.update("doc1", &rev1, serde_json::json!({"v": "a"})),
        db2.update("doc1", &rev2, serde_json::json!({"v": "b"}))
    );

    // Exactly one of them may win: the other must be rejected as a conflict
    // instead of silently overwriting the first (lost update).
    let (winner, winning_value) = match (r1, r2) {
        (Ok(w), Err(RouchError::Conflict)) => (w, "a"),
        (Err(RouchError::Conflict), Ok(w)) => (w, "b"),
        other => panic!("exactly one update should succeed: {other:?}"),
    };
    assert!(winner.ok, "{winner:?}");

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], winning_value);
    assert_eq!(doc.rev.unwrap().to_string(), winner.rev.unwrap());
}

// =========================================================================
// Bulk docs with duplicate IDs
// =========================================================================

#[tokio::test]
async fn bulk_docs_with_duplicate_ids_in_same_batch() {
    let db = Database::memory("test");
    let docs = vec![
        Document {
            id: "same".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 1}),
            attachments: HashMap::new(),
        },
        Document {
            id: "same".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({"v": 2}),
            attachments: HashMap::new(),
        },
    ];

    // CouchDB/PouchDB semantics: the first doc is created and the second one,
    // which carries no `_rev`, is rejected as a conflict with the first.
    let results = db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].ok, "{:?}", results[0]);
    assert!(!results[1].ok, "{:?}", results[1]);
    assert_eq!(results[1].error.as_deref(), Some("conflict"));

    let doc = db.get("same").await.unwrap();
    assert_eq!(doc.data["v"], 1);
    assert_eq!(doc.rev.unwrap().pos, 1);
}

// =========================================================================
// Plugin: modifying deleted flag
// =========================================================================

struct ResurrectPlugin;

#[async_trait::async_trait]
impl Plugin for ResurrectPlugin {
    fn name(&self) -> &str {
        "resurrect"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter_mut() {
            if doc.deleted {
                doc.deleted = false;
                if let Some(obj) = doc.data.as_object_mut() {
                    obj.insert("resurrected".into(), serde_json::json!(true));
                }
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn plugin_can_prevent_deletion() {
    for kind in KINDS {
        let b =
            Backend::open(kind, "test").configure(|db| db.with_plugin(Arc::new(ResurrectPlugin)));
        let db = &b.db;

        let r = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev = r.rev.unwrap();

        // Try to delete — the plugin turns it into an update.
        let result = db.remove("doc1", &rev).await.unwrap();
        assert!(result.ok);

        let doc = db.get("doc1").await.unwrap();
        assert!(!doc.deleted, "{kind}");
        assert_eq!(doc.rev.unwrap().to_string(), result.rev.unwrap(), "{kind}");
        assert_eq!(doc.data, serde_json::json!({"resurrected": true}), "{kind}");
        let info = db.info().await.unwrap();
        assert_eq!((info.doc_count, info.doc_del_count), (1, 0), "{kind}");
    }
}

// =========================================================================
// Partition: colon in partition name
// =========================================================================

#[tokio::test]
async fn partition_with_colon_in_name() {
    let db = Database::memory("test");

    for id in ["a:b:doc1", "a:b:doc2", "a:other", "ab:doc", "a", "b:a:doc"] {
        db.put(id, serde_json::json!({})).await.unwrap();
    }

    // Partition "a" gets every doc starting with "a:", nested colons included.
    let all = db
        .partition("a")
        .all_docs(AllDocsOptions::new())
        .await
        .unwrap();
    assert_eq!(row_ids(&all), ["a:b:doc1", "a:b:doc2", "a:other"]);
    // A partition name may itself contain a colon.
    let nested = db
        .partition("a:b")
        .all_docs(AllDocsOptions::new())
        .await
        .unwrap();
    assert_eq!(row_ids(&nested), ["a:b:doc1", "a:b:doc2"]);
}

// =========================================================================
// Partition: find with conflicting _id selector
// =========================================================================

#[tokio::test]
async fn partition_find_with_conflicting_id_selector() {
    let db = Database::memory("test");

    db.put("users:alice", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    db.put("orders:o1", serde_json::json!({"name": "Order1"}))
        .await
        .unwrap();

    let partition = &db.partition("users");
    let find = |selector: serde_json::Value| async move {
        partition
            .find(FindOptions {
                selector,
                ..Default::default()
            })
            .await
            .unwrap()
            .docs
    };

    // The partition AND the selector constraints conflict: nothing.
    let result = find(serde_json::json!({"_id": "orders:o1"})).await;
    assert!(result.is_empty(), "{result:?}");
    // A selector inside the partition still matches.
    let result = find(serde_json::json!({"_id": "users:alice"})).await;
    assert_eq!(ids_of(&result), ["users:alice"]);
}

// =========================================================================
// Index updates after document deletion
// =========================================================================

#[tokio::test]
async fn index_returns_correct_results_after_delete() {
    let db = Database::memory("test");

    db.put("alice", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();
    let bob_result = db
        .put("bob", serde_json::json!({"name": "Bob", "age": 25}))
        .await
        .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    let adults = || async {
        db.find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 0}}),
            sort: Some(vec![SortField::Simple("age".into())]),
            ..Default::default()
        })
        .await
        .unwrap()
        .docs
    };

    // Both should be found
    assert_eq!(ids_of(&adults().await), ["bob", "alice"]);

    // Delete Bob
    assert!(db.remove("bob", &bob_result.rev.unwrap()).await.unwrap().ok);

    // Only Alice should remain
    assert_eq!(ids_of(&adults().await), ["alice"]);
}

#[tokio::test]
async fn index_updates_on_field_value_change() {
    let db = Database::memory("test");

    let r = db
        .put("doc1", serde_json::json!({"status": "pending", "v": 1}))
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"status": "complete", "v": 1}))
        .await
        .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("status".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    let with_status = |status: &'static str| {
        let db = &db;
        async move {
            db.find(FindOptions {
                selector: serde_json::json!({"status": status}),
                ..Default::default()
            })
            .await
            .unwrap()
            .docs
        }
    };

    assert_eq!(ids_of(&with_status("pending").await), ["doc1"]);

    // Update to "complete"
    assert!(
        db.update(
            "doc1",
            &r.rev.unwrap(),
            serde_json::json!({"status": "complete", "v": 2}),
        )
        .await
        .unwrap()
        .ok
    );

    // The old value is gone from the index; the new one is found.
    assert!(with_status("pending").await.is_empty());
    let complete = with_status("complete").await;
    assert_eq!(ids_of(&complete), ["doc1", "doc2"]);
    assert_eq!(complete[0]["v"], 2);
}

// =========================================================================
// Changes feed: future sequence
// =========================================================================

#[tokio::test]
async fn changes_since_future_sequence_returns_empty() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();

    let changes = db
        .changes(ChangesOptions {
            since: rouchdb::Seq::Num(999999),
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(
        changes.results.is_empty(),
        "Future seq should return no results"
    );
}

// =========================================================================
// Unicode in field names and values
// =========================================================================

#[tokio::test]
async fn unicode_field_names_in_find() {
    let db = Database::memory("test");

    db.put("doc1", serde_json::json!({"名前": "Alice", "年齢": 30}))
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"名前": "Bob", "年齢": 25}))
        .await
        .unwrap();

    let result = db
        .find(FindOptions {
            selector: serde_json::json!({"名前": "Alice"}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(ids_of(&result.docs), ["doc1"]);
    assert_eq!(result.docs[0]["年齢"], 30);
}

#[tokio::test]
async fn emoji_in_field_names_and_values() {
    let db = Database::memory("test");

    let body = serde_json::json!({"status_emoji": "✅", "likes": "👍👍", "🔑": "🗝"});
    db.put("doc1", body.clone()).await.unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data, body);
}

#[tokio::test]
async fn field_names_with_dots_and_colons() {
    let db = Database::memory("test");

    let body = serde_json::json!({"field.with.dots": 1, "field:with:colons": 2});
    db.put("doc1", body.clone()).await.unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data, body);
}

// =========================================================================
// Attachment edge cases
// =========================================================================

/// Like CouchDB: a stale revision that has the attachment conflicts, and one
/// that does not have it yet reports the attachment as missing. Either way
/// nothing is removed.
#[tokio::test]
async fn remove_attachment_with_wrong_rev() {
    for b in backends("test") {
        let db = &b.db;
        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let r1 = r1.rev.unwrap();
        let r2 = db
            .put_attachment("doc1", "a.txt", &r1, b"hello".to_vec(), "text/plain")
            .await
            .unwrap()
            .rev
            .unwrap();
        let current = db
            .put_attachment("doc1", "b.txt", &r2, b"bye".to_vec(), "text/plain")
            .await
            .unwrap()
            .rev
            .unwrap();

        let stale = db.remove_attachment("doc1", "a.txt", &r2).await;
        assert!(
            matches!(stale, Err(RouchError::Conflict)),
            "{}: {stale:?}",
            b.name
        );
        let before = db.remove_attachment("doc1", "b.txt", &r1).await;
        assert!(
            matches!(before, Err(RouchError::NotFound(_))),
            "{}: {before:?}",
            b.name
        );

        assert_eq!(db.get_attachment("doc1", "a.txt").await.unwrap(), b"hello");
        assert_eq!(db.get_attachment("doc1", "b.txt").await.unwrap(), b"bye");
        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), current, "{}", b.name);
        assert_eq!(
            db.info().await.unwrap().update_seq,
            Seq::Num(3),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// Replication filter edge cases
// =========================================================================

#[tokio::test]
async fn replication_filter_empty_doc_ids() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source.put("a", serde_json::json!({})).await.unwrap();
    source.put("b", serde_json::json!({})).await.unwrap();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::DocIds(vec![])),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(
        result.docs_written, 0,
        "Empty filter should replicate nothing"
    );
    assert_eq!(target.info().await.unwrap().doc_count, 0);
}

#[tokio::test]
async fn replication_selector_filter_only_matching() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    let user = source
        .put("user1", serde_json::json!({"type": "user"}))
        .await
        .unwrap();
    source
        .put("inv1", serde_json::json!({"type": "invoice"}))
        .await
        .unwrap();

    let result = source
        .replicate_to_with_opts(
            &target,
            ReplicationOptions {
                filter: Some(ReplicationFilter::Selector(
                    serde_json::json!({"type": "user"}),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(result.ok);
    assert_eq!(result.docs_written, 1);
    let all = target.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(row_ids(&all), ["user1"]);
    assert_eq!(all.rows[0].value.rev, user.rev.unwrap());
    assert!(matches!(
        target.get("inv1").await,
        Err(RouchError::NotFound(_))
    ));
}

// =========================================================================
// Live changes: events after cancel
// =========================================================================

#[tokio::test]
async fn live_changes_events_cancel_stops_stream() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({})).await.unwrap();

    let (mut rx, handle) = db.live_changes_events(ChangesStreamOptions {
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // The first event is the change for the existing doc.
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Some(ChangesEvent::Change(change))) => assert_eq!(change.id, "doc1"),
        other => panic!("expected the doc1 change first, got {other:?}"),
    }

    handle.cancel();

    // After cancel the stream must report Complete and close the channel.
    // Events buffered before the cancel was observed (e.g. Paused) may still
    // arrive first, so drain until the channel closes.
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("channel should close after cancel");
    assert!(
        matches!(drained.last(), Some(ChangesEvent::Complete { .. })),
        "expected Complete as the last event, got {drained:?}"
    );
}

// =========================================================================
// Close then operate — should not panic
// =========================================================================

#[tokio::test]
async fn close_then_operations_behave_gracefully() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.close().await.unwrap();

    // close() is a no-op for the memory adapter: the data stays readable and
    // intact afterwards.
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 1);
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);
}

// =========================================================================
// Design docs: conflicts and updates of a given revision
// =========================================================================

fn empty_design(id: &str) -> DesignDocument {
    DesignDocument {
        id: id.into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    }
}

#[tokio::test]
async fn design_doc_update_requires_rev() {
    for b in backends("test") {
        let db = &b.db;
        let first = db.put_design(empty_design("_design/myapp")).await.unwrap();
        assert!(first.ok, "{}: {first:?}", b.name);

        // Putting it again without the revision is a conflict, reported as
        // an error like `put` (not `Ok` with `ok: false`), and changes
        // nothing.
        let result = db.put_design(empty_design("_design/myapp")).await;
        assert!(
            matches!(result, Err(RouchError::Conflict)),
            "{}: expected a conflict, got {result:?}",
            b.name
        );
        let stored = db.get_design("myapp").await.unwrap();
        assert_eq!(stored.rev, first.rev, "{}", b.name);
        assert_eq!(
            db.info().await.unwrap().update_seq,
            Seq::Num(1),
            "{}",
            b.name
        );
    }
}

/// What `put_design` removes through the struct stays removed: only fields
/// the struct cannot represent are carried over from the replaced revision.
#[tokio::test]
async fn put_design_update_drops_what_the_struct_removes() {
    let js = "function(doc) { emit(doc._id, 1); }";
    for b in backends("test") {
        let db = &b.db;
        let r1 = db
            .put(
                "_design/app",
                serde_json::json!({
                    "views": {
                        "counted": {"map": js, "reduce": "_count", "options": {"collation": "raw"}},
                        "by_type": {"map": {"fields": {"type": "asc"}}, "options": {"def": {"fields": ["type"]}}},
                        "lib": {"util": "exports.x = 1"}
                    },
                    "validate_doc_update": "function(newDoc) {}",
                    "custom": "kept"
                }),
            )
            .await
            .unwrap()
            .rev
            .unwrap();
        let mut ddoc = db.get_design("app").await.unwrap();
        assert_eq!(ddoc.rev.as_deref(), Some(r1.as_str()), "{}", b.name);
        // Drop the reduce and the validation function, and redefine the
        // Mango index view as a JavaScript view.
        ddoc.views.get_mut("counted").unwrap().reduce = None;
        ddoc.validate_doc_update = None;
        ddoc.views.insert(
            "by_type".into(),
            rouchdb::ViewDef {
                map: js.into(),
                reduce: None,
            },
        );
        let r2 = db.put_design(ddoc).await.unwrap().rev.unwrap();
        let stored = db
            .get_with_opts(
                "_design/app",
                GetOptions {
                    rev: Some(r2.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .data;
        assert_eq!(
            stored["views"],
            serde_json::json!({
                "counted": {"map": js, "options": {"collation": "raw"}},
                "by_type": {"map": js},
                "lib": {"util": "exports.x = 1"}
            }),
            "{}",
            b.name
        );
        assert!(
            stored.get("validate_doc_update").is_none(),
            "{}: {stored}",
            b.name
        );
        assert_eq!(stored["custom"], "kept", "{}", b.name);
    }
}

/// `put_design` carries over what `DesignDocument` does not model from the
/// revision it replaces, which is not always the winning one.
#[tokio::test]
async fn put_design_keeps_fields_of_the_revision_it_replaces() {
    for b in backends("test") {
        let db = &b.db;
        let views = serde_json::json!({"v": {"map": "function(doc) { emit(doc._id, 1); }"}});
        let r1 = db
            .put(
                "_design/app",
                serde_json::json!({"views": views, "custom": "base"}),
            )
            .await
            .unwrap()
            .rev
            .unwrap();
        let h1 = r1.split_once('-').unwrap().1.to_string();
        // Two sibling revisions: 2-aaa… loses to 2-fff….
        let mut revs = Vec::new();
        for (c, custom) in [('a', "loser"), ('f', "winner")] {
            let hash: String = std::iter::repeat_n(c, 32).collect();
            let doc = Document::from_json(serde_json::json!({
                "_id": "_design/app",
                "_rev": format!("2-{hash}"),
                "_revisions": {"start": 2, "ids": [hash, h1]},
                "views": views,
                "custom": custom,
            }))
            .unwrap();
            let results = db
                .bulk_docs(vec![doc], BulkDocsOptions::replication())
                .await
                .unwrap();
            assert!(results[0].ok, "{}: {results:?}", b.name);
            revs.push(format!("2-{hash}"));
        }
        let (loser, winner) = (&revs[0], &revs[1]);
        let current = db.get_design("app").await.unwrap();
        assert_eq!(current.rev.as_ref(), Some(winner), "{}", b.name);

        // Update the losing branch.
        let result = db
            .put_design(DesignDocument {
                rev: Some(loser.clone()),
                ..current
            })
            .await
            .unwrap();
        assert!(result.ok, "{}: {result:?}", b.name);
        let written = db
            .get_with_opts(
                "_design/app",
                GetOptions {
                    rev: result.rev.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(written.data["custom"], "loser", "{}", b.name);
        assert_eq!(written.data["views"], views, "{}", b.name);
    }
}

// =========================================================================
// Very long document IDs
// =========================================================================

#[tokio::test]
async fn very_long_document_id() {
    for b in backends("test") {
        let long_id: String = "x".repeat(5000);

        let r =
            b.db.put(&long_id, serde_json::json!({"v": 1}))
                .await
                .unwrap();
        assert_eq!(r.id, long_id);

        let doc = b.db.get(&long_id).await.unwrap();
        assert_eq!(doc.id, long_id, "{}", b.name);
        assert_eq!(doc.data["v"], 1, "{}", b.name);
    }
}

// =========================================================================
// Changes: descending order
// =========================================================================

#[tokio::test]
async fn changes_descending_returns_reverse_order() {
    for b in backends("test") {
        let db = &b.db;
        for id in ["a", "b", "c"] {
            db.put(id, serde_json::json!({})).await.unwrap();
        }

        let changes = db
            .changes(ChangesOptions {
                descending: true,
                ..Default::default()
            })
            .await
            .unwrap();

        // Newest first.
        let got: Vec<(&str, u64)> = changes
            .results
            .iter()
            .map(|c| (c.id.as_str(), c.seq.as_num()))
            .collect();
        assert_eq!(got, [("c", 3), ("b", 2), ("a", 1)], "{}", b.name);
    }
}
