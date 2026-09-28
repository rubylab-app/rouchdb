//! all_docs advanced options: include_docs, key range, descending, pagination.

mod common;

use common::fresh_remote_db;
use rouchdb::{AllDocsOptions, Database};

#[tokio::test]
#[ignore]
async fn all_docs_include_docs() {
    let url = fresh_remote_db("ad_incdocs").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"name": "Alice"}))
        .await
        .unwrap();
    db.put("doc2", serde_json::json!({"name": "Bob"}))
        .await
        .unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            include_docs: true,
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert!(result.rows[0].doc.is_some());
    let doc_json = result.rows[0].doc.as_ref().unwrap();
    assert!(doc_json.get("name").is_some());
}

#[tokio::test]
#[ignore]
async fn all_docs_key_range() {
    let url = fresh_remote_db("ad_range").await;
    let db = Database::http(&url);

    db.put("apple", serde_json::json!({})).await.unwrap();
    db.put("banana", serde_json::json!({})).await.unwrap();
    db.put("cherry", serde_json::json!({})).await.unwrap();
    db.put("date", serde_json::json!({})).await.unwrap();
    db.put("elderberry", serde_json::json!({})).await.unwrap();

    let result = db
        .all_docs(AllDocsOptions {
            start_key: Some("banana".into()),
            end_key: Some("date".into()),
            ..AllDocsOptions::new()
        })
        .await
        .unwrap();

    let ids: Vec<&str> = result.rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"banana"));
    assert!(ids.contains(&"cherry"));
    assert!(ids.contains(&"date"));
    assert!(!ids.contains(&"apple"));
    assert!(!ids.contains(&"elderberry"));
}

#[tokio::test]
#[ignore]
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

    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0].id, "ccc");
    assert_eq!(result.rows[1].id, "bbb");
    assert_eq!(result.rows[2].id, "aaa");
}

#[tokio::test]
#[ignore]
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

    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].id, "b");
    assert_eq!(result.rows[1].id, "c");
}

#[tokio::test]
#[ignore]
async fn all_docs_empty_database() {
    let url = fresh_remote_db("ad_empty").await;
    let db = Database::http(&url);

    let result = db.all_docs(AllDocsOptions::new()).await.unwrap();
    assert_eq!(result.total_rows, 0);
    assert_eq!(result.rows.len(), 0);
}
