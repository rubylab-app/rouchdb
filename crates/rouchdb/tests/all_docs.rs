//! all_docs advanced options against CouchDB: include_docs, key range,
//! descending, pagination, conflicts and update_seq.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, AllDocsResponse, BulkDocsOptions, Database, Document};

fn ids(result: &AllDocsResponse) -> Vec<&str> {
    result.rows.iter().map(|r| r.key.as_str()).collect()
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_include_docs() {
    let url = fresh_remote_db("ad_incdocs").await;
    let db = Database::http(&url);

    let r1 = db
        .put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    let r2 = db
        .put("doc2", serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(ids(&result), ["doc1", "doc2"]);
    for (row, (r, name)) in result.rows.iter().zip([(&r1, "Alice"), (&r2, "Bob")]) {
        let rev = r.rev.clone().unwrap();
        assert_eq!(row.rev(), Some(rev.as_str()));
        assert_eq!(
            row.doc,
            Some(serde_json::json!({"_id": r.id, "_rev": rev, "name": name}))
        );
    }
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_key_range() {
    let url = fresh_remote_db("ad_range").await;
    let db = Database::http(&url);

    for id in ["apple", "banana", "cherry", "date", "elderberry"] {
        db.put(id, serde_json::json!({})).await.unwrap();
    }

    let range = |start: &str, end: &str, inclusive_end| AllDocsOptions {
        start_key: Some(start.into()),
        end_key: Some(end.into()),
        inclusive_end,
        ..AllDocsOptions::new()
    };
    let result = db.all_docs(range("banana", "date", true)).await.unwrap();
    assert_eq!(ids(&result), ["banana", "cherry", "date"]);
    let result = db.all_docs(range("banana", "date", false)).await.unwrap();
    assert_eq!(ids(&result), ["banana", "cherry"]);
    let result = db.all_docs(range("b", "d", true)).await.unwrap();
    assert_eq!(ids(&result), ["banana", "cherry"]);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_descending() {
    let url = fresh_remote_db("ad_desc").await;
    let db = Database::http(&url);

    db.put("aaa", serde_json::json!({})).await.unwrap();
    db.put("bbb", serde_json::json!({})).await.unwrap();
    db.put("ccc", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            descending: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&result), ["ccc", "bbb", "aaa"]);

    // When descending, start_key is the upper bound.
    let result = db
        .all_docs(AllDocsOptions {
            descending: true,
            start_key: Some("bbb".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&result), ["bbb", "aaa"]);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_skip_and_limit() {
    let url = fresh_remote_db("ad_paging").await;
    let db = Database::http(&url);

    for c in ["a", "b", "c", "d", "e"] {
        db.put(c, serde_json::json!({})).await.unwrap();
    }

    let result = db
        .all_docs(AllDocsOptions {
            skip: 1,
            limit: Some(2),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(ids(&result), ["b", "c"]);
    assert_eq!(result.total_rows, 5);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_empty_database() {
    let url = fresh_remote_db("ad_empty").await;
    let db = Database::http(&url);

    let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(result.total_rows, 0);
    assert_eq!(result.rows.len(), 0);
}

/// The same checks as parity_core's local all_docs tests, against CouchDB:
/// `conflicts` lists the losing revisions of a real conflict and
/// `update_seq` is the database's sequence.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn all_docs_conflicts_and_update_seq() {
    let url = fresh_remote_db("ad_conflicts").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r1 = r1.rev.unwrap();
    let local = db
        .update("doc1", &r1, serde_json::json!({"v": "local"}))
        .await
        .unwrap()
        .rev
        .unwrap();
    let hash = "f".repeat(32);
    let winner = format!("2-{hash}");
    let doc = Document::from_json(serde_json::json!({
        "_id": "doc1",
        "_rev": winner,
        "v": "remote",
        "_revisions": {"start": 2, "ids": [hash, r1.split_once('-').unwrap().1]},
    }))
    .unwrap();
    db.bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"v": 2})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            conflicts: true,
            update_seq: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&result), ["doc1", "doc2"]);
    assert_eq!(result.rows[0].rev(), Some(winner.as_str()));
    let doc1 = result.rows[0].doc.as_ref().unwrap();
    assert_eq!(doc1["v"], "remote");
    assert_eq!(doc1["_conflicts"], serde_json::json!([local]));
    assert!(
        result.rows[1]
            .doc
            .as_ref()
            .unwrap()
            .get("_conflicts")
            .is_none()
    );
    let info = db.info().await.unwrap();
    assert_eq!(
        result.update_seq.as_ref().map(|s| s.as_num()),
        Some(info.update_seq.as_num())
    );

    let plain = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert!(plain.update_seq.is_none());
}

/// CouchDB's `offset` is the global position of the first returned row;
/// the local adapters report the `skip` instead (an accepted difference,
/// pinned by `accepted_divergence_all_docs_offset_is_the_skip` in
/// `adapter_conformance.rs`). For `keys`, CouchDB sends null (read as 0).
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn offset_is_the_global_position_on_couchdb() {
    let url = fresh_remote_db("ad_offset").await;
    let db = Database::http(&url);
    for id in ["a", "b", "c", "d", "e"] {
        db.put(id, serde_json::json!({})).await.unwrap();
    }
    let from_c = db
        .all_docs(AllDocsOptions {
            start_key: Some("c".into()),
            skip: 1,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(&from_c), ["d", "e"]);
    assert_eq!((from_c.total_rows, from_c.offset), (5, 3));
    let keys = db
        .all_docs(AllDocsOptions {
            keys: Some(vec!["a".into(), "b".into()]),
            skip: 1,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();
    assert_eq!((ids(&keys), keys.offset), (vec!["b"], 0));
}

/// Item 3 (F30): the rows of a `keys` query are the same on CouchDB, the
/// memory and the redb adapter: one per requested key, in order, with
/// deleted docs (`value.deleted`, no doc) and `not_found` error rows, and
/// skip/limit/descending applied to that list. The documents are written
/// with fixed revisions (replication mode) so the rows compare exactly.
#[tokio::test]
#[ignore = "requires CouchDB"]
async fn keys_rows_match_couchdb() {
    let url = fresh_remote_db("ad_keys_rows").await;
    let dir = tempfile::tempdir().unwrap();
    let backends = [
        ("couchdb", Database::http(&url)),
        ("memory", Database::memory("keys_rows")),
        (
            "redb",
            Database::open(dir.path().join("db.redb"), "keys_rows").unwrap(),
        ),
    ];
    let written = [
        serde_json::json!({"_id": "a", "_rev": "1-aaa", "v": 1}),
        serde_json::json!({"_id": "b", "_rev": "1-bbb", "v": 2}),
        serde_json::json!({"_id": "b", "_rev": "2-ddd", "_deleted": true,
            "_revisions": {"start": 2, "ids": ["ddd", "bbb"]}}),
        serde_json::json!({"_id": "c", "_rev": "1-ccc"}),
    ];
    let queries = [
        AllDocsOptions {
            keys: Some(vec!["a".into(), "b".into(), "zz".into(), "a".into()]),
            include_docs: true,
            ..AllDocsOptions::new()
        },
        AllDocsOptions {
            keys: Some(vec!["a".into(), "b".into(), "zz".into(), "c".into()]),
            descending: true,
            skip: 1,
            limit: Some(2),
            ..AllDocsOptions::new()
        },
        AllDocsOptions {
            keys: Some(vec![]),
            ..AllDocsOptions::new()
        },
    ];
    let mut results = Vec::new();
    for (name, db) in &backends {
        for json in &written {
            let doc = Document::from_json(json.clone()).unwrap();
            db.bulk_docs(vec![doc], BulkDocsOptions::replication())
                .await
                .unwrap();
        }
        let mut rows = Vec::new();
        for query in &queries {
            let res = db.all_docs(query.clone()).await.unwrap();
            assert_eq!(res.total_rows, 2, "{name}");
            rows.push(serde_json::to_value(&res.rows).unwrap());
        }
        results.push((name, rows));
    }
    assert_eq!(
        results[0].1[0],
        serde_json::json!([
            {"id": "a", "key": "a", "value": {"rev": "1-aaa"}, "doc": {"_id": "a", "_rev": "1-aaa", "v": 1}},
            {"id": "b", "key": "b", "value": {"rev": "2-ddd", "deleted": true}},
            {"key": "zz", "error": "not_found"},
            {"id": "a", "key": "a", "value": {"rev": "1-aaa"}, "doc": {"_id": "a", "_rev": "1-aaa", "v": 1}},
        ])
    );
    assert_eq!(
        results[0].1[1],
        serde_json::json!([
            {"key": "zz", "error": "not_found"},
            {"id": "b", "key": "b", "value": {"rev": "2-ddd", "deleted": true}},
        ])
    );
    for (name, rows) in &results[1..] {
        assert_eq!(rows, &results[0].1, "{name} differs from CouchDB");
    }
}
