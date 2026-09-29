//! all_docs advanced options against CouchDB: include_docs, key range,
//! descending, pagination, conflicts and update_seq.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, AllDocsResponse, BulkDocsOptions, Database, Document};

fn ids(result: &AllDocsResponse) -> Vec<&str> {
    result.rows.iter().map(|r| r.id.as_str()).collect()
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
        assert_eq!(row.value.rev, rev);
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
    assert_eq!(result.rows[0].value.rev, winner);
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
