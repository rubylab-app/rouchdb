//! CouchDB parity tests for replication, the changes feed and the HTTP
//! adapter, run against a real CouchDB (see `common`).

mod common;

use common::{delete_remote_db, fresh_remote_db};
use rouchdb::{BulkDocsOptions, Database, Document, GetOptions};

/// Write one revision with its full ancestry (`new_edits=false`).
async fn put_rev(db: &Database, id: &str, ids: &[&str], data: serde_json::Value) {
    let start = ids.len() as u64;
    let mut json = data;
    json["_id"] = serde_json::json!(id);
    json["_rev"] = serde_json::json!(format!("{}-{}", start, ids[0]));
    json["_revisions"] = serde_json::json!({"start": start, "ids": ids});
    let doc = Document::from_json(json).unwrap();
    db.adapter()
        .bulk_docs(vec![doc], BulkDocsOptions::replication())
        .await
        .unwrap();
}

async fn conflicts_of(db: &Database, id: &str) -> Vec<String> {
    let doc = db
        .get_with_opts(
            id,
            GetOptions {
                conflicts: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    doc.data["_conflicts"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

// =========================================================================
// Conflict branches (F12)
// =========================================================================

#[tokio::test]
#[ignore]
async fn pull_from_couchdb_brings_conflict_branches() {
    let url = fresh_remote_db("conflict_pull").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    put_rev(&remote, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
    put_rev(&remote, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;
    assert_eq!(conflicts_of(&remote, "d").await, vec!["2-bbb"]);

    let result = local.replicate_from(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(conflicts_of(&local, "d").await, vec!["2-bbb"]);

    delete_remote_db(&url).await;
}

#[tokio::test]
#[ignore]
async fn push_to_couchdb_carries_conflict_branches() {
    let url = fresh_remote_db("conflict_push").await;
    let remote = Database::http(&url);
    let local = Database::memory("local");

    put_rev(&local, "d", &["bbb", "aaa"], serde_json::json!({"v": "b"})).await;
    put_rev(&local, "d", &["ccc", "aaa"], serde_json::json!({"v": "c"})).await;

    let result = local.replicate_to(&remote).await.unwrap();
    assert!(result.ok, "{:?}", result.errors);
    assert_eq!(conflicts_of(&remote, "d").await, vec!["2-bbb"]);

    delete_remote_db(&url).await;
}
