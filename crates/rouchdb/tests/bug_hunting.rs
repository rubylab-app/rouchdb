//! Regression tests for bugs found while hunting through the `Database`
//! API. Each section states the behavior it pins; most run on the memory
//! and the redb backend.
//!
//! The partition, plugin, document JSON and error-variant tests that used
//! to live here are in partition.rs, plugin_contract.rs, parity_core.rs and
//! error_conditions.rs.

mod backends;

use std::collections::HashMap;
use std::time::Duration;

use backends::{Backend, KINDS, backends, row_ids};
use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, ChangesStreamOptions, Database,
    DesignDocument, Document, FindOptions, GetOptions, Revision, RouchError, Seq, SortField,
    ViewDef, ViewEngine, ViewQueryOptions, query_view,
};

// =========================================================================
// Purge removes a leaf revision for good and ignores inner revisions
// =========================================================================

#[tokio::test]
async fn purge_removes_a_leaf_and_ignores_inner_revisions() {
    for b in backends("test") {
        let db = &b.db;
        let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let rev1 = r1.rev.unwrap();
        let rev2 = db
            .update("doc1", &rev1, serde_json::json!({"v": 2}))
            .await
            .unwrap()
            .rev
            .unwrap();
        db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

        // Purging a non-leaf revision is ignored (CouchDB reports it with an
        // empty list); the document is untouched.
        let result = db.purge("doc1", vec![rev1]).await.unwrap();
        assert_eq!(result.purged["doc1"], Vec::<String>::new(), "{}", b.name);
        let doc = db.get("doc1").await.unwrap();
        assert_eq!(doc.rev.unwrap().to_string(), rev2, "{}", b.name);
        assert_eq!(doc.data, serde_json::json!({"v": 2}), "{}", b.name);

        // Purging the leaf erases the document: not readable, not listed,
        // and gone from the changes feed (so it never replicates).
        let result = db.purge("doc1", vec![rev2.clone()]).await.unwrap();
        assert_eq!(result.purged["doc1"], [rev2], "{}", b.name);
        assert!(
            matches!(db.get("doc1").await, Err(RouchError::NotFound(_))),
            "{}",
            b.name
        );
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(row_ids(&all), ["doc2"], "{}", b.name);
        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        let ids: Vec<&str> = changes.results.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["doc2"], "{}", b.name);
        let info = db.info().await.unwrap();
        assert_eq!((info.doc_count, info.doc_del_count), (1, 0), "{}", b.name);
    }
}

// =========================================================================
// Views: descending swaps the roles of start_key and end_key
// =========================================================================

#[tokio::test]
async fn view_descending_swaps_the_range_bounds() {
    for b in backends("test") {
        let db = &b.db;
        for i in 0..10 {
            db.put(&format!("doc{i}"), serde_json::json!({"n": i}))
                .await
                .unwrap();
        }
        let map_fn = |doc: &serde_json::Value| -> Vec<(serde_json::Value, serde_json::Value)> {
            vec![(doc["n"].clone(), serde_json::json!(1))]
        };
        let keys = |opts: ViewQueryOptions| async move {
            let result = query_view(db.adapter(), &map_fn, None, opts).await.unwrap();
            let rows: Vec<(i64, String)> = result
                .rows
                .iter()
                .map(|r| (r.key.as_i64().unwrap(), r.id.clone().unwrap()))
                .collect();
            rows
        };
        let desc = |start: Option<i64>, end: Option<i64>, inclusive_end| ViewQueryOptions {
            descending: true,
            start_key: start.map(serde_json::Value::from),
            end_key: end.map(serde_json::Value::from),
            inclusive_end,
            ..ViewQueryOptions::new()
        };
        let expect = |keys: &[i64]| -> Vec<(i64, String)> {
            keys.iter().map(|&k| (k, format!("doc{k}"))).collect()
        };

        // start_key is the upper bound, end_key the lower one.
        assert_eq!(
            keys(desc(Some(7), Some(3), true)).await,
            expect(&[7, 6, 5, 4, 3]),
            "{}",
            b.name
        );
        assert_eq!(
            keys(desc(Some(7), Some(3), false)).await,
            expect(&[7, 6, 5, 4]),
            "{}",
            b.name
        );
        assert_eq!(
            keys(desc(None, Some(7), true)).await,
            expect(&[9, 8, 7]),
            "{}",
            b.name
        );
        assert_eq!(
            keys(desc(Some(2), None, true)).await,
            expect(&[2, 1, 0]),
            "{}",
            b.name
        );
        assert_eq!(
            keys(desc(None, None, true)).await,
            expect(&[9, 8, 7, 6, 5, 4, 3, 2, 1, 0]),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// Changes feed: documents written after a live feed starts are streamed
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
    assert_eq!(event.seq, Seq::Num(1));

    handle.cancel();
}

// =========================================================================
// allDocs: paging past the end, limit 0, descending, ranges and keys
// =========================================================================

#[tokio::test]
async fn all_docs_paging_and_range_edge_cases() {
    for b in backends("test") {
        let db = &b.db;
        for id in ["a", "b", "c", "d"] {
            db.put(id, serde_json::json!({})).await.unwrap();
        }
        let query = |opts: AllDocsOptions| async move { db.all_docs(opts).await.unwrap() };

        let past_end = query(AllDocsOptions {
            skip: 100,
            ..AllDocsOptions::new()
        })
        .await;
        assert!(past_end.rows.is_empty(), "{}", b.name);
        assert_eq!(past_end.total_rows, 4, "{}", b.name);

        let none = query(AllDocsOptions {
            limit: Some(0),
            ..AllDocsOptions::new()
        })
        .await;
        assert!(none.rows.is_empty(), "{}", b.name);
        assert_eq!(none.total_rows, 4, "{}", b.name);

        let desc = query(AllDocsOptions {
            descending: true,
            ..AllDocsOptions::new()
        })
        .await;
        assert_eq!(row_ids(&desc), ["d", "c", "b", "a"], "{}", b.name);

        let range = query(AllDocsOptions {
            start_key: Some("b".into()),
            end_key: Some("c".into()),
            ..AllDocsOptions::new()
        })
        .await;
        assert_eq!(row_ids(&range), ["b", "c"], "{}", b.name);

        // Keys that do not exist are left out.
        let keys = query(AllDocsOptions {
            keys: Some(vec!["a".into(), "nonexistent".into(), "c".into()]),
            ..AllDocsOptions::new()
        })
        .await;
        assert_eq!(row_ids(&keys), ["a", "c"], "{}", b.name);
        let unknown = query(AllDocsOptions {
            keys: Some(vec!["fake1".into(), "fake2".into()]),
            ..AllDocsOptions::new()
        })
        .await;
        assert!(unknown.rows.is_empty(), "{}", b.name);
    }
}

// =========================================================================
// Design doc: every field survives a put/get round trip
// =========================================================================

#[tokio::test]
async fn design_doc_full_roundtrip() {
    for b in backends("test") {
        let db = &b.db;
        let named = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let ddoc = DesignDocument {
            id: "_design/full".into(),
            rev: None,
            views: HashMap::from([
                (
                    "by_type".to_string(),
                    ViewDef {
                        map: "function(doc) { emit(doc.type, 1); }".into(),
                        reduce: Some("_count".into()),
                    },
                ),
                (
                    "by_name".to_string(),
                    ViewDef {
                        map: "function(doc) { emit(doc.name, null); }".into(),
                        reduce: None,
                    },
                ),
            ]),
            filters: named(&[("my_filter", "function(doc) { return true; }")]),
            validate_doc_update: Some("function(n,o,u) {}".into()),
            shows: named(&[("detail", "function(doc,req) {}")]),
            lists: named(&[("all", "function(head,req) {}")]),
            updates: named(&[("bump", "function(doc,req) {}")]),
            language: Some("javascript".into()),
        };

        let result = db.put_design(ddoc.clone()).await.unwrap();
        assert!(result.ok, "{}: {result:?}", b.name);

        let retrieved = db.get_design("full").await.unwrap();
        assert_eq!(retrieved.rev, result.rev, "{}", b.name);
        let expected = DesignDocument {
            rev: result.rev.clone(),
            ..ddoc
        };
        assert_eq!(
            serde_json::to_value(&retrieved).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// Mango find: empty selector, sort, skip, limit and fields
// =========================================================================

#[tokio::test]
async fn find_sort_skip_limit_and_fields() {
    for b in backends("test") {
        let db = &b.db;
        for (id, doc) in [
            ("a", serde_json::json!({"name": "Zara", "age": 20})),
            (
                "b",
                serde_json::json!({"name": "Alice", "age": 30, "email": "alice@example.com"}),
            ),
            ("c", serde_json::json!({"name": "Bob", "age": 25})),
            ("d", serde_json::json!({"name": "Carol", "age": 35})),
            ("e", serde_json::json!({"name": "Eve"})),
        ] {
            db.put(id, doc).await.unwrap();
        }
        let find = |opts: FindOptions| async move {
            let docs = db.find(opts).await.unwrap().docs;
            docs.iter()
                .map(|d| d["_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let has_age = serde_json::json!({"age": {"$exists": true}});
        let by_name = Some(vec![SortField::Simple("name".into())]);

        // An empty selector matches every document, in id order.
        let all = find(FindOptions {
            selector: serde_json::json!({}),
            ..Default::default()
        })
        .await;
        assert_eq!(all, ["a", "b", "c", "d", "e"], "{}", b.name);

        let sorted = find(FindOptions {
            selector: has_age.clone(),
            sort: by_name.clone(),
            limit: Some(2),
            ..Default::default()
        })
        .await;
        assert_eq!(sorted, ["b", "c"], "{}", b.name);

        let paged = find(FindOptions {
            selector: has_age.clone(),
            sort: by_name,
            skip: Some(1),
            limit: Some(2),
            ..Default::default()
        })
        .await;
        assert_eq!(paged, ["c", "d"], "{}", b.name);

        let skipped = find(FindOptions {
            selector: has_age.clone(),
            skip: Some(3),
            ..Default::default()
        })
        .await;
        assert_eq!(skipped, ["d"], "{}", b.name);

        let descending = find(FindOptions {
            selector: has_age,
            sort: Some(vec![SortField::WithDirection(HashMap::from([(
                "age".to_string(),
                "desc".to_string(),
            )]))]),
            ..Default::default()
        })
        .await;
        assert_eq!(descending, ["d", "b", "c", "a"], "{}", b.name);

        // Only the requested fields are returned.
        let projected = db
            .find(FindOptions {
                selector: serde_json::json!({"name": "Alice"}),
                fields: Some(vec!["name".into(), "age".into()]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            projected.docs,
            [serde_json::json!({"name": "Alice", "age": 30})],
            "{}",
            b.name
        );
    }
}

// =========================================================================
// Replication: syncing again writes nothing and duplicates nothing
// =========================================================================

#[tokio::test]
async fn sync_is_idempotent() {
    for kind in KINDS {
        let a = Backend::open(kind, "a");
        let b = Backend::open(kind, "b");
        let (a, b) = (&a.db, &b.db);
        let r1 = a.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
        let r2 = b.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

        let (push, pull) = a.sync(b).await.unwrap();
        assert_eq!((push.docs_written, pull.docs_written), (1, 1), "{kind}");
        for _ in 0..2 {
            let (push, pull) = a.sync(b).await.unwrap();
            assert!(push.ok && pull.ok, "{kind}");
            assert_eq!((push.docs_written, pull.docs_written), (0, 0), "{kind}");
        }

        for db in [a, b] {
            let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
            assert_eq!(row_ids(&all), ["doc1", "doc2"], "{kind}");
            assert_eq!(all.rows[0].value.rev, *r1.rev.as_ref().unwrap(), "{kind}");
            assert_eq!(all.rows[1].value.rev, *r2.rev.as_ref().unwrap(), "{kind}");
            let info = db.info().await.unwrap();
            assert_eq!((info.doc_count, info.doc_del_count), (2, 0), "{kind}");
        }
    }
}

// =========================================================================
// ViewEngine: every pair a document emits is indexed, and replaced when
// the document changes
// =========================================================================

/// The (key, id) rows of the `app/by_tag` view.
async fn tag_rows(engine: &mut ViewEngine, db: &Database) -> Vec<(String, String)> {
    let result = engine
        .query(db.adapter(), "app", "by_tag", None, ViewQueryOptions::new())
        .await
        .unwrap();
    result
        .rows
        .iter()
        .map(|r| (r.key.as_str().unwrap().to_string(), r.id.clone().unwrap()))
        .collect()
}

#[tokio::test]
async fn view_engine_multiple_emits_per_doc() {
    let db = Database::memory("test");
    let r1 = db
        .put(
            "doc1",
            serde_json::json!({"tags": ["rust", "db", "local-first"]}),
        )
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"tags": ["db"]}))
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
    let pairs = |p: &[(&str, &str)]| -> Vec<(String, String)> {
        p.iter()
            .map(|(k, i)| (k.to_string(), i.to_string()))
            .collect()
    };

    assert_eq!(
        tag_rows(&mut engine, &db).await,
        pairs(&[
            ("db", "doc1"),
            ("db", "doc2"),
            ("local-first", "doc1"),
            ("rust", "doc1")
        ])
    );
    assert_eq!(
        engine.get_index("app", "by_tag").unwrap().entries["doc1"].len(),
        3
    );

    // Dropping a tag removes its row on the next update.
    db.update(
        "doc1",
        &r1.rev.unwrap(),
        serde_json::json!({"tags": ["rust"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        tag_rows(&mut engine, &db).await,
        pairs(&[("db", "doc2"), ("rust", "doc1")])
    );
}

// =========================================================================
// Conflicts: a replicated sibling revision makes a deterministic winner
// =========================================================================

#[tokio::test]
async fn create_conflict_via_bulk_docs() {
    for b in backends("test") {
        let db = &b.db;
        // Create initial doc
        let local_rev = db
            .put("doc1", serde_json::json!({"v": 1}))
            .await
            .unwrap()
            .rev
            .unwrap();

        // Force a conflicting revision via replication mode
        let conflict_rev = "1-conflicting_hash".to_string();
        let conflict_doc = Document {
            id: "doc1".into(),
            rev: Some(Revision::new(1, "conflicting_hash".into())),
            deleted: false,
            data: serde_json::json!({"v": "conflict"}),
            attachments: HashMap::new(),
        };

        let results = db
            .bulk_docs(vec![conflict_doc], BulkDocsOptions::replication())
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.ok), "{results:?}");

        // Both leaves are generation 1 and live, so the winner is the one
        // with the lexicographically greater hash (deterministic, like
        // CouchDB).
        let (winner, loser, winner_v) = if conflict_rev > local_rev {
            (&conflict_rev, &local_rev, serde_json::json!("conflict"))
        } else {
            (&local_rev, &conflict_rev, serde_json::json!(1))
        };

        let doc = db
            .get_with_opts(
                "doc1",
                GetOptions {
                    conflicts: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(&doc.rev.unwrap().to_string(), winner, "{}", b.name);
        assert_eq!(doc.data["v"], winner_v, "{}", b.name);
        assert_eq!(
            doc.data["_conflicts"],
            serde_json::json!([loser]),
            "{}",
            b.name
        );
    }
}

// =========================================================================
// Edge case: a large document is stored whole
// =========================================================================

#[tokio::test]
async fn put_large_document() {
    for b in backends("test") {
        let large_array: Vec<i64> = (0..10000).collect();
        let body = serde_json::json!({"data": large_array, "text": "x".repeat(100_000)});
        let result = b.db.put("large", body.clone()).await.unwrap();
        assert!(result.ok);

        let doc = b.db.get("large").await.unwrap();
        assert_eq!(doc.data, body, "{}", b.name);
    }
}

// =========================================================================
// Edge case: special characters in document IDs
// =========================================================================

#[tokio::test]
async fn special_characters_in_doc_id() {
    for b in backends("test") {
        let db = &b.db;
        let mut ids = vec![
            "doc with spaces",
            "doc/with/slashes",
            "doc-with-dashes",
            "doc_with_underscores",
            "123numeric",
            "UPPERCASE",
            "plus+sign",
            "question?mark",
            "percent%20",
            "hash#tag",
            "caf\u{e9}",
            "\u{1F600}",
        ];

        for id in &ids {
            let result = db.put(id, serde_json::json!({"id": id})).await.unwrap();
            assert_eq!(result.id, *id, "{}", b.name);

            let doc = db.get(id).await.unwrap();
            assert_eq!(doc.id, *id, "{}", b.name);
            assert_eq!(doc.data["id"], *id, "{}", b.name);
        }
        ids.sort();
        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert_eq!(row_ids(&all), ids, "{}", b.name);
    }
}

// =========================================================================
// Edge case: an empty database answers every query with nothing
// =========================================================================

#[tokio::test]
async fn operations_on_empty_db() {
    for b in backends("test") {
        let db = &b.db;
        let info = db.info().await.unwrap();
        assert_eq!(
            (info.doc_count, info.doc_del_count, info.update_seq),
            (0, 0, Seq::Num(0)),
            "{}",
            b.name
        );

        let all = db.all_docs(AllDocsOptions::new()).await.unwrap();
        assert!(all.rows.is_empty(), "{}", b.name);
        assert_eq!(all.total_rows, 0, "{}", b.name);

        let changes = db.changes(ChangesOptions::default()).await.unwrap();
        assert!(changes.results.is_empty(), "{}", b.name);
        assert_eq!(changes.last_seq, Seq::Num(0), "{}", b.name);

        let find = db
            .find(FindOptions {
                selector: serde_json::json!({}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(find.docs.is_empty(), "{}", b.name);

        assert!(
            matches!(db.get("anything").await, Err(RouchError::NotFound(_))),
            "{}",
            b.name
        );
    }
}
