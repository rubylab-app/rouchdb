//! Tests for Plugin architecture and Partitioned databases:
//! - Plugin trait (before_write, after_write, on_destroy)
//! - Multiple plugins
//! - Partition scoped queries (all_docs, find, get, put)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rouchdb::{AllDocsOptions, Database, DocResult, Document, FindOptions, Plugin, Result};

// =========================================================================
// Plugin: before_write hook
// =========================================================================

struct TimestampPlugin;

#[async_trait::async_trait]
impl Plugin for TimestampPlugin {
    fn name(&self) -> &str {
        "timestamp"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter_mut() {
            if let serde_json::Value::Object(ref mut map) = doc.data {
                map.insert(
                    "created_at".to_string(),
                    serde_json::json!("2026-02-10T00:00:00Z"),
                );
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn plugin_before_write_modifies_docs() {
    let db = Database::memory("test").with_plugin(Arc::new(TimestampPlugin));

    let result = db
        .put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    assert!(result.ok);

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["name"], "Alice");
    assert_eq!(doc.data["created_at"], "2026-02-10T00:00:00Z");
}

// =========================================================================
// Plugin: after_write hook
// =========================================================================

struct CountPlugin {
    write_count: AtomicU64,
}

impl CountPlugin {
    fn new() -> Self {
        Self {
            write_count: AtomicU64::new(0),
        }
    }

    fn count(&self) -> u64 {
        self.write_count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Plugin for CountPlugin {
    fn name(&self) -> &str {
        "counter"
    }

    async fn after_write(&self, results: &[DocResult]) -> Result<()> {
        let successful = results.iter().filter(|r| r.ok).count() as u64;
        self.write_count.fetch_add(successful, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn plugin_after_write_tracks_count() {
    let counter = Arc::new(CountPlugin::new());
    let db = Database::memory("test").with_plugin(counter.clone());

    db.put("doc1", serde_json::json!({})).await.unwrap();
    assert_eq!(counter.count(), 1);

    db.put("doc2", serde_json::json!({})).await.unwrap();
    assert_eq!(counter.count(), 2);

    // bulk_docs
    let docs = vec![
        Document {
            id: "doc3".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        },
        Document {
            id: "doc4".into(),
            rev: None,
            deleted: false,
            data: serde_json::json!({}),
            attachments: HashMap::new(),
        },
    ];
    db.bulk_docs(docs, rouchdb::BulkDocsOptions::new())
        .await
        .unwrap();
    assert_eq!(counter.count(), 4);
}

// =========================================================================
// Plugin: on_destroy hook
// =========================================================================

struct DestroyPlugin {
    destroyed: AtomicBool,
}

impl DestroyPlugin {
    fn new() -> Self {
        Self {
            destroyed: AtomicBool::new(false),
        }
    }

    fn was_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Plugin for DestroyPlugin {
    fn name(&self) -> &str {
        "destroy-tracker"
    }

    async fn on_destroy(&self) -> Result<()> {
        self.destroyed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn plugin_on_destroy_called() {
    let destroy_plugin = Arc::new(DestroyPlugin::new());
    let db = Database::memory("test").with_plugin(destroy_plugin.clone());

    db.put("doc1", serde_json::json!({})).await.unwrap();

    assert!(!destroy_plugin.was_destroyed());
    db.destroy().await.unwrap();
    assert!(destroy_plugin.was_destroyed());
}

// =========================================================================
// Multiple plugins
// =========================================================================

#[tokio::test]
async fn multiple_plugins_all_called() {
    let counter = Arc::new(CountPlugin::new());
    let destroy_tracker = Arc::new(DestroyPlugin::new());

    let db = Database::memory("test")
        .with_plugin(Arc::new(TimestampPlugin))
        .with_plugin(counter.clone())
        .with_plugin(destroy_tracker.clone());

    db.put("doc1", serde_json::json!({"name": "Test"}))
        .await
        .unwrap();

    // TimestampPlugin should have added created_at
    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["created_at"], "2026-02-10T00:00:00Z");

    // CountPlugin should have counted
    assert_eq!(counter.count(), 1);

    // DestroyPlugin should trigger on destroy
    db.destroy().await.unwrap();
    assert!(destroy_tracker.was_destroyed());
}

// =========================================================================
// Plugin: before_write can reject writes
// =========================================================================

struct ValidationPlugin;

#[async_trait::async_trait]
impl Plugin for ValidationPlugin {
    fn name(&self) -> &str {
        "validation"
    }

    async fn before_write(&self, docs: &mut Vec<Document>) -> Result<()> {
        for doc in docs.iter() {
            if doc.data.get("name").is_none() && !doc.deleted {
                return Err(rouchdb::RouchError::BadRequest(
                    "name field is required".into(),
                ));
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn plugin_validation_rejects_invalid_docs() {
    let db = Database::memory("test").with_plugin(Arc::new(ValidationPlugin));

    // Valid doc — has name
    let result = db.put("doc1", serde_json::json!({"name": "Alice"})).await;
    assert!(result.is_ok());

    // Invalid doc — no name field
    let result = db.put("doc2", serde_json::json!({"age": 25})).await;
    assert!(result.is_err());
}

// =========================================================================
// Partitioned databases
// =========================================================================

#[tokio::test]
async fn partition_put_and_get() {
    let db = Database::memory("test");
    let partition = db.partition("users");

    let result = partition
        .put("alice", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.id, "users:alice");

    let doc = partition.get("alice").await.unwrap();
    assert_eq!(doc.data["name"], "Alice");
    assert_eq!(doc.id, "users:alice");

    // Also accessible from the main db with full ID
    let doc2 = db.get("users:alice").await.unwrap();
    assert_eq!(doc2.data["name"], "Alice");
}

#[tokio::test]
async fn partition_put_with_full_id() {
    let db = Database::memory("test");
    let partition = db.partition("products");

    // Put with full partition prefix
    partition
        .put("products:widget", serde_json::json!({"name": "Widget"}))
        .await
        .unwrap();

    let doc = partition.get("widget").await.unwrap();
    assert_eq!(doc.id, "products:widget");
    assert_eq!(doc.data["name"], "Widget");
}

#[tokio::test]
async fn partition_all_docs() {
    let db = Database::memory("test");

    // Put docs in different partitions
    db.put("users:alice", serde_json::json!({"type": "user"}))
        .await
        .unwrap();
    db.put("users:bob", serde_json::json!({"type": "user"}))
        .await
        .unwrap();
    db.put("orders:o1", serde_json::json!({"type": "order"}))
        .await
        .unwrap();
    db.put("orders:o2", serde_json::json!({"type": "order"}))
        .await
        .unwrap();
    db.put("orders:o3", serde_json::json!({"type": "order"}))
        .await
        .unwrap();

    let users_partition = db.partition("users");
    let user_docs = users_partition
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(user_docs.rows.len(), 2);

    let orders_partition = db.partition("orders");
    let order_docs = orders_partition
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(order_docs.rows.len(), 3);
}

#[tokio::test]
async fn partition_find() {
    let db = Database::memory("test");

    db.put(
        "users:alice",
        serde_json::json!({"type": "user", "name": "Alice", "age": 30}),
    )
    .await
    .unwrap();
    db.put(
        "users:bob",
        serde_json::json!({"type": "user", "name": "Bob", "age": 25}),
    )
    .await
    .unwrap();
    db.put(
        "orders:o1",
        serde_json::json!({"type": "order", "amount": 100}),
    )
    .await
    .unwrap();

    let users = db.partition("users");
    let result = users
        .find(FindOptions {
            selector: serde_json::json!({"age": {"$gte": 28}}),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(result.docs.len(), 1);
    assert_eq!(result.docs[0]["name"], "Alice");
}

#[tokio::test]
async fn partition_get_nonexistent() {
    let db = Database::memory("test");
    let partition = db.partition("users");

    let err = partition.get("nonexistent").await;
    assert!(err.is_err());
}

#[tokio::test]
async fn partition_isolation() {
    let db = Database::memory("test");

    // Put same short ID in different partitions
    db.put("team_a:doc1", serde_json::json!({"team": "A"}))
        .await
        .unwrap();
    db.put("team_b:doc1", serde_json::json!({"team": "B"}))
        .await
        .unwrap();

    let team_a = db.partition("team_a");
    let doc = team_a.get("doc1").await.unwrap();
    assert_eq!(doc.data["team"], "A");

    let team_b = db.partition("team_b");
    let doc = team_b.get("doc1").await.unwrap();
    assert_eq!(doc.data["team"], "B");
}

// =========================================================================
// Database constructors
// =========================================================================

#[tokio::test]
async fn from_adapter_works() {
    let adapter = Arc::new(rouchdb::MemoryAdapter::new("custom"));
    let db = Database::from_adapter(adapter);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);
    assert_eq!(info.db_name, "custom");
}

#[tokio::test]
async fn redb_adapter_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");

    // Write data
    {
        let db = Database::open(&path, "persist_test").unwrap();
        db.put("doc1", serde_json::json!({"persistent": true}))
            .await
            .unwrap();
    }

    // Reopen and verify
    {
        let db = Database::open(&path, "persist_test").unwrap();
        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.data["persistent"], true);
    }
}
