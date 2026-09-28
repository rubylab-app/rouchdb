/// Design documents and persistent views for RouchDB.
///
/// Provides `DesignDocument` types that model CouchDB design documents,
/// plus a `ViewEngine` for building and querying persistent views using
/// Rust closures as map/reduce functions.
mod design_doc;
mod engine;

pub use design_doc::{DesignDocument, ViewDef};
pub use engine::{PersistentViewIndex, ViewEngine};

#[cfg(test)]
mod tests {
    use super::*;
    use rouchdb_adapter_memory::MemoryAdapter;
    use rouchdb_core::adapter::Adapter;
    use rouchdb_core::document::{BulkDocsOptions, Document};
    use std::collections::HashMap;

    async fn setup_db() -> MemoryAdapter {
        let db = MemoryAdapter::new("test");
        let docs = vec![
            Document {
                id: "_design/myapp".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({
                    "views": {
                        "by_type": {
                            "map": "function(doc) { emit(doc.type, 1); }"
                        }
                    },
                    "filters": {
                        "users_only": "function(doc) { return doc.type === 'user'; }"
                    }
                }),
                attachments: HashMap::new(),
            },
            Document {
                id: "alice".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"type": "user", "name": "Alice"}),
                attachments: HashMap::new(),
            },
            Document {
                id: "bob".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"type": "user", "name": "Bob"}),
                attachments: HashMap::new(),
            },
            Document {
                id: "order1".into(),
                rev: None,
                deleted: false,
                data: serde_json::json!({"type": "order", "total": 50}),
                attachments: HashMap::new(),
            },
        ];
        db.bulk_docs(docs, BulkDocsOptions::new()).await.unwrap();
        db
    }

    #[tokio::test]
    async fn design_document_roundtrip() {
        let db = setup_db().await;
        let doc = db.get("_design/myapp", Default::default()).await.unwrap();
        let json = doc.to_json();
        let ddoc = DesignDocument::from_json(json).unwrap();
        assert_eq!(ddoc.id, "_design/myapp");
        assert_eq!(ddoc.name(), "myapp");
        assert!(ddoc.views.contains_key("by_type"));
        assert!(ddoc.filters.contains_key("users_only"));

        // Convert back to JSON
        let back = ddoc.to_json();
        assert_eq!(back["_id"], "_design/myapp");
        assert!(back["views"]["by_type"]["map"].is_string());
    }

    #[test]
    fn design_document_name_strips_the_prefix() {
        let ddoc = |id: &str| DesignDocument::from_json(serde_json::json!({"_id": id})).unwrap();
        assert_eq!(ddoc("_design/app").name(), "app");
        assert_eq!(ddoc("_design/a/b").name(), "a/b");
        assert_eq!(ddoc("app").name(), "app");
    }

    #[tokio::test]
    async fn view_engine_with_rust_map() {
        let db = setup_db().await;
        let mut engine = ViewEngine::new();

        // Register a Rust-based map function
        engine.register_map("myapp", "by_type", |doc| {
            let doc_type = doc.get("type").and_then(|v| v.as_str());
            if let Some(t) = doc_type {
                vec![(serde_json::json!(t), serde_json::json!(1))]
            } else {
                vec![]
            }
        });

        // The design document (which has no type anyway) is not mapped.
        let result = engine
            .query(
                &db,
                "myapp",
                "by_type",
                None,
                rouchdb_query::ViewQueryOptions::new(),
            )
            .await
            .unwrap();
        let rows: Vec<_> = result
            .rows
            .iter()
            .map(|r| (r.id.clone().unwrap(), r.key.clone()))
            .collect();
        assert_eq!(
            rows,
            [
                ("order1".to_string(), serde_json::json!("order")),
                ("alice".to_string(), serde_json::json!("user")),
                ("bob".to_string(), serde_json::json!("user")),
            ]
        );
        let counts = engine
            .query(
                &db,
                "myapp",
                "by_type",
                Some(&rouchdb_query::ReduceFn::Count),
                rouchdb_query::ViewQueryOptions {
                    group: true,
                    ..rouchdb_query::ViewQueryOptions::new()
                },
            )
            .await
            .unwrap();
        let counts: Vec<_> = counts
            .rows
            .iter()
            .map(|r| (r.key.clone(), r.value.clone()))
            .collect();
        assert_eq!(
            counts,
            [
                (serde_json::json!("order"), serde_json::json!(1)),
                (serde_json::json!("user"), serde_json::json!(2)),
            ]
        );
    }
}
