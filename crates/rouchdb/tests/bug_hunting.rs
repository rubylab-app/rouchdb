//! Bug-hunting tests: edge cases, boundary conditions, and regression tests.
//! These tests are designed to expose bugs in the implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, ChangesStreamOptions, Database,
    DesignDocument, Document, FindOptions, Plugin, Result, Revision, SecurityDocument,
    SecurityGroup, SortField, ViewDef, ViewEngine, ViewQueryOptions, query_view,
};

// =========================================================================
// BUG: Partition find() doesn't escape regex metacharacters
// =========================================================================

#[tokio::test]
async fn partition_name_with_regex_metacharacters() {
    let db = Database::memory("test");

    // Put docs with different prefixes
    db.put("user.test:doc1", serde_json::json!({"type": "a"}))
        .await
        .unwrap();
    db.put("userXtest:doc2", serde_json::json!({"type": "b"}))
        .await
        .unwrap();
    db.put("user_test:doc3", serde_json::json!({"type": "c"}))
        .await
        .unwrap();

    // "user.test" as partition name — the "." is a regex metacharacter
    let partition = db.partition("user.test");
    let result = partition
        .find(FindOptions {
            selector: serde_json::json!({"type": {"$exists": true}}),
            ..Default::default()
        })
        .await
        .unwrap();

    // Should ONLY match "user.test:" prefix, not "userXtest:"
    // Because "." in regex matches any character
    for doc in &result.docs {
        let id = doc["_id"].as_str().unwrap();
        assert!(
            id.starts_with("user.test:"),
            "Partition find should only return docs with exact prefix, got: {}",
            id
        );
    }
}

// =========================================================================
// BUG: Partition all_docs() can escape partition boundaries
// =========================================================================

#[tokio::test]
async fn partition_all_docs_enforces_boundaries() {
    let db = Database::memory("test");

    db.put("users:alice", serde_json::json!({"type": "user"}))
        .await
        .unwrap();
    db.put("users:bob", serde_json::json!({"type": "user"}))
        .await
        .unwrap();
    db.put("zzz:other", serde_json::json!({"type": "other"}))
        .await
        .unwrap();

    let partition = db.partition("users");

    // User tries to escape partition by providing start_key outside bounds
    let result = partition
        .all_docs(AllDocsOptions {
            start_key: Some("a".to_string()), // before "users:"
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    // Should still only return docs from users partition
    for row in &result.rows {
        assert!(
            row.id.starts_with("users:"),
            "Partition should enforce boundaries, got: {}",
            row.id
        );
    }
}

// =========================================================================
// BUG: Empty partition name
// =========================================================================

#[tokio::test]
async fn partition_empty_name() {
    let db = Database::memory("test");
    db.put(":doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("normal_doc", serde_json::json!({"v": 2}))
        .await
        .unwrap();

    let partition = db.partition("");
    let result = partition.all_docs(AllDocsOptions::new()).await.unwrap();

    // Empty partition name creates ":" prefix
    // This should only match docs starting with ":"
    for row in &result.rows {
        assert!(
            row.id.starts_with(':'),
            "Empty partition should match ':' prefix, got: {}",
            row.id
        );
    }
}

// =========================================================================
// BUG: Document from_json/to_json roundtrip loses _attachments
// =========================================================================

#[tokio::test]
async fn document_roundtrip_preserves_attachments() {
    let json = serde_json::json!({
        "_id": "doc1",
        "_attachments": {
            "file.txt": {
                "content_type": "text/plain",
                "digest": "md5-abc123",
                "length": 5,
                "stub": true
            }
        },
        "name": "test"
    });

    let doc = Document::from_json(json).unwrap();
    assert_eq!(doc.attachments.len(), 1);
    assert!(doc.attachments.contains_key("file.txt"));

    // Roundtrip
    let json_out = doc.to_json();
    let doc2 = Document::from_json(json_out).unwrap();
    assert_eq!(doc2.attachments.len(), 1);
    assert!(doc2.attachments.contains_key("file.txt"));
    assert_eq!(doc2.attachments["file.txt"].content_type, "text/plain");
}

#[tokio::test]
async fn document_from_json_with_empty_attachments() {
    let json = serde_json::json!({
        "_id": "doc1",
        "_attachments": {},
        "name": "test"
    });

    let doc = Document::from_json(json).unwrap();
    assert!(doc.attachments.is_empty());
}

#[tokio::test]
async fn document_from_json_without_id() {
    let json = serde_json::json!({"name": "test"});
    let doc = Document::from_json(json).unwrap();
    assert!(doc.id.is_empty());
}

#[tokio::test]
async fn document_from_json_non_object() {
    let json = serde_json::json!("just a string");
    let result = Document::from_json(json);
    assert!(result.is_err());
}

#[tokio::test]
async fn document_from_json_with_invalid_rev() {
    let json = serde_json::json!({
        "_id": "doc1",
        "_rev": "not-a-valid-rev"
    });
    let result = Document::from_json(json);
    assert!(result.is_err());
}

#[tokio::test]
async fn document_to_json_preserves_all_fields() {
    let doc = Document {
        id: "doc1".into(),
        rev: Some(Revision::new(1, "abc".into())),
        deleted: false,
        data: serde_json::json!({"name": "Alice", "nested": {"a": 1}}),
        attachments: HashMap::new(),
    };

    let json = doc.to_json();
    assert_eq!(json["_id"], "doc1");
    assert_eq!(json["_rev"], "1-abc");
    assert_eq!(json["name"], "Alice");
    assert_eq!(json["nested"]["a"], 1);
    assert!(json.get("_deleted").is_none());
}

#[tokio::test]
async fn document_to_json_deleted_doc() {
    let doc = Document {
        id: "doc1".into(),
        rev: Some(Revision::new(2, "def".into())),
        deleted: true,
        data: serde_json::json!({}),
        attachments: HashMap::new(),
    };

    let json = doc.to_json();
    assert_eq!(json["_deleted"], true);
}

// =========================================================================
// BUG: Purge doesn't clean rev_tree
// =========================================================================

#[tokio::test]
async fn purge_then_get_returns_not_found() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev = r1.rev.unwrap();

    db.purge("doc1", vec![rev]).await.unwrap();

    // After purge, doc should be completely gone
    let result = db.get("doc1").await;
    assert!(result.is_err(), "Purged doc should not be retrievable");
}

#[tokio::test]
async fn purge_then_changes_excludes_purged() {
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

    db.purge("doc1", vec![r1.rev.unwrap()]).await.unwrap();

    let changes = db.changes(ChangesOptions::default()).await.unwrap();
    let ids: Vec<&str> = changes.results.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !ids.contains(&"doc1"),
        "Purged doc should not appear in changes"
    );
    assert!(ids.contains(&"doc2"));
}

#[tokio::test]
async fn purge_partial_revs() {
    let db = Database::memory("test");

    // Create doc with two revisions
    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let rev1 = r1.rev.unwrap();
    let r2 = db
        .update("doc1", &rev1, serde_json::json!({"v": 2}))
        .await
        .unwrap();
    let _rev2 = r2.rev.unwrap();

    // Purge only the first revision — doc should still exist with rev2
    let purge_result = db.purge("doc1", vec![rev1]).await.unwrap();
    assert!(purge_result.purged.contains_key("doc1"));

    // Doc should still be accessible via latest rev
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 2);
}

// =========================================================================
// BUG: ViewQueryOptions descending with start/end key
// =========================================================================

#[tokio::test]
async fn view_descending_with_key_range() {
    let db = Database::memory("test");

    for i in 0..10 {
        db.put(&format!("doc{}", i), serde_json::json!({"n": i}))
            .await
            .unwrap();
    }

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["n"].clone(), serde_json::json!(1))]
    };

    // Descending order with start_key=7, end_key=3
    // Should return 7, 6, 5, 4, 3
    let results = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            descending: true,
            start_key: Some(serde_json::json!(7)),
            end_key: Some(serde_json::json!(3)),
            inclusive_end: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        results.rows.len(),
        5,
        "Descending range 7..=3 should have 5 rows"
    );

    // Verify order is descending
    let keys: Vec<i64> = results
        .rows
        .iter()
        .map(|r| r.key.as_i64().unwrap())
        .collect();
    assert_eq!(keys, vec![7, 6, 5, 4, 3]);
}

#[tokio::test]
async fn view_descending_without_range() {
    let db = Database::memory("test");

    db.put("a", serde_json::json!({"n": 1})).await.unwrap();
    db.put("b", serde_json::json!({"n": 2})).await.unwrap();
    db.put("c", serde_json::json!({"n": 3})).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["n"].clone(), serde_json::json!(null))]
    };

    let results = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            descending: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    let keys: Vec<i64> = results
        .rows
        .iter()
        .map(|r| r.key.as_i64().unwrap())
        .collect();
    assert_eq!(keys, vec![3, 2, 1]);
}

// =========================================================================
// Plugin: multiple plugins, first modifies then second rejects
// =========================================================================

struct AddFieldPlugin {
    field: String,
    value: serde_json::Value,
}

#[async_trait::async_trait]
impl Plugin for AddFieldPlugin {
    fn name(&self) -> &str {
        "add-field"
    }
    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter_mut() {
            if let serde_json::Value::Object(ref mut map) = doc.data {
                map.insert(self.field.clone(), self.value.clone());
            }
        }
        Ok(())
    }
}

struct RejectPlugin;

#[async_trait::async_trait]
impl Plugin for RejectPlugin {
    fn name(&self) -> &str {
        "reject"
    }
    async fn before_write(&self, _docs: &mut Vec<Document>) -> Result<()> {
        Err(rouchdb::RouchError::BadRequest("rejected".into()))
    }
}

#[tokio::test]
async fn plugin_first_modifies_second_rejects_no_write() {
    let db = Database::memory("test")
        .with_plugin(Arc::new(AddFieldPlugin {
            field: "modified".into(),
            value: serde_json::json!(true),
        }))
        .with_plugin(Arc::new(RejectPlugin));

    let result = db.put("doc1", serde_json::json!({"v": 1})).await;
    assert!(result.is_err());

    // Doc should NOT have been written
    let get_result = db.get("doc1").await;
    assert!(get_result.is_err(), "Rejected doc should not be stored");
}

#[tokio::test]
async fn plugin_before_write_called_on_update() {
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter_clone = counter.clone();

    struct WriteCountPlugin(Arc<std::sync::atomic::AtomicU64>);

    #[async_trait::async_trait]
    impl Plugin for WriteCountPlugin {
        fn name(&self) -> &str {
            "write-count"
        }
        async fn before_write(&self, _docs: &mut Vec<Document>) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    let db = Database::memory("test").with_plugin(Arc::new(WriteCountPlugin(counter_clone)));

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

    db.update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn plugin_before_write_called_on_remove() {
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter_clone = counter.clone();

    struct WriteCountPlugin2(Arc<std::sync::atomic::AtomicU64>);

    #[async_trait::async_trait]
    impl Plugin for WriteCountPlugin2 {
        fn name(&self) -> &str {
            "write-count"
        }
        async fn before_write(&self, _docs: &mut Vec<Document>) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    let db = Database::memory("test").with_plugin(Arc::new(WriteCountPlugin2(counter_clone)));

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    db.remove("doc1", &r1.rev.unwrap()).await.unwrap();
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
}

// =========================================================================
// Changes feed: selector with all results filtered should still advance seq
// =========================================================================

#[tokio::test]
async fn changes_selector_advances_last_seq_even_when_all_filtered() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"type": "x"})).await.unwrap();
    db.put("b", serde_json::json!({"type": "x"})).await.unwrap();

    // Selector matches nothing — but last_seq should still advance
    let changes = db
        .changes(ChangesOptions {
            selector: Some(serde_json::json!({"type": "nonexistent"})),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(changes.results.len(), 0);
    // last_seq should NOT be the default — it should reflect the adapter's state
    assert_ne!(
        changes.last_seq,
        rouchdb::Seq::default(),
        "last_seq should advance even when all changes are filtered"
    );
}

// =========================================================================
// allDocs: edge cases
// =========================================================================

#[tokio::test]
async fn all_docs_skip_beyond_total() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            skip: 100,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 0);
    assert_eq!(result.total_rows, 2);
}

#[tokio::test]
async fn all_docs_limit_zero() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            limit: Some(0),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 0);
}

#[tokio::test]
async fn all_docs_descending() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();
    db.put("c", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            descending: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["c", "b", "a"]);
}

#[tokio::test]
async fn all_docs_start_end_key_range() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();
    db.put("b", serde_json::json!({})).await.unwrap();
    db.put("c", serde_json::json!({})).await.unwrap();
    db.put("d", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            start_key: Some("b".into()),
            end_key: Some("c".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "c"]);
}

#[tokio::test]
async fn all_docs_with_nonexistent_keys() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["a".into(), "nonexistent".into()]),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    // Should return what exists
    assert!(result.rows.iter().any(|r| r.id == "a"));
}

// =========================================================================
// Design doc: roundtrip via put/get preserves all fields
// =========================================================================

#[tokio::test]
async fn design_doc_full_roundtrip() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/full".into(),
        rev: None,
        views: {
            let mut v = HashMap::new();
            v.insert(
                "by_type".into(),
                ViewDef {
                    map: "function(doc) { emit(doc.type, 1); }".into(),
                    reduce: Some("_count".into()),
                },
            );
            v.insert(
                "by_name".into(),
                ViewDef {
                    map: "function(doc) { emit(doc.name, null); }".into(),
                    reduce: None,
                },
            );
            v
        },
        filters: {
            let mut f = HashMap::new();
            f.insert("my_filter".into(), "function(doc) { return true; }".into());
            f
        },
        validate_doc_update: Some("function(n,o,u) {}".into()),
        shows: {
            let mut s = HashMap::new();
            s.insert("detail".into(), "function(doc,req) {}".into());
            s
        },
        lists: {
            let mut l = HashMap::new();
            l.insert("all".into(), "function(head,req) {}".into());
            l
        },
        updates: {
            let mut u = HashMap::new();
            u.insert("bump".into(), "function(doc,req) {}".into());
            u
        },
        language: Some("javascript".into()),
    };

    db.put_design(ddoc.clone()).await.unwrap();

    let retrieved = db.get_design("full").await.unwrap();
    assert_eq!(retrieved.views.len(), 2);
    assert_eq!(retrieved.filters.len(), 1);
    assert!(retrieved.validate_doc_update.is_some());
    assert_eq!(retrieved.shows.len(), 1);
    assert_eq!(retrieved.lists.len(), 1);
    assert_eq!(retrieved.updates.len(), 1);
    assert_eq!(retrieved.language, Some("javascript".into()));

    // Verify individual fields survived the roundtrip
    assert_eq!(retrieved.views["by_type"].reduce, Some("_count".into()));
    assert_eq!(retrieved.views["by_name"].reduce, None);
}

// =========================================================================
// Mango find: edge cases
// =========================================================================

#[tokio::test]
async fn find_with_empty_selector_matches_all() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"v": 1})).await.unwrap();
    db.put("b", serde_json::json!({"v": 2})).await.unwrap();

    let result = db
        .find(FindOptions {
            selector: serde_json::json!({}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);
}

#[tokio::test]
async fn find_with_sort_and_limit() {
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"name": "Zara", "age": 20}))
        .await
        .unwrap();
    db.put("b", serde_json::json!({"name": "Alice", "age": 30}))
        .await
        .unwrap();
    db.put("c", serde_json::json!({"name": "Bob", "age": 25}))
        .await
        .unwrap();

    let result = db
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$exists": true}}),
            sort: Some(vec![SortField::Simple("name".into())]),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);
    assert_eq!(result.docs[0]["name"], "Alice");
    assert_eq!(result.docs[1]["name"], "Bob");
}

#[tokio::test]
async fn find_with_skip() {
    let db = Database::memory("test");
    for i in 0..5 {
        db.put(&format!("doc{}", i), serde_json::json!({"i": i}))
            .await
            .unwrap();
    }

    let result = db
        .find(FindOptions {
            selector: serde_json::json!({"i": {"$exists": true}}),
            skip: Some(3),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 2);
}

#[tokio::test]
async fn find_with_fields_projection() {
    let db = Database::memory("test");
    db.put(
        "a",
        serde_json::json!({"name": "Alice", "age": 30, "email": "alice@example.com"}),
    )
    .await
    .unwrap();

    let result = db
        .find(FindOptions {
            selector: serde_json::json!({"name": "Alice"}),
            fields: Some(vec!["name".into(), "age".into()]),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Alice");
    assert_eq!(result.docs[0]["age"], 30);
    // email should NOT be present
    assert!(result.docs[0].get("email").is_none());
}

// =========================================================================
// Security: update and verify
// =========================================================================

#[tokio::test]
async fn security_overwrite() {
    let db = Database::memory("test");

    let sec1 = SecurityDocument {
        admins: SecurityGroup {
            names: vec!["admin1".into()],
            roles: vec![],
        },
        members: SecurityGroup::default(),
    };
    db.put_security(sec1).await.unwrap();

    let sec2 = SecurityDocument {
        admins: SecurityGroup {
            names: vec!["admin2".into()],
            roles: vec!["role1".into()],
        },
        members: SecurityGroup::default(),
    };
    db.put_security(sec2).await.unwrap();

    let fetched = db.get_security().await.unwrap();
    assert_eq!(fetched.admins.names, vec!["admin2"]);
    assert_eq!(fetched.admins.roles, vec!["role1"]);
}

// =========================================================================
// Replication: sync with concurrent modifications
// =========================================================================

#[tokio::test]
async fn sync_idempotent() {
    let a = Database::memory("a");
    let b = Database::memory("b");

    a.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    // Sync twice — second should be a no-op
    a.sync(&b).await.unwrap();
    let (push, pull) = a.sync(&b).await.unwrap();
    assert!(push.ok);
    assert!(pull.ok);
    assert_eq!(push.docs_written, 0);
}

// =========================================================================
// Live changes: adding docs during streaming
// =========================================================================

#[tokio::test]
async fn live_changes_picks_up_docs_added_after_start() {
    let db = Database::memory("test");

    let (mut rx, handle) = db.live_changes(ChangesStreamOptions {
        poll_interval: Duration::from_millis(50),
        ..Default::default()
    });

    // Add a doc after the live stream started
    db.put("late_doc", serde_json::json!({"v": 1}))
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.id, "late_doc");

    handle.cancel();
}

// =========================================================================
// ViewEngine: multiple emits per document
// =========================================================================

#[tokio::test]
async fn view_engine_multiple_emits_per_doc() {
    let db = Database::memory("test");

    db.put(
        "doc1",
        serde_json::json!({"tags": ["rust", "db", "local-first"]}),
    )
    .await
    .unwrap();

    let mut engine = ViewEngine::new();
    engine.register_map("app", "by_tag", |doc| {
        let mut emitted = vec![];
        if let Some(tags) = doc.get("tags").and_then(|t| t.as_array()) {
            for tag in tags {
                emitted.push((tag.clone(), serde_json::json!(1)));
            }
        }
        emitted
    });

    engine
        .update_index(db.adapter(), "app", "by_tag")
        .await
        .unwrap();

    let index = engine.get_index("app", "by_tag").unwrap();
    // doc1 should have 3 emitted pairs
    assert_eq!(index.entries["doc1"].len(), 3);
}

// =========================================================================
// Conflicts: creating and querying
// =========================================================================

#[tokio::test]
async fn create_conflict_via_bulk_docs() {
    let db = Database::memory("test");

    // Create initial doc
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    // Force a conflicting revision via replication mode
    let conflict_doc = Document {
        id: "doc1".into(),
        rev: Some(Revision::new(1, "conflicting_hash".into())),
        deleted: false,
        data: serde_json::json!({"v": "conflict"}),
        attachments: HashMap::new(),
    };

    db.bulk_docs(vec![conflict_doc], BulkDocsOptions::replication())
        .await
        .unwrap();

    // Get the doc — should get the winning revision
    let doc = db.get("doc1").await.unwrap();
    assert!(doc.data.get("v").is_some());
}

// =========================================================================
// Edge case: put with very large document
// =========================================================================

#[tokio::test]
async fn put_large_document() {
    let db = Database::memory("test");

    let large_array: Vec<i64> = (0..10000).collect();
    let result = db
        .put("large", serde_json::json!({"data": large_array}))
        .await
        .unwrap();
    assert!(result.ok);

    let doc = db.get("large").await.unwrap();
    assert_eq!(doc.data["data"].as_array().unwrap().len(), 10000);
}

// =========================================================================
// Edge case: special characters in document IDs
// =========================================================================

#[tokio::test]
async fn special_characters_in_doc_id() {
    let db = Database::memory("test");

    let ids = vec![
        "doc with spaces",
        "doc/with/slashes",
        "doc-with-dashes",
        "doc_with_underscores",
        "123numeric",
        "UPPERCASE",
    ];

    for id in &ids {
        let result = db.put(id, serde_json::json!({"id": id})).await.unwrap();
        assert!(result.ok, "Failed to put doc with id: {}", id);

        let doc = db.get(id).await.unwrap();
        assert_eq!(doc.data["id"], *id);
    }
}

// =========================================================================
// Edge case: empty database operations
// =========================================================================

#[tokio::test]
async fn operations_on_empty_db() {
    let db = Database::memory("test");

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);

    let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(all.rows.len(), 0);

    let changes = db.changes(ChangesOptions::default()).await.unwrap();
    assert_eq!(changes.results.len(), 0);

    let find = db
        .find(FindOptions {
            selector: serde_json::json!({}),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(find.docs.len(), 0);
}

// =========================================================================
// Edge case: update with wrong rev
// =========================================================================

#[tokio::test]
async fn update_with_wrong_rev_fails() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db
        .update("doc1", "1-wronghash", serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(!result.ok, "Update with wrong rev should fail");
}

#[tokio::test]
async fn remove_with_wrong_rev_fails() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db.remove("doc1", "1-wronghash").await.unwrap();
    assert!(!result.ok, "Remove with wrong rev should fail");
}

// =========================================================================
// Explain: with multiple indexes, picks the right one
// =========================================================================

#[tokio::test]
async fn explain_picks_matching_index() {
    let db = Database::memory("test");

    db.put(
        "a",
        serde_json::json!({"name": "Alice", "age": 30, "city": "NYC"}),
    )
    .await
    .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: "".into(),
        fields: vec![SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    db.create_index(rouchdb::IndexDefinition {
        name: "".into(),
        fields: vec![SortField::Simple("city".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    // Query on age — should pick age index
    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"age": {"$gt": 20}}),
            ..Default::default()
        })
        .await;
    assert_eq!(explanation.index.name, "idx-age");

    // Query on city — should pick city index
    let explanation = db
        .explain(FindOptions {
            selector: serde_json::json!({"city": "NYC"}),
            ..Default::default()
        })
        .await;
    assert_eq!(explanation.index.name, "idx-city");
}
