//! Tests for design documents and persistent views:
//! - DesignDocument CRUD (put, get, delete)
//! - ViewEngine with Rust map functions
//! - Incremental view index updates
//! - Multi-key view queries
//! - StaleOption
//! - view_cleanup()

use std::collections::HashMap;

use rouchdb::{
    Database, DesignDocument, ReduceFn, ViewDef, ViewEngine, ViewQueryOptions, query_view,
};

// =========================================================================
// Design document CRUD
// =========================================================================

#[tokio::test]
async fn put_and_get_design_document() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/myapp".into(),
        rev: None,
        views: {
            let mut views = HashMap::new();
            views.insert(
                "by_type".into(),
                ViewDef {
                    map: "function(doc) { emit(doc.type, 1); }".into(),
                    reduce: Some("_count".into()),
                },
            );
            views
        },
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: Some("javascript".into()),
    };

    let result = db.put_design(ddoc).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.id, "_design/myapp");

    // Retrieve by name
    let retrieved = db.get_design("myapp").await.unwrap();
    assert_eq!(retrieved.id, "_design/myapp");
    assert!(retrieved.views.contains_key("by_type"));
    assert_eq!(
        retrieved.views["by_type"].map,
        "function(doc) { emit(doc.type, 1); }"
    );
    assert_eq!(retrieved.views["by_type"].reduce, Some("_count".into()));
    assert_eq!(retrieved.language, Some("javascript".into()));
}

#[tokio::test]
async fn get_design_with_full_id() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/app".into(),
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

    // Retrieve using full _design/ prefix
    let retrieved = db.get_design("_design/app").await.unwrap();
    assert_eq!(retrieved.id, "_design/app");

    // Retrieve using short name
    let retrieved = db.get_design("app").await.unwrap();
    assert_eq!(retrieved.id, "_design/app");
}

#[tokio::test]
async fn delete_design_document() {
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

    let result = db.put_design(ddoc).await.unwrap();
    let rev = result.rev.unwrap();

    // Delete it
    let del_result = db.delete_design("myapp", &rev).await.unwrap();
    assert!(del_result.ok);

    // Should be gone
    let err = db.get_design("myapp").await;
    assert!(err.is_err());
}

#[tokio::test]
async fn update_design_document() {
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

    let r1 = db.put_design(ddoc).await.unwrap();
    let rev1 = r1.rev.unwrap();

    // Update the design doc with a new view
    let mut ddoc2 = db.get_design("myapp").await.unwrap();
    ddoc2.views.insert(
        "all".into(),
        ViewDef {
            map: "function(doc) { emit(doc._id, null); }".into(),
            reduce: None,
        },
    );
    ddoc2.rev = Some(rev1);

    let r2 = db.put_design(ddoc2).await.unwrap();
    assert!(r2.ok);

    let retrieved = db.get_design("myapp").await.unwrap();
    assert!(retrieved.views.contains_key("all"));
}

#[tokio::test]
async fn design_document_with_filters_and_validate() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/validation".into(),
        rev: None,
        views: HashMap::new(),
        filters: {
            let mut f = HashMap::new();
            f.insert(
                "by_type".into(),
                "function(doc, req) { return doc.type === req.query.type; }".into(),
            );
            f
        },
        validate_doc_update: Some(
            "function(newDoc, oldDoc, userCtx) { if (!newDoc.name) throw({forbidden: 'name required'}); }"
                .into(),
        ),
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };

    let result = db.put_design(ddoc).await.unwrap();
    assert!(result.ok);

    let retrieved = db.get_design("validation").await.unwrap();
    assert!(retrieved.filters.contains_key("by_type"));
    assert!(retrieved.validate_doc_update.is_some());
}

#[tokio::test]
async fn design_document_with_show_list_update() {
    let db = Database::memory("test");

    let ddoc = DesignDocument {
        id: "_design/app".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: {
            let mut s = HashMap::new();
            s.insert(
                "detail".into(),
                "function(doc, req) { return '<h1>' + doc.name + '</h1>'; }".into(),
            );
            s
        },
        lists: {
            let mut l = HashMap::new();
            l.insert("all".into(), "function(head, req) { /* list fn */ }".into());
            l
        },
        updates: {
            let mut u = HashMap::new();
            u.insert(
                "increment".into(),
                "function(doc, req) { doc.count++; return [doc, 'ok']; }".into(),
            );
            u
        },
        language: None,
    };

    let result = db.put_design(ddoc).await.unwrap();
    assert!(result.ok);

    let retrieved = db.get_design("app").await.unwrap();
    assert!(retrieved.shows.contains_key("detail"));
    assert!(retrieved.lists.contains_key("all"));
    assert!(retrieved.updates.contains_key("increment"));
}

// =========================================================================
// ViewEngine with Rust map functions
// =========================================================================

#[tokio::test]
async fn view_engine_register_and_query() {
    let db = Database::memory("test");

    db.put(
        "alice",
        serde_json::json!({"type": "user", "name": "Alice", "age": 30}),
    )
    .await
    .unwrap();
    db.put(
        "bob",
        serde_json::json!({"type": "user", "name": "Bob", "age": 25}),
    )
    .await
    .unwrap();
    db.put(
        "inv1",
        serde_json::json!({"type": "invoice", "amount": 100}),
    )
    .await
    .unwrap();

    let mut engine = ViewEngine::new();
    engine.register_map("myapp", "by_name", |doc| {
        if doc.get("type").and_then(|t| t.as_str()) == Some("user") {
            vec![(doc["name"].clone(), doc["age"].clone())]
        } else {
            vec![]
        }
    });

    engine
        .update_index(db.adapter(), "myapp", "by_name")
        .await
        .unwrap();

    let index = engine.get_index("myapp", "by_name").unwrap();
    assert_eq!(index.entries.len(), 2);

    // Should have entries for alice and bob
    assert!(index.entries.contains_key("alice"));
    assert!(index.entries.contains_key("bob"));
    assert!(!index.entries.contains_key("inv1"));
}

#[tokio::test]
async fn view_engine_incremental_update() {
    let db = Database::memory("test");

    db.put("doc1", serde_json::json!({"val": 10}))
        .await
        .unwrap();

    let mut engine = ViewEngine::new();
    engine.register_map("app", "all", |doc| {
        if let Some(val) = doc.get("val") {
            vec![(val.clone(), serde_json::json!(1))]
        } else {
            vec![]
        }
    });

    // First update
    engine
        .update_index(db.adapter(), "app", "all")
        .await
        .unwrap();
    assert_eq!(engine.get_index("app", "all").unwrap().entries.len(), 1);

    // Add more docs
    db.put("doc2", serde_json::json!({"val": 20}))
        .await
        .unwrap();
    db.put("doc3", serde_json::json!({"val": 30}))
        .await
        .unwrap();

    // Incremental update
    engine
        .update_index(db.adapter(), "app", "all")
        .await
        .unwrap();
    assert_eq!(engine.get_index("app", "all").unwrap().entries.len(), 3);
}

#[tokio::test]
async fn view_engine_handles_deleted_docs() {
    let db = Database::memory("test");

    let r1 = db
        .put("doc1", serde_json::json!({"val": 10}))
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"val": 20}))
        .await
        .unwrap();

    let mut engine = ViewEngine::new();
    engine.register_map("app", "all", |doc| {
        if let Some(val) = doc.get("val") {
            vec![(val.clone(), serde_json::json!(null))]
        } else {
            vec![]
        }
    });

    engine
        .update_index(db.adapter(), "app", "all")
        .await
        .unwrap();
    assert_eq!(engine.get_index("app", "all").unwrap().entries.len(), 2);

    // Delete doc1
    db.remove("doc1", &r1.rev.unwrap()).await.unwrap();

    engine
        .update_index(db.adapter(), "app", "all")
        .await
        .unwrap();
    assert_eq!(engine.get_index("app", "all").unwrap().entries.len(), 1);
    assert!(
        engine
            .get_index("app", "all")
            .unwrap()
            .entries
            .contains_key("doc2")
    );
}

#[tokio::test]
async fn view_engine_skips_design_docs() {
    let db = Database::memory("test");

    db.put("doc1", serde_json::json!({"val": 1})).await.unwrap();

    // Store a design doc
    let ddoc = DesignDocument {
        id: "_design/app".into(),
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

    let mut engine = ViewEngine::new();
    engine.register_map("app", "all", |doc| {
        vec![(
            doc.get("_id").cloned().unwrap_or(serde_json::json!(null)),
            serde_json::json!(null),
        )]
    });

    engine
        .update_index(db.adapter(), "app", "all")
        .await
        .unwrap();

    // Should NOT include the design doc
    let index = engine.get_index("app", "all").unwrap();
    assert!(!index.entries.keys().any(|k| k.starts_with("_design/")));
}

#[tokio::test]
async fn view_engine_unregistered_map_returns_error() {
    let db = Database::memory("test");
    let mut engine = ViewEngine::new();

    let result = engine.update_index(db.adapter(), "unknown", "view").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn view_engine_remove_indexes_not_in() {
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let mut engine = ViewEngine::new();
    engine.register_map("app", "v1", |_| vec![]);
    engine.register_map("app", "v2", |_| vec![]);
    engine.register_map("old", "stale", |_| vec![]);

    engine
        .update_index(db.adapter(), "app", "v1")
        .await
        .unwrap();
    engine
        .update_index(db.adapter(), "app", "v2")
        .await
        .unwrap();
    engine
        .update_index(db.adapter(), "old", "stale")
        .await
        .unwrap();

    assert_eq!(engine.index_names().len(), 3);

    // Keep only app views
    let valid: std::collections::HashSet<String> =
        vec!["app/v1".into(), "app/v2".into()].into_iter().collect();
    engine.remove_indexes_not_in(&valid);

    assert_eq!(engine.index_names().len(), 2);
    assert!(engine.get_index("app", "v1").is_some());
    assert!(engine.get_index("app", "v2").is_some());
    assert!(engine.get_index("old", "stale").is_none());
}

// =========================================================================
// view_cleanup()
// =========================================================================

#[tokio::test]
async fn view_cleanup_succeeds() {
    let db = Database::memory("test");
    // view_cleanup is a no-op in the base implementation, but should not error
    db.view_cleanup().await.unwrap();
}

// =========================================================================
// Map/reduce with multi-key query (keys option)
// =========================================================================

#[tokio::test]
async fn view_query_with_keys() {
    let db = Database::memory("test");

    db.put("a", serde_json::json!({"name": "Alice", "dept": "eng"}))
        .await
        .unwrap();
    db.put("b", serde_json::json!({"name": "Bob", "dept": "sales"}))
        .await
        .unwrap();
    db.put("c", serde_json::json!({"name": "Charlie", "dept": "eng"}))
        .await
        .unwrap();
    db.put("d", serde_json::json!({"name": "Diana", "dept": "hr"}))
        .await
        .unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["dept"].clone(), doc["name"].clone())]
    };

    // Query with specific keys
    let results = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            keys: Some(vec![serde_json::json!("eng"), serde_json::json!("hr")]),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    // Should only return eng and hr, not sales
    assert_eq!(results.rows.len(), 3); // Alice, Charlie (eng) + Diana (hr)
    assert!(results.rows.iter().all(|r| r.key == "eng" || r.key == "hr"));
}

#[tokio::test]
async fn view_query_with_single_key() {
    let db = Database::memory("test");

    db.put("a", serde_json::json!({"dept": "eng"}))
        .await
        .unwrap();
    db.put("b", serde_json::json!({"dept": "sales"}))
        .await
        .unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(doc["dept"].clone(), serde_json::json!(1))]
    };

    let results = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            key: Some(serde_json::json!("eng")),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 1);
    assert_eq!(results.rows[0].key, "eng");
}

// =========================================================================
// Map/reduce with reduce and group
// =========================================================================

#[tokio::test]
async fn view_reduce_stats() {
    let db = Database::memory("test");

    db.put("a", serde_json::json!({"score": 10})).await.unwrap();
    db.put("b", serde_json::json!({"score": 20})).await.unwrap();
    db.put("c", serde_json::json!({"score": 30})).await.unwrap();

    let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
        vec![(serde_json::json!("all"), doc["score"].clone())]
    };

    let results = query_view(
        db.adapter(),
        &map_fn,
        Some(&ReduceFn::Stats),
        ViewQueryOptions {
            reduce: true,
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(results.rows.len(), 1);
    let stats = &results.rows[0].value;
    assert_eq!(stats["count"], 3);
    assert_eq!(stats["sum"], 60.0);
    assert_eq!(stats["min"], 10.0);
    assert_eq!(stats["max"], 30.0);
}

// =========================================================================
// Regression tests for audited findings
// =========================================================================

fn engine_by_val() -> ViewEngine {
    let mut engine = ViewEngine::new();
    engine.register_map("app", "by_val", |doc| match doc.get("val") {
        Some(v) => vec![(v.clone(), serde_json::json!(1))],
        None => vec![],
    });
    engine
}

#[tokio::test]
async fn view_engine_reregistering_map_rebuilds_index() {
    // F51: a new map function must not be mixed with rows of the old one.
    let db = Database::memory("test");
    db.put("doc1", serde_json::json!({"val": 1, "other": "x"}))
        .await
        .unwrap();
    let mut engine = engine_by_val();
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();

    engine.register_map("app", "by_val", |doc| match doc.get("other") {
        Some(v) => vec![(v.clone(), serde_json::json!(2))],
        None => vec![],
    });
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();
    let index = engine.get_index("app", "by_val").unwrap();
    assert_eq!(
        index.entries["doc1"],
        vec![(serde_json::json!("x"), serde_json::json!(2))]
    );
}

#[tokio::test]
async fn view_engine_drops_purged_docs() {
    // F52: purged documents leave no change behind, but must leave the index.
    let db = Database::memory("test");
    let r1 = db.put("doc1", serde_json::json!({"val": 1})).await.unwrap();
    db.put("doc2", serde_json::json!({"val": 2})).await.unwrap();
    let mut engine = engine_by_val();
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();
    assert_eq!(engine.get_index("app", "by_val").unwrap().entries.len(), 2);

    db.purge("doc1", vec![r1.rev.unwrap()]).await.unwrap();
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();
    let index = engine.get_index("app", "by_val").unwrap();
    assert!(!index.entries.contains_key("doc1"));
    assert!(index.entries.contains_key("doc2"));
}

#[tokio::test]
async fn view_engine_resets_after_database_recreated() {
    // F52: a destroyed and recreated database starts its sequence again.
    let db = Database::memory("test");
    for i in 0..5 {
        db.put(&format!("old{i}"), serde_json::json!({"val": i}))
            .await
            .unwrap();
    }
    let mut engine = engine_by_val();
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();

    db.destroy().await.unwrap();
    db.put("new", serde_json::json!({"val": 42})).await.unwrap();
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();
    let index = engine.get_index("app", "by_val").unwrap();
    assert_eq!(index.entries.keys().collect::<Vec<_>>(), vec!["new"]);

    // Recreated again with more updates than the index has seen.
    db.destroy().await.unwrap();
    for i in 0..8 {
        db.put(&format!("n{i}"), serde_json::json!({"val": i}))
            .await
            .unwrap();
    }
    engine
        .update_index(db.adapter(), "app", "by_val")
        .await
        .unwrap();
    let index = engine.get_index("app", "by_val").unwrap();
    assert_eq!(index.entries.len(), 8);
    assert!(index.entries.keys().all(|k| k.starts_with('n')));
}

#[tokio::test]
async fn view_engine_query_uses_the_index() {
    // F56: ViewEngine can be queried with the same options as query_view.
    let db = Database::memory("test");
    for (id, val) in [("a", 3), ("b", 1), ("c", 2)] {
        db.put(id, serde_json::json!({"val": val})).await.unwrap();
    }
    let mut engine = engine_by_val();

    let result = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            None,
            ViewQueryOptions {
                start_key: Some(serde_json::json!(2)),
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.total_rows, 3);
    assert_eq!(result.offset, 1);
    let ids: Vec<_> = result.rows.iter().map(|r| r.id.clone().unwrap()).collect();
    assert_eq!(ids, ["c", "a"]);
    assert_eq!(result.rows[0].doc.as_ref().unwrap()["val"], 2);

    let result = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            Some(&ReduceFn::Sum),
            ViewQueryOptions::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0].value, serde_json::json!(3));
}

#[tokio::test]
async fn view_engine_query_honors_stale() {
    // F56: stale=ok serves the index as it is; update_after refreshes it
    // after answering; the default brings it up to date first.
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"val": 1})).await.unwrap();
    let mut engine = engine_by_val();
    let count = |r: &rouchdb::ViewResult| r.rows.len();

    let r = engine
        .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
        .await
        .unwrap();
    assert_eq!(count(&r), 1);

    db.put("b", serde_json::json!({"val": 2})).await.unwrap();
    let stale = |stale| ViewQueryOptions {
        stale,
        ..ViewQueryOptions::new()
    };
    let r = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            None,
            stale(rouchdb::StaleOption::Ok),
        )
        .await
        .unwrap();
    assert_eq!(count(&r), 1);
    let r = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            None,
            stale(rouchdb::StaleOption::UpdateAfter),
        )
        .await
        .unwrap();
    assert_eq!(count(&r), 1);
    let r = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            None,
            stale(rouchdb::StaleOption::Ok),
        )
        .await
        .unwrap();
    assert_eq!(count(&r), 2);

    db.put("c", serde_json::json!({"val": 3})).await.unwrap();
    let r = engine
        .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
        .await
        .unwrap();
    assert_eq!(count(&r), 3);
}

#[tokio::test]
async fn destroy_clears_mango_indexes() {
    // F56: destroying the database also drops its Mango indexes.
    let db = Database::memory("test");
    db.put("a", serde_json::json!({"age": 1})).await.unwrap();
    db.create_index(rouchdb::IndexDefinition {
        name: "by-age".into(),
        fields: vec![rouchdb::SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();
    db.destroy().await.unwrap();
    assert!(db.get_indexes().await.is_empty());
    let plan = db
        .explain(rouchdb::FindOptions {
            selector: serde_json::json!({"age": 1}),
            ..Default::default()
        })
        .await;
    assert_eq!(plan.index.name, "_all_docs");
}

#[tokio::test]
async fn design_document_roundtrip_keeps_unmodeled_fields() {
    // F55: get_design + put_design must not drop what DesignDocument does
    // not model (views.lib, Mango index views, options, custom fields).
    let db = Database::memory("test");
    let raw = serde_json::json!({
        "language": "javascript",
        "views": {
            "lib": {"util": "exports.x = 1;"},
            "by_type": {"map": "function(doc){ emit(doc.type); }", "options": {"collation": "raw"}},
            "mango-idx": {"map": {"fields": {"age": "asc"}}, "reduce": "_count", "options": {"def": {"fields": ["age"]}}}
        },
        "options": {"partitioned": false},
        "autoupdate": false,
        "custom": {"anything": [1, 2]}
    });
    db.put("_design/app", raw.clone()).await.unwrap();

    let mut ddoc = db.get_design("app").await.unwrap();
    ddoc.views.insert(
        "all".into(),
        ViewDef {
            map: "function(doc){ emit(doc._id); }".into(),
            reduce: None,
        },
    );
    db.put_design(ddoc).await.unwrap();

    let stored = db.get("_design/app").await.unwrap().data;
    assert_eq!(stored["views"]["lib"], raw["views"]["lib"]);
    assert_eq!(stored["views"]["mango-idx"], raw["views"]["mango-idx"]);
    assert_eq!(
        stored["views"]["by_type"]["options"],
        raw["views"]["by_type"]["options"]
    );
    assert!(stored["views"]["all"]["map"].is_string());
    assert_eq!(stored["options"], raw["options"]);
    assert_eq!(stored["autoupdate"], raw["autoupdate"]);
    assert_eq!(stored["custom"], raw["custom"]);

    // A view removed through the struct is really removed.
    let mut ddoc = db.get_design("app").await.unwrap();
    ddoc.views.remove("by_type");
    db.put_design(ddoc).await.unwrap();
    let stored = db.get("_design/app").await.unwrap().data;
    assert!(stored["views"].get("by_type").is_none());
    assert_eq!(stored["views"]["lib"], raw["views"]["lib"]);
}

struct DropEverything;

#[async_trait::async_trait]
impl rouchdb::Plugin for DropEverything {
    fn name(&self) -> &str {
        "drop-everything"
    }
    async fn before_write(&self, docs: &mut Vec<rouchdb::Document>) -> rouchdb::Result<()> {
        docs.clear();
        Ok(())
    }
}

#[tokio::test]
async fn put_design_does_not_panic_when_a_plugin_drops_it() {
    // F107: results.remove(0) panicked on an empty result list.
    let db = Database::memory("test").with_plugin(std::sync::Arc::new(DropEverything));
    let ddoc = DesignDocument {
        id: "_design/app".into(),
        rev: None,
        views: HashMap::new(),
        filters: HashMap::new(),
        validate_doc_update: None,
        shows: HashMap::new(),
        lists: HashMap::new(),
        updates: HashMap::new(),
        language: None,
    };
    assert!(db.put_design(ddoc).await.is_err());
}
