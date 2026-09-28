//! Database operations (info, compact, destroy) and cross-adapter fidelity.

mod common;

use common::fresh_remote_db;
use rouchdb::Database;

#[tokio::test]
#[ignore]
async fn database_info_http() {
    let url = fresh_remote_db("db_info").await;
    let db = Database::http(&url);

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 0);

    db.put("doc1", serde_json::json!({})).await.unwrap();
    db.put("doc2", serde_json::json!({})).await.unwrap();

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 2);
}

#[tokio::test]
#[ignore]
async fn database_compact_http() {
    let url = fresh_remote_db("db_compact").await;
    let db = Database::http(&url);

    let r1 = db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();
    let r2 = db
        .update("doc1", &r1.rev.unwrap(), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert!(
        db.update("doc1", &r2.rev.unwrap(), serde_json::json!({"v": 3}))
            .await
            .unwrap()
            .ok
    );

    db.compact().await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let doc = db.get("doc1").await.unwrap();
    assert_eq!(doc.data["v"], 3);

    let info = db.info().await.unwrap();
    assert_eq!(info.doc_count, 1);
}

#[tokio::test]
#[ignore]
async fn database_destroy_http() {
    let url = fresh_remote_db("db_destroy").await;
    let db = Database::http(&url);

    db.put("doc1", serde_json::json!({"v": 1})).await.unwrap();

    db.destroy().await.unwrap();

    let result = db.info().await;
    assert!(result.is_err());
}

#[tokio::test]
#[ignore]
async fn cross_adapter_fidelity_memory_couchdb_redb() {
    let url = fresh_remote_db("fidelity").await;
    let memory = Database::memory("mem");
    let remote = Database::http(&url);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.redb");
    let redb = Database::open(&path, "redb").unwrap();

    let data = serde_json::json!({
        "string": "hello",
        "int": 42,
        "float": 3.15,
        "bool_t": true,
        "bool_f": false,
        "null_val": null,
        "array": [1, "two", null],
        "nested": {"a": {"b": {"c": "deep"}}},
        "empty_arr": [],
        "empty_obj": {}
    });

    memory.put("doc1", data.clone()).await.unwrap();
    memory.replicate_to(&remote).await.unwrap();
    redb.replicate_from(&remote).await.unwrap();

    let mem_doc = memory.get("doc1").await.unwrap();
    let remote_doc = remote.get("doc1").await.unwrap();
    let redb_doc = redb.get("doc1").await.unwrap();

    assert_eq!(mem_doc.data["string"], remote_doc.data["string"]);
    assert_eq!(remote_doc.data["string"], redb_doc.data["string"]);
    assert_eq!(mem_doc.data["int"], remote_doc.data["int"]);
    assert_eq!(remote_doc.data["int"], redb_doc.data["int"]);
    assert_eq!(mem_doc.data["float"], remote_doc.data["float"]);
    assert_eq!(remote_doc.data["float"], redb_doc.data["float"]);
    assert_eq!(mem_doc.data["null_val"], remote_doc.data["null_val"]);
    assert_eq!(remote_doc.data["null_val"], redb_doc.data["null_val"]);
    assert_eq!(mem_doc.data["array"], remote_doc.data["array"]);
    assert_eq!(remote_doc.data["array"], redb_doc.data["array"]);
    assert_eq!(mem_doc.data["nested"], remote_doc.data["nested"]);
    assert_eq!(remote_doc.data["nested"], redb_doc.data["nested"]);
}
