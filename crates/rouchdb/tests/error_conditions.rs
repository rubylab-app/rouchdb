//! Error condition tests: nonexistent docs, wrong revisions, conflicts.

mod common;

use common::fresh_remote_db;
use rouchdb::{Database, RouchError};

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_get_nonexistent_doc() {
    let url = fresh_remote_db("err_noexist").await;
    let db = Database::http(&url);

    let result = db.get("does_not_exist").await;
    assert!(matches!(result, Err(RouchError::NotFound(_))));
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_update_wrong_rev() {
    let url = fresh_remote_db("err_wrongrev").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db
        .update("doc1", "1-bogusrevisionhash", serde_json::json!({"v": 2}))
        .await;
    // CouchDB rejects the write with a 409, surfaced as a conflict error.
    assert!(matches!(result, Err(RouchError::Conflict)), "{result:?}");
    // The stored document is left untouched.
    assert_eq!(db.get("doc1").await.unwrap().data["v"], 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_delete_wrong_rev() {
    let url = fresh_remote_db("err_delrev").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db.remove("doc1", "1-bogusrevisionhash").await;
    // CouchDB rejects the write with a 409, surfaced as a conflict error.
    assert!(matches!(result, Err(RouchError::Conflict)), "{result:?}");
    // The stored document is left untouched.
    assert_eq!(db.get("doc1").await.unwrap().data["v"], 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_put_existing_without_rev() {
    let url = fresh_remote_db("err_dup").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    let result = db.put("doc1", serde_json::json!({"v": 2})).await;
    // CouchDB rejects the write with a 409, surfaced as a conflict error.
    assert!(matches!(result, Err(RouchError::Conflict)), "{result:?}");
    // The stored document is left untouched.
    assert_eq!(db.get("doc1").await.unwrap().data["v"], 1);
}

#[tokio::test]
#[ignore = "requires CouchDB"]
async fn error_get_deleted_doc() {
    let url = fresh_remote_db("err_deleted").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    assert!(db.remove("doc1", &r1.rev.unwrap()).await.unwrap().ok);

    let result = db.get("doc1").await;
    assert!(matches!(result, Err(RouchError::NotFound(_))));
}
