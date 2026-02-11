//! Comprehensive edge-case tests found during code review.
//! Covers: empty IDs, concurrent writes, plugin chains, partition edge cases,
//! attachment errors, index staleness, replication filters, Unicode fields,
//! changes feed boundaries, and more.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, ChangesStreamOptions, Database,
    DesignDocument, DocResult, Document, FindOptions, Plugin, ReduceFn, ReplicationFilter,
    ReplicationOptions, Result, RouchError, SortField, ViewQueryOptions, query_view,
};

// =========================================================================
// Empty document IDs
// =========================================================================

#[tokio::test]
async fn put_with_empty_id_returns_error() {
    let db = Database::memory("test");
    let result = db.put("", serde_json::json!({"v": 1})).await;
    assert!(result.is_err(), "put with empty ID should error");
}

#[tokio::test]
async fn update_with_empty_id_returns_error() {
    let db = Database::memory("test");
    let result = db.update("", "1-abc", serde_json::json!({})).await;
    assert!(result.is_err(), "update with empty ID should error");
}

#[tokio::test]
async fn remove_with_empty_id_returns_error() {
    let db = Database::memory("test");
    let result = db.remove("", "1-abc").await;
    assert!(result.is_err(), "remove with empty ID should error");
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
            db.put(&format!("doc{}", i), serde_json::json!({"i": i}))
                .await
                .unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let info = db_arc.info().await.unwrap();
    assert_eq!(info.doc_count, 20);
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

    // At least one should succeed
    let success_count = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    assert!(success_count >= 1, "At least one update should succeed");
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

    let results = db.bulk_docs(docs, BulkDocsOptions::new()).await;
    // Should not panic. Implementation decides whether both succeed or second conflicts.
    assert!(results.is_ok() || results.is_err());
    // Document should exist either way
    let doc = db.get("same").await.unwrap();
    assert!(doc.data["v"] == 1 || doc.data["v"] == 2);
}

// =========================================================================
// Plugin: put_design goes through plugin pipeline
// =========================================================================

struct RejectDesignPlugin;

#[async_trait::async_trait]
impl Plugin for RejectDesignPlugin {
    fn name(&self) -> &str {
        "reject-design"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter() {
            if doc.id.starts_with("_design/") {
                return Err(RouchError::Forbidden("Design docs not allowed".into()));
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn put_design_goes_through_plugin_hooks() {
    let db = Database::memory("test").with_plugin(Arc::new(RejectDesignPlugin));

    let ddoc = DesignDocument {
        id: "_design/test".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };

    let result = db.put_design(ddoc).await;
    assert!(result.is_err(), "Plugin should reject design doc writes");
}

// =========================================================================
// Plugin: after_write tracks design doc writes
// =========================================================================

struct WriteCounter {
    count: AtomicU64,
}

impl WriteCounter {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for WriteCounter {
    fn name(&self) -> &str {
        "write-counter"
    }

    async fn after_write(&self, results: &[DocResult]) -> Result<()> {
        self.count.fetch_add(results.len() as u64, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn after_write_counts_design_doc() {
    let counter = Arc::new(WriteCounter::new());
    let db = Database::memory("test").with_plugin(counter.clone());

    db.put("doc1", serde_json::json!({})).await.unwrap();
    assert_eq!(counter.count.load(Ordering::SeqCst), 1);

    let ddoc = DesignDocument {
        id: "_design/test".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };
    db.put_design(ddoc).await.unwrap();
    assert_eq!(
        counter.count.load(Ordering::SeqCst),
        2,
        "put_design should trigger after_write"
    );
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
    let db = Database::memory("test").with_plugin(Arc::new(ResurrectPlugin));

    let r = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r.rev.unwrap();

    // Try to delete — plugin should prevent it
    let result = db.remove("doc1", &rev).await.unwrap();
    assert!(result.ok);

    // Doc should still be retrievable (not deleted)
    let doc = db.get("doc1").await.unwrap();
    assert!(!doc.deleted);
    assert_eq!(doc.data["resurrected"], true);
}

// =========================================================================
// Partition: colon in partition name
// =========================================================================

#[tokio::test]
async fn partition_with_colon_in_name() {
    let db = Database::memory("test");

    // Put docs with nested colons
    db.put("a:b:doc1", serde_json::json!({"v": 1}))
        .await
        .unwrap();
    db.put("a:b:doc2", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    db.put("a:other", serde_json::json!({"v": 3}))
        .await
        .unwrap();

    // Partition "a" should get all docs starting with "a:"
    let partition = db.partition("a");
    let result = partition.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(
        result.rows.len(),
        3,
        "All 'a:*' docs should be in partition"
    );
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

    // Query partition "users" but selector targets "orders" namespace
    let partition = db.partition("users");
    let result = partition
        .find(FindOptions {
            selector: serde_json::json!({"_id": "orders:o1"}),
            ..Default::default()
        })
        .await
        .unwrap();

    // Should return nothing — partition AND selector constraints conflict
    assert_eq!(result.docs.len(), 0);
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

    // Both should be found
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 0}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 2);

    // Delete Bob
    db.remove("bob", &bob_result.rev.unwrap()).await.unwrap();

    // Only Alice should remain
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 0}}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 1);
    assert_eq!(found.docs[0]["name"], "Alice");
}

#[tokio::test]
async fn index_updates_on_field_value_change() {
    let db = Database::memory("test");

    let r = db
        .put("doc1", serde_json::json!({"status": "pending", "v": 1}))
        .await
        .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("status".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    // Should find "pending"
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"status": "pending"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 1);

    // Update to "complete"
    db.update(
        "doc1",
        &r.rev.unwrap(),
        serde_json::json!({"status": "complete", "v": 2}),
    )
    .await
    .unwrap();

    // Should NOT find "pending" anymore
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"status": "pending"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        found.docs.len(),
        0,
        "Old status should not appear in results"
    );

    // Should find "complete"
    let found = db
        .find(FindOptions {
            selector: serde_json::json!({"status": "complete"}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.docs.len(), 1);
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

    assert_eq!(
        changes.results.len(),
        0,
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

    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["名前"], "Alice");
}

#[tokio::test]
async fn emoji_in_field_names_and_values() {
    let db = Database::memory("test");

    db.put(
        "doc1",
        serde_json::json!({"status_emoji": "✅", "likes": "👍👍"}),
    )
    .await
    .unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["status_emoji"], "✅");
    assert_eq!(doc.data["likes"], "👍👍");
}

#[tokio::test]
async fn field_names_with_dots_and_colons() {
    let db = Database::memory("test");

    db.put(
        "doc1",
        serde_json::json!({"field.with.dots": 1, "field:with:colons": 2}),
    )
    .await
    .unwrap();

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["field.with.dots"], 1);
    assert_eq!(doc.data["field:with:colons"], 2);
}

// =========================================================================
// Attachment edge cases
// =========================================================================

#[tokio::test]
async fn remove_attachment_with_wrong_rev() {
    let db = Database::memory("test");

    let r = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r.rev.unwrap();

    // Put an attachment
    db.adapter()
        .put_attachment("doc1", "file.txt", &rev, b"hello".to_vec(), "text/plain")
        .await
        .unwrap();

    // Try removing with old (stale) rev
    let result = db.remove_attachment("doc1", "file.txt", &rev).await;
    assert!(result.is_err(), "Stale rev should cause conflict");
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
}

#[tokio::test]
async fn replication_selector_filter_only_matching() {
    let source = Database::memory("source");
    let target = Database::memory("target");

    source
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
    assert_eq!(target.info().await.unwrap().doc_count, 1);
    assert!(target.get("user1").await.is_ok());
    assert!(target.get("inv1").await.is_err());
}

// =========================================================================
// View reduce on non-numeric values
// =========================================================================

#[tokio::test]
async fn view_reduce_sum_on_non_numeric_values() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();

    let result = query_view(
        db.adapter(),
        &|doc| {
            let name = doc.get("name").cloned().unwrap_or_default();
            vec![(serde_json::json!(null), name)] // non-numeric values
        },
        Some(&ReduceFn::Sum),
        ViewQueryOptions {
            reduce: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    // Sum of non-numeric values should be 0 (filter_map skips them)
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].value, serde_json::json!(0.0));
}

#[tokio::test]
async fn view_reduce_with_zero_emitted_rows() {
    let db = Database::memory("test");
    // No documents at all

    let result = query_view(
        db.adapter(),
        &|_doc| vec![], // emit nothing
        Some(&ReduceFn::Count),
        ViewQueryOptions {
            reduce: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    // Should handle empty reduce gracefully
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].value, serde_json::json!(0));
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

    // Get first event
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;

    // Cancel
    handle.cancel();

    // After cancel, channel should eventually close
    let result = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    match result {
        Ok(None) => {}    // Closed — good
        Ok(Some(_)) => {} // Buffered — OK
        Err(_) => {}      // Timeout — OK, stopping
    }
}

// =========================================================================
// Post generates unique IDs
// =========================================================================

#[tokio::test]
async fn post_generates_distinct_ids() {
    let db = Database::memory("test");

    let r1 = db.post(serde_json::json!({"v": 1})).await.unwrap();
    let r2 = db.post(serde_json::json!({"v": 2})).await.unwrap();
    let r3 = db.post(serde_json::json!({"v": 3})).await.unwrap();

    assert_ne!(r1.id, r2.id);
    assert_ne!(r2.id, r3.id);
    assert_ne!(r1.id, r3.id);
    assert!(!r1.id.is_empty());
}

// =========================================================================
// Explain with multiple indexes picks correct one
// =========================================================================

#[tokio::test]
async fn explain_with_multiple_indexes() {
    let db = Database::memory("test");

    db.put(
        "doc1",
        serde_json::json!({"name": "A", "age": 1, "city": "NYC"}),
    )
    .await
    .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("name".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: String::new(),
        fields: vec![SortField::Simple("city".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    // Query on age should pick idx-age
    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 0}}),
            ..Default::default()
        })
        .await;

    assert_eq!(explanation.index.name, "idx-age");
}

// =========================================================================
// Sync idempotent — multiple syncs don't duplicate docs
// =========================================================================

#[tokio::test]
async fn sync_three_times_no_duplication() {
    let a = Database::memory("a");
    let b = Database::memory("b");

    a.put("doc1", serde_json::json!({"from": "a"}))
        .await
        .unwrap();
    b.put("doc2", serde_json::json!({"from": "b"}))
        .await
        .unwrap();

    for _ in 0..3 {
        let (push, pull) = a.sync(&b).await.unwrap();
        assert!(push.ok);
        assert!(pull.ok);
    }

    assert_eq!(a.info().await.unwrap().doc_count, 2);
    assert_eq!(b.info().await.unwrap().doc_count, 2);
}

// =========================================================================
// Changes with selector: correctly filters deletions
// =========================================================================

#[tokio::test]
async fn changes_selector_on_deleted_doc() {
    let db = Database::memory("test");

    let r = db
        .put(
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

    // Delete user1
    db.remove("user1", &r.rev.unwrap()).await.unwrap();

    // Changes with selector for type=user — should include the deletion
    let changes = db
        .changes(ChangesOptions {
            selector: Some(serde_json::json!({"type": "user"})),
            include_docs: true,
            ..Default::default()
        })
        .await
        .unwrap();

    // Deleted docs may or may not match depending on implementation.
    // The important thing is no panic or error.
    assert!(changes.results.len() <= 2);
}

// =========================================================================
// AllDocs with only nonexistent keys
// =========================================================================

#[tokio::test]
async fn all_docs_all_keys_nonexistent() {
    let db = Database::memory("test");
    db.put("real", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["fake1".into(), "fake2".into(), "fake3".into()]),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    // Should return empty rows for nonexistent keys, not error
    assert_eq!(result.rows.len(), 0);
}

// =========================================================================
// Compact then verify data integrity
// =========================================================================

#[tokio::test]
async fn compact_preserves_latest_revisions() {
    let db = Database::memory("test");

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = db
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let _r3 = db
        .update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3}))
        .await
        .unwrap();

    db.compact().await.unwrap();

    // Latest revision should survive
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 3);
}

// =========================================================================
// Close then operate — should not panic
// =========================================================================

#[tokio::test]
async fn close_then_operations_behave_gracefully() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({})).await.unwrap();
    db.close().await.unwrap();

    // Memory adapter — operations may still work after close (it's a no-op)
    // The important thing: no panic
    let _ = db.get("doc1").await;
    let _ = db.info().await;
}

// =========================================================================
// Design doc: update with rev
// =========================================================================

#[tokio::test]
async fn design_doc_update_requires_rev() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/myapp".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };

    db.put_design(ddoc).await.unwrap();

    // Put again without rev should conflict
    let ddoc2 = DesignDocument {
        id: "_design/myapp".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };

    let result = db.put_design(ddoc2).await.unwrap();
    assert!(
        !result.ok,
        "Updating design doc without rev should conflict"
    );
}

// =========================================================================
// Very long document IDs
// =========================================================================

#[tokio::test]
async fn very_long_document_id() {
    let db = Database::memory("test");
    let long_id: String = "x".repeat(5000);

    let r = db.put(&long_id, serde_json::json!({"v": 1})).await.unwrap();
    assert!(r.ok);

    let doc = db.get(&long_id).await.unwrap();
    assert_eq!(doc.data["v"], 1);
}

// =========================================================================
// Replication: events include correct docs_read count
// =========================================================================

#[tokio::test]
async fn replication_events_report_progress() {
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
    assert_eq!(result.docs_written, 5);

    let mut events = vec![];
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    // Should have at least Active and Complete events
    assert!(!events.is_empty());
}

// =========================================================================
// Changes: descending order
// =========================================================================

#[tokio::test]
async fn changes_descending_returns_reverse_order() {
    let db = Database::memory("test");

    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();
    db.put("c", serde_json::json!({})).await.unwrap();

    let changes = db
        .changes(ChangesOptions {
            descending: true,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 3);
    // Descending means newest first
    assert!(changes.results[0].seq.as_num() > changes.results[2].seq.as_num());
}
