//! Tests for design documents and persistent views:
//! - DesignDocument CRUD (put, get, delete)
//! - ViewEngine: the incremental index compared with a full rebuild
//!   (`query_view`) after every kind of write, incrementality, stale
//! - view_cleanup()

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rouchdb::{
    BulkDocsOptions, Database, DesignDocument, Document, ReduceFn, RouchError, StaleOption,
    ViewDef, ViewEngine, ViewQueryOptions, ViewResult, query_view,
};
use serde_json::{Value, json};

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
    assert_eq!(retrieved.name(), "app");
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
    assert!(matches!(err, Err(RouchError::NotFound(_))));
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
// ViewEngine: the incremental index against a full rebuild
// =========================================================================

type Emitted = Vec<(Value, Value)>;

/// Emits `(val, 1)` for the documents that have a `val`.
fn by_val(doc: &Value) -> Emitted {
    match doc.get("val") {
        Some(v) => vec![(v.clone(), json!(1))],
        None => vec![],
    }
}

fn engine_by_val() -> ViewEngine {
    let mut engine = ViewEngine::new();
    engine.register_map("app", "by_val", by_val);
    engine
}

/// A view result as JSON (totals, ids, keys, values and docs), to compare
/// results exactly.
fn result_json(result: &ViewResult) -> Value {
    let rows: Vec<Value> = result
        .rows
        .iter()
        .map(|r| json!({"id": r.id, "key": r.key, "value": r.value, "doc": r.doc}))
        .collect();
    json!({"total_rows": result.total_rows, "offset": result.offset, "rows": rows})
}

/// The queries compared after every step.
fn probe_queries() -> Vec<(Option<ReduceFn>, ViewQueryOptions)> {
    vec![
        (None, ViewQueryOptions::new()),
        (
            None,
            ViewQueryOptions {
                include_docs: true,
                ..ViewQueryOptions::new()
            },
        ),
        (
            None,
            ViewQueryOptions {
                descending: true,
                skip: 1,
                limit: Some(3),
                ..ViewQueryOptions::new()
            },
        ),
        (
            None,
            ViewQueryOptions {
                start_key: Some(json!(2)),
                end_key: Some(json!("s")),
                inclusive_end: false,
                ..ViewQueryOptions::new()
            },
        ),
        (
            None,
            ViewQueryOptions {
                keys: Some(vec![json!(3), json!("s1"), json!(15)]),
                ..ViewQueryOptions::new()
            },
        ),
        (Some(ReduceFn::Count), ViewQueryOptions::new()),
        (
            Some(ReduceFn::Sum),
            ViewQueryOptions {
                group: true,
                ..ViewQueryOptions::new()
            },
        ),
        (
            Some(ReduceFn::Count),
            ViewQueryOptions {
                group_level: Some(1),
                descending: true,
                ..ViewQueryOptions::new()
            },
        ),
    ]
}

/// Every probe query answered from the engine's index must equal the
/// same query run from scratch over the current documents.
async fn assert_engine_matches_rebuild(engine: &mut ViewEngine, db: &Database, step: &str) {
    for (reduce, opts) in probe_queries() {
        let indexed = engine
            .query(db.adapter(), "app", "by_val", reduce.as_ref(), opts.clone())
            .await
            .unwrap();
        let rebuilt = query_view(db.adapter(), &by_val, reduce.as_ref(), opts.clone())
            .await
            .unwrap();
        assert_eq!(
            result_json(&indexed),
            result_json(&rebuilt),
            "{step}: {opts:?}"
        );
    }
}

async fn put(db: &Database, id: &str, body: Value) {
    db.put(id, body).await.unwrap();
}

async fn update(db: &Database, id: &str, body: Value) {
    let rev = db.get(id).await.unwrap().rev.unwrap().to_string();
    db.update(id, &rev, body).await.unwrap();
}

async fn remove(db: &Database, id: &str) {
    let rev = db.get(id).await.unwrap().rev.unwrap().to_string();
    db.remove(id, &rev).await.unwrap();
}

/// Write a revision as replication does (keeping `_rev` and `_revisions`).
async fn put_replicated(db: &Database, doc: Value) {
    let doc = Document::from_json(doc).unwrap();
    let result = db
        .bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
    assert!(result[0].ok, "{:?}", result[0]);
}

async fn run_engine_differential(db: &Database) {
    let mut engine = engine_by_val();
    assert_engine_matches_rebuild(&mut engine, db, "empty").await;

    put(db, "doc1", json!({"val": 10})).await;
    put(db, "doc2", json!({"val": 20})).await;
    assert_engine_matches_rebuild(&mut engine, db, "first documents").await;

    put(db, "doc3", json!({"val": 3})).await;
    assert_engine_matches_rebuild(&mut engine, db, "one more").await;

    update(db, "doc1", json!({"val": 15})).await;
    assert_engine_matches_rebuild(&mut engine, db, "doc1 updated").await;

    update(db, "doc2", json!({"other": 1})).await;
    assert_engine_matches_rebuild(&mut engine, db, "doc2 stops emitting").await;

    update(db, "doc2", json!({"val": "s1"})).await;
    assert_engine_matches_rebuild(&mut engine, db, "doc2 emits again").await;

    remove(db, "doc3").await;
    assert_engine_matches_rebuild(&mut engine, db, "doc3 deleted").await;

    put(db, "doc3", json!({"val": 3})).await;
    assert_engine_matches_rebuild(&mut engine, db, "doc3 recreated").await;

    update(db, "doc1", json!({"val": 4})).await;
    update(db, "doc1", json!({"val": 2})).await;
    assert_engine_matches_rebuild(&mut engine, db, "doc1 updated twice").await;

    // Conflicting revisions: the winner (highest hash) is mapped, and the
    // loser once the winner is deleted.
    let first = db
        .put("doc4", json!({"val": 1}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let first_hash = first.split_once('-').unwrap().1.to_string();
    for (hash, val) in [("a".repeat(32), 11), ("f".repeat(32), 12)] {
        put_replicated(
            db,
            json!({"_id": "doc4", "_rev": format!("2-{hash}"), "val": val,
                   "_revisions": {"start": 2, "ids": [hash, first_hash]}}),
        )
        .await;
    }
    assert_eq!(db.get("doc4").await.unwrap().data["val"], 12);
    assert_engine_matches_rebuild(&mut engine, db, "conflict").await;
    db.remove("doc4", &format!("2-{}", "f".repeat(32)))
        .await
        .unwrap();
    assert_eq!(db.get("doc4").await.unwrap().data["val"], 11);
    assert_engine_matches_rebuild(&mut engine, db, "winning revision deleted").await;

    put(db, "_design/app", json!({"val": 99})).await;
    assert_engine_matches_rebuild(&mut engine, db, "design document").await;

    for i in 0..15 {
        let val = match i % 5 {
            0 => json!(i % 4),
            1 => json!(format!("s{}", i % 3)),
            2 => json!([i % 2, "x"]),
            3 => json!(null),
            _ => json!({"k": i % 2}),
        };
        put(db, &format!("n{i:02}"), json!({"val": val})).await;
    }
    assert_engine_matches_rebuild(&mut engine, db, "mixed keys").await;

    let rev = db.get("doc1").await.unwrap().rev.unwrap().to_string();
    db.purge("doc1", vec![rev]).await.unwrap();
    assert_engine_matches_rebuild(&mut engine, db, "doc1 purged").await;
}

#[tokio::test]
async fn view_engine_matches_a_rebuild_after_every_write() {
    run_engine_differential(&Database::memory("engine")).await;

    let dir = tempfile::tempdir().unwrap();
    let redb = Database::open(dir.path().join("engine.redb"), "engine").unwrap();
    run_engine_differential(&redb).await;
}

#[tokio::test]
async fn view_engine_register_and_query() {
    let db = Database::memory("test");
    put(
        &db,
        "alice",
        json!({"type": "user", "name": "Alice", "age": 30}),
    )
    .await;
    put(
        &db,
        "bob",
        json!({"type": "user", "name": "Bob", "age": 25}),
    )
    .await;
    put(&db, "inv1", json!({"type": "invoice", "amount": 100})).await;
    let by_name = |doc: &Value| -> Emitted {
        if doc["type"] == "user" {
            vec![(doc["name"].clone(), doc["age"].clone())]
        } else {
            vec![]
        }
    };
    let mut engine = ViewEngine::new();
    engine.register_map("myapp", "by_name", by_name);

    let result = engine
        .query(
            db.adapter(),
            "myapp",
            "by_name",
            None,
            ViewQueryOptions::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        result_json(&result),
        json!({"total_rows": 2, "offset": 0, "rows": [
            {"id": "alice", "key": "Alice", "value": 30, "doc": null},
            {"id": "bob", "key": "Bob", "value": 25, "doc": null}
        ]})
    );
    let rebuilt = query_view(db.adapter(), &by_name, None, ViewQueryOptions::new())
        .await
        .unwrap();
    assert_eq!(result_json(&result), result_json(&rebuilt));
}

#[tokio::test]
async fn view_engine_maps_only_the_changed_documents() {
    // The index is updated from the changes feed: after the first build,
    // a query maps at most the changed documents plus the change the index
    // stopped at (re-read to notice a recreated database).
    let db = Database::memory("test");
    for i in 0..10 {
        put(&db, &format!("d{i}"), json!({"val": i})).await;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut engine = ViewEngine::new();
    let counter = Arc::clone(&calls);
    engine.register_map("app", "by_val", move |doc| {
        counter.fetch_add(1, Ordering::SeqCst);
        by_val(doc)
    });
    let mut query = async |step: &str, max_calls: usize| {
        calls.store(0, Ordering::SeqCst);
        engine
            .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
            .await
            .unwrap();
        let mapped = calls.load(Ordering::SeqCst);
        assert!(mapped <= max_calls, "{step}: {mapped} documents mapped");
    };

    query("first build", 10).await;
    query("nothing changed", 1).await;
    update(&db, "d3", json!({"val": 33})).await;
    query("d3 updated", 2).await;
    update(&db, "d3", json!({"val": 34})).await;
    query("d3 updated again", 1).await;
    update(&db, "d5", json!({"val": 55})).await;
    update(&db, "d6", json!({"val": 66})).await;
    query("two updates", 3).await;
    remove(&db, "d7").await;
    query("d7 deleted", 2).await;
}

#[tokio::test]
async fn view_engine_rebuilds_a_recreated_database_of_the_same_shape() {
    // After destroy, a database with as many documents and writes as
    // before has the same doc_count and update_seq; only the change the
    // index stopped at (another revision, or another document) tells.
    for new_ids in [["a", "b"], ["c", "d"]] {
        let db = Database::memory("test");
        put(&db, "a", json!({"val": 1})).await;
        put(&db, "b", json!({"val": 2})).await;
        let mut engine = engine_by_val();
        assert_engine_matches_rebuild(&mut engine, &db, "before destroy").await;
        let before = db.info().await.unwrap();

        db.destroy().await.unwrap();
        put(&db, new_ids[0], json!({"val": 10})).await;
        put(&db, new_ids[1], json!({"val": 20})).await;
        let after = db.info().await.unwrap();
        assert_eq!(
            (after.doc_count, after.update_seq),
            (before.doc_count, before.update_seq),
            "fixture: same shape"
        );
        assert_engine_matches_rebuild(&mut engine, &db, &format!("recreated as {new_ids:?}")).await;
    }
}

#[tokio::test]
async fn view_engine_resets_after_database_recreated() {
    // F52: a destroyed and recreated database starts its sequence again.
    let db = Database::memory("test");
    for i in 0..5 {
        put(&db, &format!("old{i}"), json!({"val": i})).await;
    }
    let mut engine = engine_by_val();
    assert_engine_matches_rebuild(&mut engine, &db, "old").await;

    db.destroy().await.unwrap();
    put(&db, "new", json!({"val": 42})).await;
    assert_engine_matches_rebuild(&mut engine, &db, "recreated with fewer writes").await;

    // Recreated again with more updates than the index has seen.
    db.destroy().await.unwrap();
    for i in 0..8 {
        put(&db, &format!("n{i}"), json!({"val": i})).await;
    }
    assert_engine_matches_rebuild(&mut engine, &db, "recreated with more writes").await;
}

#[tokio::test]
async fn view_engine_drops_purged_docs() {
    // F52: purged documents leave no change behind, but must leave the index.
    let db = Database::memory("test");
    let r1 = db.put("doc1", json!({"val": 1})).await.unwrap();
    put(&db, "doc2", json!({"val": 2})).await;
    let mut engine = engine_by_val();
    assert_engine_matches_rebuild(&mut engine, &db, "before purge").await;

    db.purge("doc1", vec![r1.rev.unwrap()]).await.unwrap();
    assert_engine_matches_rebuild(&mut engine, &db, "after purge").await;
    let result = engine
        .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id.as_deref(), Some("doc2"));
}

#[tokio::test]
async fn view_engine_reregistering_map_rebuilds_index() {
    // F51: a new map function must not be mixed with rows of the old one.
    let db = Database::memory("test");
    put(&db, "doc1", json!({"val": 1, "other": "x"})).await;
    let mut engine = engine_by_val();
    engine
        .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
        .await
        .unwrap();

    let by_other = |doc: &Value| -> Emitted {
        match doc.get("other") {
            Some(v) => vec![(v.clone(), json!(2))],
            None => vec![],
        }
    };
    engine.register_map("app", "by_val", by_other);
    let result = engine
        .query(db.adapter(), "app", "by_val", None, ViewQueryOptions::new())
        .await
        .unwrap();
    assert_eq!(
        result_json(&result)["rows"],
        json!([{"id": "doc1", "key": "x", "value": 2, "doc": null}])
    );
}

#[tokio::test]
async fn view_engine_unregistered_map_returns_error() {
    let db = Database::memory("test");
    let mut engine = ViewEngine::new();

    let result = engine.update_index(db.adapter(), "unknown", "view").await;
    assert!(matches!(result, Err(RouchError::BadRequest(_))));
    let result = engine
        .query(
            db.adapter(),
            "unknown",
            "view",
            None,
            ViewQueryOptions::new(),
        )
        .await;
    assert!(matches!(result, Err(RouchError::BadRequest(_))));
}

#[tokio::test]
async fn view_engine_remove_indexes_not_in() {
    let db = Database::memory("test");
    put(&db, "doc1", json!({"v": 1})).await;

    let mut engine = ViewEngine::new();
    engine.register_map("app", "v1", |_| vec![]);
    engine.register_map("app", "v2", |_| vec![]);
    engine.register_map("old", "stale", |_| vec![]);
    for (ddoc, view) in [("app", "v1"), ("app", "v2"), ("old", "stale")] {
        engine.update_index(db.adapter(), ddoc, view).await.unwrap();
    }
    let mut names = engine.index_names();
    names.sort();
    assert_eq!(names, ["app/v1", "app/v2", "old/stale"]);

    let valid: HashSet<String> = ["app/v1".to_string(), "app/v2".to_string()].into();
    engine.remove_indexes_not_in(&valid);

    let mut names = engine.index_names();
    names.sort();
    assert_eq!(names, ["app/v1", "app/v2"]);
    assert!(engine.get_index("old", "stale").is_none());
    // The map function is gone too.
    assert!(
        engine
            .update_index(db.adapter(), "old", "stale")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn view_cleanup_keeps_indexes_and_documents() {
    // view_cleanup is a no-op kept for PouchDB compatibility: Mango indexes
    // are removed with delete_index only.
    let db = Database::memory("test");
    put(&db, "a", json!({"age": 1})).await;
    db.create_index(rouchdb::IndexDefinition {
        name: "by-age".into(),
        fields: vec![rouchdb::SortField::Simple("age".into())],
        ddoc: None,
    })
    .await
    .unwrap();

    db.view_cleanup().await.unwrap();

    let names: Vec<_> = db.get_indexes().await.into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["by-age"]);
    assert_eq!(db.get("a").await.unwrap().data, json!({"age": 1}));
}

#[tokio::test]
async fn view_query_with_single_key() {
    let db = Database::memory("test");
    put(&db, "a", json!({"dept": "eng"})).await;
    put(&db, "b", json!({"dept": "sales"})).await;
    let map_fn = |doc: &Value| -> Emitted { vec![(doc["dept"].clone(), json!(1))] };

    let results = query_view(
        db.adapter(),
        &map_fn,
        None,
        ViewQueryOptions {
            key: Some(json!("eng")),
            ..ViewQueryOptions::new()
        },
    )
    .await
    .unwrap();

    assert_eq!(
        result_json(&results),
        json!({"total_rows": 2, "offset": 0, "rows": [
            {"id": "a", "key": "eng", "value": 1, "doc": null}
        ]})
    );
}

#[tokio::test]
async fn view_engine_query_uses_the_index() {
    // F56: ViewEngine can be queried with the same options as query_view.
    let db = Database::memory("test");
    for (id, val) in [("a", 3), ("b", 1), ("c", 2)] {
        put(&db, id, json!({"val": val})).await;
    }
    let mut engine = engine_by_val();

    let result = engine
        .query(
            db.adapter(),
            "app",
            "by_val",
            None,
            ViewQueryOptions {
                start_key: Some(json!(2)),
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
    assert_eq!(result.rows[0].value, json!(3));
}

#[tokio::test]
async fn view_engine_query_honors_stale() {
    // F56: stale=ok serves the index as it is; update_after refreshes it
    // after answering; the default brings it up to date first.
    let db = Database::memory("test");
    put(&db, "a", json!({"val": 1})).await;
    let mut engine = engine_by_val();
    let mut ids = async |stale| {
        let opts = ViewQueryOptions {
            stale,
            ..ViewQueryOptions::new()
        };
        let result = engine
            .query(db.adapter(), "app", "by_val", None, opts)
            .await
            .unwrap();
        result
            .rows
            .iter()
            .map(|r| r.id.clone().unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(ids(StaleOption::False).await, ["a"]);
    put(&db, "b", json!({"val": 2})).await;
    assert_eq!(ids(StaleOption::Ok).await, ["a"]);
    assert_eq!(ids(StaleOption::UpdateAfter).await, ["a"]);
    assert_eq!(ids(StaleOption::Ok).await, ["a", "b"]);
    put(&db, "c", json!({"val": 3})).await;
    assert_eq!(ids(StaleOption::False).await, ["a", "b", "c"]);
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
