//! Tests for Plugin architecture and Partitioned databases:
//! - Plugins that change, count or validate documents
//! - Partition scoped queries (find, get)
//!
//! The ordering and error contract of plugins is in plugin_contract.rs and
//! the partition query contract in partition.rs.

mod backends;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use backends::{Backend, KINDS};
use rouchdb::{
    BulkDocsOptions, Database, DocResult, Document, FindOptions, Plugin, Result, RouchError, Seq,
};

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
    for kind in KINDS {
        let b =
            Backend::open(kind, "test").configure(|db| db.with_plugin(Arc::new(TimestampPlugin)));

        let result =
            b.db.put("doc1", serde_json::json!({"name": "Alice"}))
                .await
                .unwrap();
        assert!(result.ok);

        let doc = b.db.get("doc1").await.unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), result.rev.unwrap(), "{kind}");
        assert_eq!(
            doc.data,
            serde_json::json!({"name": "Alice", "created_at": "2026-02-10T00:00:00Z"}),
            "{kind}"
        );
    }
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

fn new_doc(id: &str) -> Document {
    Document {
        id: id.into(),
        rev: None,
        deleted: false,
        data: serde_json::json!({}),
        attachments: HashMap::new(),
    }
}

#[tokio::test]
async fn plugin_after_write_sees_every_write_result() {
    for kind in KINDS {
        let counter = Arc::new(CountPlugin::new());
        let b = Backend::open(kind, "test").configure(|db| db.with_plugin(counter.clone()));
        let db = &b.db;

        db.put("doc1", serde_json::json!({})).await.unwrap();
        assert_eq!(counter.count(), 1, "{kind}");

        let r2 = db.put("doc2", serde_json::json!({})).await.unwrap();
        assert_eq!(counter.count(), 2, "{kind}");

        // A batch reports each document; the conflicting doc1 is not ok.
        let results = db
            .bulk_docs(
                vec![new_doc("doc3"), new_doc("doc4"), new_doc("doc1")],
                BulkDocsOptions::new(),
            )
            .await
            .unwrap();
        assert_eq!(results[2].error.as_deref(), Some("conflict"), "{kind}");
        assert_eq!(counter.count(), 4, "{kind}");

        // Attachment writes and deletions are writes too.
        let r2 = db
            .put_attachment(
                "doc2",
                "a.txt",
                &r2.rev.unwrap(),
                b"a".to_vec(),
                "text/plain",
            )
            .await
            .unwrap();
        db.remove("doc2", &r2.rev.unwrap()).await.unwrap();
        assert_eq!(counter.count(), 6, "{kind}");
    }
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
    for kind in KINDS {
        let b =
            Backend::open(kind, "test").configure(|db| db.with_plugin(Arc::new(ValidationPlugin)));
        let db = &b.db;

        // Valid doc — has name
        let r1 = db
            .put("doc1", serde_json::json!({"name": "Alice"}))
            .await
            .unwrap();

        // Invalid doc — no name field
        let result = db.put("doc2", serde_json::json!({"age": 25})).await;
        assert!(
            matches!(&result, Err(RouchError::BadRequest(reason)) if reason == "name field is required"),
            "{kind}: {result:?}"
        );
        assert!(
            matches!(db.get("doc2").await, Err(RouchError::NotFound(_))),
            "{kind}"
        );
        // Nor can a valid document lose its name.
        let result = db
            .update("doc1", r1.rev.as_ref().unwrap(), serde_json::json!({}))
            .await;
        assert!(
            matches!(result, Err(RouchError::BadRequest(_))),
            "{kind}: {result:?}"
        );
        assert_eq!(db.get("doc1").await.unwrap().data["name"], "Alice");
        assert_eq!(db.info().await.unwrap().update_seq, Seq::Num(1), "{kind}");

        // A deletion carries no body and is allowed.
        db.remove("doc1", &r1.rev.unwrap()).await.unwrap();
        assert!(
            matches!(db.get("doc1").await, Err(RouchError::NotFound(_))),
            "{kind}"
        );
    }
}

// =========================================================================
// Partitioned databases
// =========================================================================

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
        "users:carol",
        serde_json::json!({"type": "user", "name": "Carol", "age": 40}),
    )
    .await
    .unwrap();
    db.put(
        "orders:o1",
        serde_json::json!({"type": "order", "amount": 100, "age": 50}),
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

    let ids: Vec<&str> = result
        .docs
        .iter()
        .map(|d| d["_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["users:alice", "users:carol"]);
    assert_eq!(result.docs[0]["name"], "Alice");
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
    assert_eq!(
        (doc.id.as_str(), &doc.data["team"]),
        ("team_a:doc1", &serde_json::json!("A"))
    );

    let team_b = db.partition("team_b");
    let doc = team_b.get("doc1").await.unwrap();
    assert_eq!(
        (doc.id.as_str(), &doc.data["team"]),
        ("team_b:doc1", &serde_json::json!("B"))
    );

    // Writing through one partition leaves the other untouched.
    let rev = doc.rev.unwrap().to_string();
    team_b
        .put("doc1", serde_json::json!({"team": "B", "_rev": rev}))
        .await
        .unwrap();
    assert_eq!(team_a.get("doc1").await.unwrap().rev.unwrap().pos, 1);
    assert_eq!(team_b.get("doc1").await.unwrap().rev.unwrap().pos, 2);
}
